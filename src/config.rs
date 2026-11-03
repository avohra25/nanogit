//! Minimal git-config reader/writer (`user.name`, `core.*`, ...).

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::repo::Repo;

/// Parsed key/value pairs; later entries override earlier ones.
#[derive(Clone, Debug, Default)]
pub struct Config {
    entries: Vec<(String, String)>,
}

fn unquote(raw: &str) -> String {
    let mut out = String::new();
    let mut in_quotes = false;
    let mut chars = raw.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            },
            '#' | ';' if !in_quotes => break,
            _ => out.push(c),
        }
    }
    if !in_quotes {
        out.truncate(out.trim_end().len());
    }
    out
}

/// Split a `[section "sub"]` header into `(section, subsection)`.
fn parse_header(line: &str) -> Option<(String, Option<String>)> {
    let inner = line.trim().strip_prefix('[')?.split(']').next()?;
    if let Some((name, rest)) = inner.split_once(char::is_whitespace) {
        let sub = rest.trim().trim_matches('"').replace("\\\"", "\"");
        return Some((name.to_ascii_lowercase(), Some(sub)));
    }
    match inner.split_once('.') {
        Some((name, sub)) => Some((name.to_ascii_lowercase(), Some(sub.to_string()))),
        None => Some((inner.to_ascii_lowercase(), None)),
    }
}

impl Config {
    /// Parse the text of a config file.
    pub fn parse(text: &str) -> Config {
        let mut entries = Vec::new();
        let mut section = String::new();
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
                continue;
            }
            if t.starts_with('[') {
                if let Some((name, sub)) = parse_header(t) {
                    section = match sub {
                        Some(s) => format!("{name}.{s}"),
                        None => name,
                    };
                }
                continue;
            }
            let (key, value) = match t.split_once('=') {
                Some((k, v)) => (k.trim(), unquote(v)),
                None => (t, "true".to_string()),
            };
            entries.push((format!("{section}.{}", key.to_ascii_lowercase()), value));
        }
        Config { entries }
    }

    /// Read a file; a missing file yields an empty config.
    pub fn from_file(path: &Path) -> Config {
        fs::read_to_string(path).map(|t| Config::parse(&t)).unwrap_or_default()
    }

    /// Global config (`$GIT_CONFIG_GLOBAL` or `~/.gitconfig`) then the repo's.
    pub fn load(repo: Option<&Repo>) -> Config {
        let mut cfg = Config::default();
        if let Some(path) = global_path() {
            cfg.entries.extend(Config::from_file(&path).entries);
        }
        if let Some(repo) = repo {
            cfg.entries.extend(Config::from_file(&repo.git_dir.join("config")).entries);
        }
        cfg
    }

    /// All `(key, value)` pairs in file order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Look a key up (`section.key` or `section.sub.key`); the last value wins.
    pub fn get(&self, key: &str) -> Option<&str> {
        let want = normalize_key(key);
        self.entries.iter().rev().find(|(k, _)| *k == want).map(|(_, v)| v.as_str())
    }
}

fn normalize_key(key: &str) -> String {
    match (key.find('.'), key.rfind('.')) {
        (Some(first), Some(last)) if first != last => format!(
            "{}.{}.{}",
            key[..first].to_ascii_lowercase(),
            &key[first + 1..last],
            key[last + 1..].to_ascii_lowercase()
        ),
        _ => key.to_ascii_lowercase(),
    }
}

fn global_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gitconfig"))
}

fn quote_value(v: &str) -> String {
    let needs = v.is_empty()
        || v.starts_with(char::is_whitespace)
        || v.ends_with(char::is_whitespace)
        || v.contains(['#', ';', '"', '\\', '\n']);
    if needs {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
    } else {
        v.to_string()
    }
}

/// Set `key` to `value` in the file at `path`, creating it if necessary.
pub fn set_value(path: &Path, key: &str, value: &str) -> Result<()> {
    let first = key.find('.');
    let last = key.rfind('.');
    let (Some(first), Some(last)) = (first, last) else {
        return Err(crate::error::Error::fatal(format!("key does not contain a section: {key}")));
    };
    let section = key[..first].to_ascii_lowercase();
    let sub = (first != last).then(|| key[first + 1..last].to_string());
    let name = key[last + 1..].to_ascii_lowercase();
    let new_line = format!("\t{name} = {}", quote_value(value));

    let text = fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut in_section = false;
    let mut section_end = None; // index after the last line of the section
    let mut replaced = false;
    for (i, line) in lines.iter_mut().enumerate() {
        let t = line.trim().to_string();
        if t.starts_with('[') {
            if in_section {
                break;
            }
            in_section = parse_header(&t).is_some_and(|(s, sb)| s == section && sb == sub);
            if in_section {
                section_end = Some(i + 1);
            }
        } else if in_section {
            let k = t.split('=').next().unwrap_or("").trim().to_ascii_lowercase();
            if k == name {
                *line = new_line.clone();
                replaced = true;
            }
            if !t.is_empty() {
                section_end = Some(i + 1);
            }
        }
    }
    if !replaced {
        match section_end {
            Some(at) => lines.insert(at, new_line),
            None => {
                lines.push(match &sub {
                    Some(s) => format!("[{section} \"{}\"]", s.replace('"', "\\\"")),
                    None => format!("[{section}]"),
                });
                lines.push(new_line);
            }
        }
    }
    fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# comment\n[core]\n\tbare = false\n[user]\n\tname = Ada Lovelace\n\temail = \"ada@example.com\" ; trailing\n[remote \"origin\"]\n\turl = https://example.com/x.git\n[Alias]\n\tCo = checkout\n\tflag\n";

    #[test]
    fn parses_sections_quotes_and_comments() {
        let c = Config::parse(SAMPLE);
        assert_eq!(c.get("user.name"), Some("Ada Lovelace"));
        assert_eq!(c.get("USER.Email"), Some("ada@example.com"));
        assert_eq!(c.get("remote.origin.url"), Some("https://example.com/x.git"));
        assert_eq!(c.get("alias.co"), Some("checkout"));
        assert_eq!(c.get("alias.flag"), Some("true"));
        assert_eq!(c.get("core.missing"), None);
    }

    #[test]
    fn last_value_wins() {
        let c = Config::parse("[a]\nb = 1\nb = 2\n");
        assert_eq!(c.get("a.b"), Some("2"));
    }

    #[test]
    fn set_value_creates_replaces_and_appends() {
        let path = std::env::temp_dir().join(format!("nanogit-config-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        set_value(&path, "user.name", "Grace Hopper").unwrap();
        set_value(&path, "user.email", "grace@example.com").unwrap();
        set_value(&path, "remote.origin.url", "git@example.com:g/x.git").unwrap();
        set_value(&path, "user.name", "Grace B. Hopper").unwrap();
        set_value(&path, "user.signoff", "has # hash").unwrap();
        let c = Config::from_file(&path);
        assert_eq!(c.get("user.name"), Some("Grace B. Hopper"));
        assert_eq!(c.get("user.email"), Some("grace@example.com"));
        assert_eq!(c.get("user.signoff"), Some("has # hash"));
        assert_eq!(c.get("remote.origin.url"), Some("git@example.com:g/x.git"));
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("[user]").count(), 1, "{text}");
        assert!(set_value(&path, "nosection", "x").is_err());
        fs::remove_file(&path).unwrap();
    }
}
