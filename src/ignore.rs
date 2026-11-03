//! `.gitignore` matching: globs (`*`, `?`, `[...]`, `**`), directory-only
//! patterns, anchoring, negation, nested ignore files and `info/exclude`.

use std::collections::HashMap;
use std::fs;

use crate::repo::Repo;

/// One parsed ignore pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    segments: Vec<String>,
    anchored: bool,
    dir_only: bool,
    negated: bool,
}

/// Parse the text of an ignore file.
pub fn parse_patterns(text: &str) -> Vec<Pattern> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut line = raw.trim_end_matches('\r');
        // Trailing spaces are dropped unless escaped with a backslash.
        while line.ends_with(' ') && !line.ends_with("\\ ") {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negated, mut body) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        if body.starts_with("\\!") || body.starts_with("\\#") {
            body = &body[1..];
        }
        let dir_only = body.ends_with('/');
        let body = body.trim_end_matches('/');
        if body.is_empty() {
            continue;
        }
        let anchored = body.contains('/');
        let body = body.strip_prefix('/').unwrap_or(body);
        let segments: Vec<String> = body.split('/').map(str::to_string).collect();
        out.push(Pattern { segments, anchored, dir_only, negated });
    }
    out
}

/// Match one path segment against one glob segment (no `/` involved).
pub fn glob_segment(pat: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        let mut matched = false;
        if p < pat.len() {
            match pat[p] {
                b'*' => {
                    star = Some((p, t));
                    p += 1;
                    continue;
                }
                b'?' => {
                    matched = true;
                    p += 1;
                }
                b'[' => {
                    if let Some((ok, next)) = match_class(pat, p, text[t]) {
                        matched = ok;
                        if ok {
                            p = next;
                        }
                    } else if text[t] == b'[' {
                        matched = true;
                        p += 1;
                    }
                }
                b'\\' if p + 1 < pat.len() => {
                    matched = pat[p + 1] == text[t];
                    if matched {
                        p += 2;
                    }
                }
                c => {
                    matched = c == text[t];
                    if matched {
                        p += 1;
                    }
                }
            }
        }
        if matched {
            t += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// Match a `[...]` class starting at `pat[start]`; returns `(matches, index after ']')`
/// or `None` when the class is unterminated.
fn match_class(pat: &[u8], start: usize, c: u8) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negate = matches!(pat.get(i), Some(b'!') | Some(b'^'));
    if negate {
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    loop {
        let &ch = pat.get(i)?;
        if ch == b']' && !first {
            return Some((hit != negate, i + 1));
        }
        first = false;
        let lo = if ch == b'\\' {
            i += 1;
            *pat.get(i)?
        } else {
            ch
        };
        if pat.get(i + 1) == Some(&b'-') && pat.get(i + 2).is_some_and(|&h| h != b']') {
            let hi = pat[i + 2];
            if lo <= c && c <= hi {
                hit = true;
            }
            i += 3;
        } else {
            if lo == c {
                hit = true;
            }
            i += 1;
        }
    }
}

fn match_segments(pat: &[String], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first == "**" => {
            if rest.is_empty() {
                return !path.is_empty();
            }
            (0..=path.len()).any(|skip| match_segments(rest, &path[skip..]))
        }
        Some((first, rest)) => match path.split_first() {
            Some((seg, tail)) => glob_segment(first.as_bytes(), seg.as_bytes()) && match_segments(rest, tail),
            None => false,
        },
    }
}

impl Pattern {
    /// Does this pattern select `rel` (path relative to the ignore file's
    /// directory)?  `is_dir` says whether the path is a directory.
    pub fn matches(&self, rel: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let parts: Vec<&str> = rel.split('/').collect();
        if self.anchored {
            match_segments(&self.segments, &parts)
        } else {
            parts.last().is_some_and(|name| glob_segment(self.segments[0].as_bytes(), name.as_bytes()))
        }
    }
}

/// Ignore rules for a work tree, loading `.gitignore` files lazily.
pub struct Ignore {
    repo: Repo,
    exclude: Vec<Pattern>,
    cache: HashMap<String, Vec<Pattern>>,
}

impl Ignore {
    /// Prepare matching for `repo` (reads `.git/info/exclude` now).
    pub fn new(repo: &Repo) -> Ignore {
        let exclude = fs::read_to_string(repo.git_dir.join("info/exclude"))
            .map(|t| parse_patterns(&t))
            .unwrap_or_default();
        Ignore { repo: repo.clone(), exclude, cache: HashMap::new() }
    }

    fn patterns_in(&mut self, dir: &str) -> &Vec<Pattern> {
        if !self.cache.contains_key(dir) {
            let file = if dir.is_empty() { ".gitignore".to_string() } else { format!("{dir}/.gitignore") };
            let pats = fs::read_to_string(self.repo.abs_path(&file)).map(|t| parse_patterns(&t)).unwrap_or_default();
            self.cache.insert(dir.to_string(), pats);
        }
        &self.cache[dir]
    }

    /// Decision from the rules alone: `Some(true)` ignored, `Some(false)`
    /// explicitly re-included, `None` no rule matched.  Ancestors are not consulted.
    fn decide(&mut self, rel: &str, is_dir: bool) -> Option<bool> {
        // Deeper ignore files take precedence, then shallower ones, then info/exclude.
        let mut dirs: Vec<&str> = vec![""];
        let mut end = 0;
        while let Some(i) = rel[end..].find('/') {
            dirs.push(&rel[..end + i]);
            end += i + 1;
        }
        for dir in dirs.into_iter().rev() {
            let sub = if dir.is_empty() { rel } else { &rel[dir.len() + 1..] };
            let verdict = self.patterns_in(dir).iter().rev().find(|p| p.matches(sub, is_dir)).map(|p| !p.negated);
            if verdict.is_some() {
                return verdict;
            }
        }
        self.exclude.iter().rev().find(|p| p.matches(rel, is_dir)).map(|p| !p.negated)
    }

    /// Is the (untracked) path ignored?  A path inside an ignored directory is
    /// ignored no matter what its own rules say.
    pub fn is_ignored(&mut self, rel: &str, is_dir: bool) -> bool {
        let mut end = 0;
        while let Some(i) = rel[end..].find('/') {
            let ancestor = &rel[..end + i];
            if self.decide(ancestor, true) == Some(true) {
                return true;
            }
            end += i + 1;
        }
        self.decide(rel, is_dir) == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(p: &str, t: &str) -> bool {
        glob_segment(p.as_bytes(), t.as_bytes())
    }

    fn check(rules: &str, cases: &[(&str, bool, bool)]) {
        let pats = parse_patterns(rules);
        for (path, is_dir, want) in cases {
            let got = pats.iter().rev().find(|p| p.matches(path, *is_dir)).map(|p| !p.negated).unwrap_or(false);
            assert_eq!(got, *want, "rules {rules:?} path {path:?} dir={is_dir}");
        }
    }

    #[test]
    fn globs() {
        assert!(g("*.rs", "main.rs"));
        assert!(!g("*.rs", "main.rc"));
        assert!(g("a?c", "abc"));
        assert!(!g("a?c", "ac"));
        assert!(g("*", ""));
        assert!(g("a*b*c", "aXXbYYc"));
        assert!(!g("a*b*c", "aXXbYY"));
        assert!(g("[abc]x", "bx"));
        assert!(!g("[!abc]x", "bx"));
        assert!(g("[^abc]x", "dx"));
        assert!(g("[a-f]1", "c1"));
        assert!(!g("[a-f]1", "g1"));
        assert!(g("\\*lit", "*lit"));
        assert!(!g("\\*lit", "xlit"));
        assert!(g("[]]x", "]x"));
        assert!(g("[", "["));
        assert!(g("*ab", "aab"));
        assert!(g("**", "anything"));
    }

    #[test]
    fn unanchored_patterns_match_any_depth() {
        check("*.log\nbuild\n", &[("a.log", false, true), ("x/y/z.log", false, true), ("src/build", true, true), ("a.txt", false, false)]);
    }

    #[test]
    fn directory_only_and_anchoring() {
        check("cache/\n", &[("cache", true, true), ("cache", false, false), ("x/cache", true, true)]);
        check("/root.txt\n", &[("root.txt", false, true), ("sub/root.txt", false, false)]);
        check("docs/*.md\n", &[("docs/a.md", false, true), ("docs/sub/a.md", false, false), ("x/docs/a.md", false, false)]);
        check("a/b\n", &[("a/b", false, true), ("b", false, false)]);
    }

    #[test]
    fn double_star() {
        check("**/tmp\n", &[("tmp", true, true), ("a/b/tmp", true, true)]);
        check("logs/**\n", &[("logs/a", false, true), ("logs/a/b", false, true), ("logs", true, false)]);
        check("a/**/z\n", &[("a/z", false, true), ("a/b/c/z", false, true), ("b/z", false, false)]);
    }

    #[test]
    fn negation_and_comments() {
        check("*.txt\n!keep.txt\n", &[("a.txt", false, true), ("keep.txt", false, false), ("d/keep.txt", false, false)]);
        check("# comment\n\n\\#hash\n\\!bang\n", &[("#hash", false, true), ("!bang", false, true), ("comment", false, false)]);
        check("trail\\ \nplain   \n", &[("trail ", false, true), ("plain", false, true)]);
    }

    fn repo(tag: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!("nanogit-ign-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Repo::init(&dir, "main").unwrap().0
    }

    #[test]
    fn nested_files_info_exclude_and_ancestors() {
        let repo = repo("nested");
        fs::write(repo.work_tree.join(".gitignore"), "*.log\n/target/\n!important.log\n").unwrap();
        fs::create_dir_all(repo.work_tree.join("sub")).unwrap();
        fs::write(repo.work_tree.join("sub/.gitignore"), "*.tmp\n!debug.log\n").unwrap();
        fs::write(repo.git_dir.join("info/exclude"), "secret.txt\n").unwrap();
        let mut ign = Ignore::new(&repo);
        assert!(ign.is_ignored("a.log", false));
        assert!(!ign.is_ignored("important.log", false));
        assert!(ign.is_ignored("target", true));
        assert!(ign.is_ignored("target/debug/x.rs", false), "inside an ignored directory");
        assert!(!ign.is_ignored("sub/debug.log", false), "deeper file overrides");
        assert!(ign.is_ignored("sub/other.log", false));
        assert!(ign.is_ignored("sub/x.tmp", false));
        assert!(!ign.is_ignored("x.tmp", false), "sub/.gitignore only applies below sub");
        assert!(ign.is_ignored("secret.txt", false));
        assert!(ign.is_ignored("sub/deep/secret.txt", false));
        assert!(!ign.is_ignored("src/main.rs", false));
        fs::remove_dir_all(&repo.work_tree).unwrap();
    }

    #[test]
    fn cannot_reinclude_inside_ignored_directory() {
        let repo = repo("reinc");
        fs::write(repo.work_tree.join(".gitignore"), "out/\n!out/keep.txt\n").unwrap();
        let mut ign = Ignore::new(&repo);
        assert!(ign.is_ignored("out/keep.txt", false));
        fs::remove_dir_all(&repo.work_tree).unwrap();
    }
}
