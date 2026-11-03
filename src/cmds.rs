//! Porcelain commands built on the library modules.

use std::fs;
use std::io::Write;

use crate::cli::{usage_error, Ctx};
use crate::commit::clean_message;
use crate::config::{set_value, Config};
use crate::error::{Error, Result};
use crate::history::log as walk_log;
use crate::ignore::Ignore;
use crate::object::{Commit, Oid};
use crate::refs::Head;
use crate::repo::Repo;
use crate::timefmt::format_git_date;
use crate::workdir::{pathspec_matches, relative_to};

/// Split `args` into options and operands, honouring a `--` separator.
fn split_dashdash(args: &[String]) -> (&[String], &[String]) {
    match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[]),
    }
}

fn is_option(arg: &str) -> bool {
    arg.starts_with('-') && arg.len() > 1
}

/// `nanogit add [-A | -u] [--] <path>...`
pub fn add(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "add [-A | -u] [-f] [--] <path>...";
    let (opts, after) = split_dashdash(args);
    let (mut all, mut update, mut force) = (false, false, false);
    let mut specs: Vec<String> = after.to_vec();
    for a in opts {
        match a.as_str() {
            "-A" | "--all" => all = true,
            "-u" | "--update" => update = true,
            "-f" | "--force" => force = true,
            s if is_option(s) => return usage_error(ctx, USAGE),
            s => specs.push(s.to_string()),
        }
    }
    if specs.is_empty() && !all && !update {
        writeln!(ctx.err, "Nothing specified, nothing added.")?;
        return Ok(0);
    }
    let repo = Repo::discover(ctx.cwd)?;
    let mut index = repo.load_index()?;
    let rels = resolve_specs(&repo, ctx, &specs, all || update)?;
    let mut refused = Vec::new();
    if update {
        repo.stage_tracked_changes(&mut index)?;
    } else if force {
        repo.add_paths(&mut index, &rels, None)?;
    } else {
        let mut ignore = Ignore::new(&repo);
        let mut wanted = Vec::new();
        for rel in rels {
            let meta = fs::symlink_metadata(repo.abs_path(&rel));
            let is_dir = meta.as_ref().is_ok_and(|m| m.is_dir());
            let tracked = if is_dir { index.contains_dir(&rel) } else { index.contains(&rel) };
            if !rel.is_empty() && meta.is_ok() && !tracked && ignore.is_ignored(&rel, is_dir) {
                refused.push(rel);
            } else {
                wanted.push(rel);
            }
        }
        let mut skip = |p: &str, is_dir: bool| ignore.is_ignored(p, is_dir);
        repo.add_paths(&mut index, &wanted, Some(&mut skip))?;
    }
    repo.save_index(&index)?;
    if !refused.is_empty() {
        writeln!(ctx.err, "The following paths are ignored by one of your .gitignore files:")?;
        for path in &refused {
            writeln!(ctx.err, "{path}")?;
        }
        writeln!(ctx.err, "hint: Use -f if you really want to add them.")?;
        return Ok(1);
    }
    Ok(0)
}

/// Turn user pathspecs into work-tree-relative ones; with `whole_tree`
/// and no specs, select the entire tree.
fn resolve_specs(repo: &Repo, ctx: &Ctx<'_>, specs: &[String], whole_tree: bool) -> Result<Vec<String>> {
    if specs.is_empty() && whole_tree {
        return Ok(vec![String::new()]);
    }
    specs.iter().map(|s| repo.rel_path(ctx.cwd, s)).collect()
}

/// `nanogit rm [-r] [-f] [--cached] [--] <path>...`
pub fn rm(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "rm [-r] [-f] [--cached] [--] <path>...";
    let (opts, after) = split_dashdash(args);
    let (mut recursive, mut force, mut cached) = (false, false, false);
    let mut specs: Vec<String> = after.to_vec();
    for a in opts {
        match a.as_str() {
            "-r" => recursive = true,
            "-f" | "--force" => force = true,
            "--cached" => cached = true,
            s if is_option(s) => return usage_error(ctx, USAGE),
            s => specs.push(s.to_string()),
        }
    }
    if specs.is_empty() {
        return usage_error(ctx, USAGE);
    }
    let repo = Repo::discover(ctx.cwd)?;
    let mut index = repo.load_index()?;
    let rels = resolve_specs(&repo, ctx, &specs, false)?;
    let removed = repo.remove_paths(&mut index, &rels, recursive, cached, force)?;
    repo.save_index(&index)?;
    for path in removed {
        writeln!(ctx.out, "rm '{path}'")?;
    }
    Ok(0)
}

/// `nanogit ls-files [-s] [-c] [-o] [-i] [--exclude-standard] [--] [<path>...]`
pub fn ls_files(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "ls-files [-s] [-c] [-o] [-i] [--exclude-standard] [--] [<path>...]";
    let (opts, after) = split_dashdash(args);
    let (mut staged, mut cached, mut others, mut ignored_only, mut exclude) = (false, false, false, false, false);
    let mut specs: Vec<String> = after.to_vec();
    for a in opts {
        match a.as_str() {
            "-s" | "--stage" => staged = true,
            "-c" | "--cached" => cached = true,
            "-o" | "--others" => others = true,
            "-i" | "--ignored" => ignored_only = true,
            "--exclude-standard" => exclude = true,
            s if is_option(s) => return usage_error(ctx, USAGE),
            s => specs.push(s.to_string()),
        }
    }
    if ignored_only && !others {
        return usage_error(ctx, USAGE);
    }
    let repo = Repo::discover(ctx.cwd)?;
    let index = repo.load_index()?;
    let prefix = repo.rel_path(ctx.cwd, ".")?;
    let rels = if specs.is_empty() { vec![prefix.clone()] } else { resolve_specs(&repo, ctx, &specs, false)? };
    if cached || staged || !others {
        let mut last: Option<String> = None;
        for e in &index.entries {
            if !rels.iter().any(|s| pathspec_matches(s, &e.path)) {
                continue;
            }
            let shown = relative_to(&prefix, &e.path);
            if staged {
                writeln!(ctx.out, "{:06o} {} {}\t{shown}", e.mode, e.oid, e.stage)?;
            } else if last.as_deref() != Some(shown.as_str()) {
                writeln!(ctx.out, "{shown}")?;
            }
            last = Some(shown);
        }
    }
    if others {
        let mut ignore = Ignore::new(&repo);
        let files = {
            let mut skip = |p: &str, is_dir: bool| !is_dir && index.contains(p);
            repo.walk_files("", &mut skip)?
        };
        for path in files {
            if !rels.iter().any(|s| pathspec_matches(s, &path)) {
                continue;
            }
            let hidden = ignore.is_ignored(&path, false);
            if hidden != ignored_only && (exclude || ignored_only) {
                continue;
            }
            writeln!(ctx.out, "{}", relative_to(&prefix, &path))?;
        }
    }
    Ok(0)
}

/// `nanogit check-ignore [-q] <path>...`: print the paths that are ignored.
pub fn check_ignore(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "check-ignore [-q] <path>...";
    let (opts, after) = split_dashdash(args);
    let mut quiet = false;
    let mut specs: Vec<String> = after.to_vec();
    for a in opts {
        match a.as_str() {
            "-q" | "--quiet" => quiet = true,
            s if is_option(s) => return usage_error(ctx, USAGE),
            s => specs.push(s.to_string()),
        }
    }
    if specs.is_empty() {
        return usage_error(ctx, USAGE);
    }
    let repo = Repo::discover(ctx.cwd)?;
    let index = repo.load_index()?;
    let mut ignore = Ignore::new(&repo);
    let mut any = false;
    for spec in &specs {
        let rel = repo.rel_path(ctx.cwd, spec)?;
        let is_dir = fs::symlink_metadata(repo.abs_path(&rel)).is_ok_and(|m| m.is_dir());
        if !index.contains(&rel) && ignore.is_ignored(&rel, is_dir) {
            any = true;
            if !quiet {
                writeln!(ctx.out, "{spec}")?;
            }
        }
    }
    Ok(if any { 0 } else { 1 })
}

/// `nanogit write-tree`
pub fn write_tree(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    if !args.is_empty() {
        return usage_error(ctx, "write-tree");
    }
    let repo = Repo::discover(ctx.cwd)?;
    let index = repo.load_index()?;
    let oid = repo.write_index_tree(&index)?;
    writeln!(ctx.out, "{oid}")?;
    Ok(0)
}

/// `nanogit commit [-a] [--allow-empty] -m <message>`
pub fn commit(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "commit [-a] [--allow-empty] -m <message>";
    let (mut all, mut allow_empty) = (false, false);
    let mut messages: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--all" => all = true,
            "--allow-empty" => allow_empty = true,
            "--message" => messages.push(it.next().ok_or_else(|| Error::fatal("option '--message' needs a value"))?.clone()),
            s if s.starts_with("--message=") => messages.push(s["--message=".len()..].to_string()),
            s if s.starts_with('-') && !s.starts_with("--") && s.len() > 1 => {
                // Clustered short flags such as -am "msg" or -m"msg".
                let mut chars = s[1..].char_indices();
                while let Some((i, c)) = chars.next() {
                    match c {
                        'a' => all = true,
                        'm' => {
                            let tail = &s[1 + i + 1..];
                            if tail.is_empty() {
                                messages.push(it.next().ok_or_else(|| Error::fatal("switch 'm' requires a value"))?.clone());
                            } else {
                                messages.push(tail.to_string());
                            }
                            break;
                        }
                        _ => return usage_error(ctx, USAGE),
                    }
                }
            }
            _ => return usage_error(ctx, USAGE),
        }
    }
    if messages.is_empty() {
        return Err(Error::fatal("a commit message is required (-m <message>)"));
    }
    let repo = Repo::discover(ctx.cwd)?;
    let mut index = repo.load_index()?;
    if all {
        repo.stage_tracked_changes(&mut index)?;
        repo.save_index(&index)?;
    }
    if index.has_conflicts() {
        let list = index.conflicted_paths().join("\n\t");
        return Err(Error::fatal(format!("committing is not possible because you have unmerged files:\n\t{list}")));
    }
    let message = messages.join("\n\n");
    if clean_message(&message).is_empty() {
        return Err(Error::fatal("aborting commit due to empty commit message"));
    }
    let tree = repo.write_index_tree(&index)?;
    let parent_tree = repo.head_tree()?;
    let unchanged = match parent_tree {
        Some(t) => t == tree,
        None => index.entries.is_empty(),
    };
    if unchanged && !allow_empty {
        writeln!(ctx.out, "nothing to commit, working tree clean")?;
        return Ok(1);
    }
    let root = repo.head_oid()?.is_none();
    let label = repo.head_label()?;
    let oid = repo.commit_index(&index, &message, &[])?;
    let subject = clean_message(&message).lines().next().unwrap_or_default().to_string();
    let root_note = if root { " (root-commit)" } else { "" };
    writeln!(ctx.out, "[{label}{root_note} {}] {subject}", oid.short(7))?;
    Ok(0)
}

/// Print one commit the way `git log` does (medium format).
pub fn write_commit_medium(out: &mut dyn Write, oid: &Oid, c: &Commit) -> Result<()> {
    writeln!(out, "commit {oid}")?;
    if c.parents.len() > 1 {
        let parents: Vec<String> = c.parents.iter().map(|p| p.short(7)).collect();
        writeln!(out, "Merge: {}", parents.join(" "))?;
    }
    writeln!(out, "Author: {} <{}>", c.author.name, c.author.email)?;
    writeln!(out, "Date:   {}", format_git_date(c.author.time, &c.author.tz))?;
    writeln!(out)?;
    for line in c.message.trim_end().lines() {
        // Like git, blank lines inside the message are indented too.
        writeln!(out, "    {line}")?;
    }
    Ok(())
}

/// `nanogit log [--oneline] [-n <N>] [<rev>...]`
pub fn log(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "log [--oneline] [--all] [-n <N>] [<rev>...]";
    let mut oneline = false;
    let mut all = false;
    let mut limit: Option<usize> = None;
    let mut revs: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let parse = |s: &str| -> Result<usize> {
            s.parse().map_err(|_| Error::fatal(format!("'{s}' is not a number")))
        };
        match a.as_str() {
            "--oneline" => oneline = true,
            "--all" => all = true,
            "-n" | "--max-count" => {
                limit = Some(parse(it.next().ok_or_else(|| Error::fatal("option '-n' needs a number"))?)?);
            }
            s if s.starts_with("--max-count=") => limit = Some(parse(&s["--max-count=".len()..])?),
            s if s.starts_with("-n") => limit = Some(parse(&s[2..])?),
            s if s.len() > 1 && s.starts_with('-') && s[1..].bytes().all(|b| b.is_ascii_digit()) => limit = Some(parse(&s[1..])?),
            s if is_option(s) => return usage_error(ctx, USAGE),
            s => revs.push(s.to_string()),
        }
    }
    let repo = Repo::discover(ctx.cwd)?;
    let starts: Vec<Oid> = if all {
        let mut starts: Vec<Oid> = repo.list_refs("refs/")?.into_iter().map(|(_, oid)| oid).collect();
        starts.extend(repo.head_oid()?);
        starts.extend(revs.iter().map(|r| repo.rev_parse(r)).collect::<Result<Vec<_>>>()?);
        starts
    } else if revs.is_empty() {
        match repo.head_oid()? {
            Some(oid) => vec![oid],
            None => {
                let branch = match repo.read_head()? {
                    Head::Branch(b) => b,
                    Head::Detached(_) => "HEAD".to_string(),
                };
                return Err(Error::fatal(format!("your current branch '{branch}' does not have any commits yet")));
            }
        }
    } else {
        revs.iter().map(|r| repo.rev_parse(r)).collect::<Result<_>>()?
    };
    let commits = walk_log(&repo.odb(), &starts, limit)?;
    for (n, (oid, c)) in commits.iter().enumerate() {
        if oneline {
            writeln!(ctx.out, "{} {}", oid.short(7), c.subject())?;
        } else {
            if n > 0 {
                writeln!(ctx.out)?;
            }
            write_commit_medium(ctx.out, oid, c)?;
        }
    }
    Ok(0)
}

/// `nanogit rev-parse [--short[=N]] [--show-toplevel] [--git-dir] <rev>...`
pub fn rev_parse(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    let repo = Repo::discover(ctx.cwd)?;
    let mut short: Option<usize> = None;
    for a in args {
        match a.as_str() {
            "--verify" => {}
            "--show-toplevel" => writeln!(ctx.out, "{}", repo.work_tree.display())?,
            "--git-dir" => writeln!(ctx.out, "{}", repo.git_dir.display())?,
            "--is-inside-work-tree" => writeln!(ctx.out, "true")?,
            "--short" => short = Some(7),
            s if s.starts_with("--short=") => {
                short = Some(s["--short=".len()..].parse().map_err(|_| Error::fatal("bad --short length"))?);
            }
            s if is_option(s) => return usage_error(ctx, "rev-parse [--short[=N]] [--show-toplevel] [--git-dir] <rev>..."),
            rev => {
                let oid = repo.rev_parse(rev)?;
                match short {
                    Some(n) => writeln!(ctx.out, "{}", oid.short(n.clamp(4, 40)))?,
                    None => writeln!(ctx.out, "{oid}")?,
                }
            }
        }
    }
    Ok(0)
}

/// `nanogit config [--list | [--get] <key> [<value>]]`
pub fn config(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "config [--list | [--get] <key> [<value>]]";
    let repo = Repo::discover(ctx.cwd)?;
    let operands: Vec<&String> = args.iter().filter(|a| a.as_str() != "--get").collect();
    match operands.as_slice() {
        [flag] if flag.as_str() == "--list" || flag.as_str() == "-l" => {
            let cfg = Config::load(Some(&repo));
            for (k, v) in cfg.entries() {
                writeln!(ctx.out, "{k}={v}")?;
            }
            Ok(0)
        }
        [key] if !is_option(key) => match Config::load(Some(&repo)).get(key) {
            Some(v) => {
                writeln!(ctx.out, "{v}")?;
                Ok(0)
            }
            None => Ok(1),
        },
        [key, value] if !is_option(key) && !args.iter().any(|a| a == "--get") => {
            set_value(&repo.git_dir.join("config"), key, value)?;
            Ok(0)
        }
        _ => usage_error(ctx, USAGE),
    }
}
