//! `status`: compare `HEAD`, the index and the working tree.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;

use crate::cli::{usage_error, Ctx};
use crate::error::{Error, Result};
use crate::ignore::Ignore;
use crate::index::{Index, IndexEntry};
use crate::object::{hash_object, mode, Kind};
use crate::refs::Head;
use crate::repo::Repo;
use crate::tree::Flat;
use crate::workdir::{pathspec_matches, relative_to};

/// How one path differs between two snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delta {
    /// Present only in the newer snapshot.
    Added,
    /// Content or permissions changed.
    Modified,
    /// Present only in the older snapshot.
    Deleted,
    /// Changed between regular file, symlink and submodule.
    TypeChange,
    /// Exact rename; carries the old path.
    Renamed(String),
}

impl Delta {
    fn long_label(&self) -> &'static str {
        match self {
            Delta::Added => "new file:   ",
            Delta::Modified => "modified:   ",
            Delta::Deleted => "deleted:    ",
            Delta::TypeChange => "typechange: ",
            Delta::Renamed(_) => "renamed:    ",
        }
    }

    fn short_code(&self) -> char {
        match self {
            Delta::Added => 'A',
            Delta::Modified => 'M',
            Delta::Deleted => 'D',
            Delta::TypeChange => 'T',
            Delta::Renamed(_) => 'R',
        }
    }
}

/// A conflicted path and which sides of the merge have it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unmerged {
    /// Path of the conflicted file.
    pub path: String,
    /// Two-letter short status code (`UU`, `AA`, `DU`, ...).
    pub code: &'static str,
    /// Long status description (`both modified`, ...).
    pub label: &'static str,
}

/// Everything `status` reports.
#[derive(Debug)]
pub struct Status {
    /// What `HEAD` points at.
    pub head: Head,
    /// True before the first commit.
    pub unborn: bool,
    /// Differences between `HEAD` and the index.
    pub staged: Vec<(String, Delta)>,
    /// Differences between the index and the working tree.
    pub unstaged: Vec<(String, Delta)>,
    /// Untracked, non-ignored paths (directories end in `/`).
    pub untracked: Vec<String>,
    /// Conflicted paths.
    pub unmerged: Vec<Unmerged>,
}

fn class(m: u32) -> u8 {
    match m {
        mode::SYMLINK => 1,
        mode::GITLINK => 2,
        _ => 0,
    }
}

/// Differences between two flat trees; with `renames`, a deleted and an added
/// file with identical content are reported as one rename.
pub fn tree_changes(old: &Flat, new: &Flat, renames: bool) -> Vec<(String, Delta)> {
    let paths: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    let mut out = Vec::new();
    for path in paths {
        let delta = match (old.get(path), new.get(path)) {
            (None, Some(_)) => Delta::Added,
            (Some(_), None) => Delta::Deleted,
            (Some(a), Some(b)) if a != b => {
                if class(a.mode) == class(b.mode) {
                    Delta::Modified
                } else {
                    Delta::TypeChange
                }
            }
            _ => continue,
        };
        out.push((path.clone(), delta));
    }
    if renames {
        let mut dead = HashSet::new();
        for i in 0..out.len() {
            if out[i].1 != Delta::Added {
                continue;
            }
            let Some(added) = new.get(&out[i].0) else { continue };
            let source = (0..out.len()).find(|&j| {
                out[j].1 == Delta::Deleted
                    && !dead.contains(&j)
                    && old.get(&out[j].0).is_some_and(|o| o.oid == added.oid && class(o.mode) == class(added.mode))
            });
            if let Some(j) = source {
                dead.insert(j);
                out[i].1 = Delta::Renamed(out[j].0.clone());
            }
        }
        let mut i = 0;
        out.retain(|_| {
            i += 1;
            !dead.contains(&(i - 1))
        });
    }
    out
}

impl Repo {
    /// How the working-tree file for a stage-0 index entry differs from it.
    pub fn entry_delta(&self, index: &Index, entry: &IndexEntry) -> Result<Option<Delta>> {
        let Some(wf) = self.read_work_file(&entry.path)? else {
            return Ok(Some(Delta::Deleted));
        };
        let racy = index.file_mtime.is_none_or(|m| entry.mtime >= m);
        if entry.stat_matches(&wf.meta) && !racy {
            return Ok(None);
        }
        if entry.mode == wf.mode && hash_object(Kind::Blob, &wf.data) == entry.oid {
            return Ok(None);
        }
        Ok(Some(if class(entry.mode) == class(wf.mode) { Delta::Modified } else { Delta::TypeChange }))
    }

    /// Tracked files whose working-tree state differs from the index
    /// (conflicted paths are skipped).
    pub fn worktree_changes(&self, index: &Index) -> Result<Vec<(String, Delta)>> {
        let mut out = Vec::new();
        for entry in index.entries.iter().filter(|e| e.stage == 0) {
            if let Some(delta) = self.entry_delta(index, entry)? {
                out.push((entry.path.clone(), delta));
            }
        }
        Ok(out)
    }

    /// Untracked, non-ignored paths below `dir`.  With `collapse`, a wholly
    /// untracked directory is reported once as `dir/`.
    pub fn untracked_paths(&self, index: &Index, ignore: &mut Ignore, dir: &str, collapse: bool) -> Result<Vec<String>> {
        let mut out = Vec::new();
        self.walk_untracked(index, ignore, dir, collapse, &mut out)?;
        Ok(out)
    }

    fn walk_untracked(
        &self,
        index: &Index,
        ignore: &mut Ignore,
        dir: &str,
        collapse: bool,
        out: &mut Vec<String>,
    ) -> Result<()> {
        let mut children = Vec::new();
        for entry in fs::read_dir(self.abs_path(dir))? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|n| Error::fatal(format!("{n:?} is not valid UTF-8")))?;
            if name == ".git" {
                continue;
            }
            let is_dir = entry.file_type()?.is_dir();
            children.push((name, is_dir));
        }
        children.sort_by(|a, b| {
            let key = |c: &(String, bool)| if c.1 { format!("{}/", c.0) } else { c.0.clone() };
            key(a).cmp(&key(b))
        });
        for (name, is_dir) in children {
            let rel = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            if is_dir {
                if index.contains_dir(&rel) {
                    self.walk_untracked(index, ignore, &rel, collapse, out)?;
                } else if ignore.is_ignored(&rel, true) {
                    continue;
                } else if collapse {
                    let mut inner = Vec::new();
                    self.walk_untracked(index, ignore, &rel, false, &mut inner)?;
                    if !inner.is_empty() {
                        out.push(format!("{rel}/"));
                    }
                } else {
                    self.walk_untracked(index, ignore, &rel, false, out)?;
                }
            } else if !index.contains(&rel) && !ignore.is_ignored(&rel, false) {
                out.push(rel);
            }
        }
        Ok(())
    }

    /// Gather the full status of the repository.
    pub fn status(&self, index: &Index) -> Result<Status> {
        let head = self.read_head()?;
        let unborn = self.head_oid()?.is_none();
        let unmerged = unmerged_paths(index);
        let skip: HashSet<&str> = unmerged.iter().map(|u| u.path.as_str()).collect();
        let staged = tree_changes(&self.head_flat()?, &Repo::index_flat(index), true)
            .into_iter()
            .filter(|(p, _)| !skip.contains(p.as_str()))
            .collect();
        let unstaged = self.worktree_changes(index)?;
        let mut ignore = Ignore::new(self);
        let untracked = self.untracked_paths(index, &mut ignore, "", true)?;
        Ok(Status { head, unborn, staged, unstaged, untracked, unmerged })
    }
}

/// Classify every conflicted path of `index`.
pub fn unmerged_paths(index: &Index) -> Vec<Unmerged> {
    let mut masks: BTreeMap<&str, u8> = BTreeMap::new();
    for e in index.entries.iter().filter(|e| e.stage > 0) {
        *masks.entry(e.path.as_str()).or_default() |= 1 << (e.stage - 1);
    }
    masks
        .into_iter()
        .map(|(path, mask)| {
            let (code, label) = match mask {
                0b111 => ("UU", "both modified"),
                0b110 => ("AA", "both added"),
                0b101 => ("DU", "deleted by us"),
                0b011 => ("UD", "deleted by them"),
                0b010 => ("AU", "added by us"),
                0b100 => ("UA", "added by them"),
                _ => ("DD", "both deleted"),
            };
            Unmerged { path: path.to_string(), code, label }
        })
        .collect()
}

fn keep(specs: &[String], path: &str) -> bool {
    specs.is_empty() || specs.iter().any(|s| pathspec_matches(s, path.trim_end_matches('/')))
}

/// Render the short (`-s`) format; `base` makes paths relative to a directory.
pub fn render_short(st: &Status, base: Option<&str>, branch: bool, specs: &[String]) -> String {
    let show = |p: &str| base.map_or_else(|| p.to_string(), |b| relative_to(b, p));
    let mut out = String::new();
    if branch {
        out.push_str(&match &st.head {
            Head::Branch(b) if st.unborn => format!("## No commits yet on {b}\n"),
            Head::Branch(b) => format!("## {b}\n"),
            Head::Detached(_) => "## HEAD (no branch)\n".to_string(),
        });
    }
    let mut rows: BTreeMap<String, (String, char, char)> = BTreeMap::new();
    for (path, delta) in &st.staged {
        let label = match delta {
            Delta::Renamed(old) => format!("{} -> {}", show(old), show(path)),
            _ => show(path),
        };
        rows.insert(path.clone(), (label, delta.short_code(), ' '));
    }
    for (path, delta) in &st.unstaged {
        rows.entry(path.clone()).or_insert_with(|| (show(path), ' ', ' ')).2 = delta.short_code();
    }
    let mut lines: BTreeMap<String, String> = BTreeMap::new();
    for (path, (label, x, y)) in rows {
        if keep(specs, &path) {
            lines.insert(path, format!("{x}{y} {label}\n"));
        }
    }
    for u in &st.unmerged {
        if keep(specs, &u.path) {
            lines.insert(u.path.clone(), format!("{} {}\n", u.code, show(&u.path)));
        }
    }
    for line in lines.values() {
        out.push_str(line);
    }
    for path in st.untracked.iter().filter(|p| keep(specs, p)) {
        out.push_str(&format!("?? {}\n", show(path)));
    }
    out
}

/// Render the long, human-oriented format.
pub fn render_long(st: &Status, base: &str, specs: &[String]) -> String {
    let show = |p: &str| relative_to(base, p);
    let mut out = String::new();
    match &st.head {
        Head::Branch(b) => out.push_str(&format!("On branch {b}\n")),
        Head::Detached(oid) => out.push_str(&format!("HEAD detached at {}\n", oid.short(7))),
    }
    if st.unborn {
        out.push_str("\nNo commits yet\n\n");
    }
    let staged: Vec<_> = st.staged.iter().filter(|(p, _)| keep(specs, p)).collect();
    let unstaged: Vec<_> = st.unstaged.iter().filter(|(p, _)| keep(specs, p)).collect();
    let unmerged: Vec<_> = st.unmerged.iter().filter(|u| keep(specs, &u.path)).collect();
    let untracked: Vec<_> = st.untracked.iter().filter(|p| keep(specs, p)).collect();
    if !unmerged.is_empty() {
        out.push_str("You have unmerged paths.\n  (fix conflicts and run \"nanogit commit\")\n");
        out.push_str("  (use \"nanogit merge --abort\" to abort the merge)\n\n");
    }
    if !staged.is_empty() {
        out.push_str("Changes to be committed:\n");
        if st.unborn {
            out.push_str("  (use \"nanogit rm --cached <file>...\" to unstage)\n");
        } else {
            out.push_str("  (use \"nanogit reset -- <file>...\" to unstage)\n");
        }
        for (path, delta) in &staged {
            let name = match delta {
                Delta::Renamed(old) => format!("{} -> {}", show(old), show(path)),
                _ => show(path),
            };
            out.push_str(&format!("\t{}{name}\n", delta.long_label()));
        }
        out.push('\n');
    }
    if !unmerged.is_empty() {
        out.push_str("Unmerged paths:\n  (use \"nanogit add <file>...\" to mark resolution)\n");
        for u in &unmerged {
            out.push_str(&format!("\t{:<17}{}\n", format!("{}:", u.label), show(&u.path)));
        }
        out.push('\n');
    }
    if !unstaged.is_empty() {
        out.push_str("Changes not staged for commit:\n  (use \"nanogit add/rm <file>...\" to update what will be committed)\n");
        out.push_str("  (use \"nanogit checkout -- <file>...\" to discard changes in working directory)\n");
        for (path, delta) in &unstaged {
            out.push_str(&format!("\t{}{}\n", delta.long_label(), show(path)));
        }
        out.push('\n');
    }
    if !untracked.is_empty() {
        out.push_str("Untracked files:\n  (use \"nanogit add <file>...\" to include in what will be committed)\n");
        for path in &untracked {
            out.push_str(&format!("\t{}\n", show(path)));
        }
        out.push('\n');
    }
    if !staged.is_empty() {
        // Ready to commit: git prints no summary line.
    } else if !unstaged.is_empty() || !unmerged.is_empty() {
        out.push_str("no changes added to commit (use \"nanogit add\" and/or \"nanogit commit -a\")\n");
    } else if !untracked.is_empty() {
        out.push_str("nothing added to commit but untracked files present (use \"nanogit add\" to track)\n");
    } else if st.unborn {
        out.push_str("nothing to commit (create/copy files and use \"nanogit add\" to track)\n");
    } else {
        out.push_str("nothing to commit, working tree clean\n");
    }
    out
}

/// `nanogit status [-s | --porcelain] [-b] [--] [<path>...]`
pub fn status(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "status [-s | --porcelain] [-b] [--] [<path>...]";
    let (mut short, mut porcelain, mut branch) = (false, false, false);
    let mut raw_specs = Vec::new();
    let mut only_paths = false;
    for a in args {
        match a.as_str() {
            _ if only_paths => raw_specs.push(a.clone()),
            "--" => only_paths = true,
            "-s" | "--short" => short = true,
            "--porcelain" => porcelain = true,
            "-b" | "--branch" => branch = true,
            "-sb" | "-bs" => {
                short = true;
                branch = true;
            }
            s if s.starts_with('-') && s.len() > 1 => return usage_error(ctx, USAGE),
            s => raw_specs.push(s.to_string()),
        }
    }
    let repo = Repo::discover(ctx.cwd)?;
    let specs = raw_specs.iter().map(|s| repo.rel_path(ctx.cwd, s)).collect::<Result<Vec<_>>>()?;
    let index = repo.load_index()?;
    let st = repo.status(&index)?;
    let base = repo.rel_path(ctx.cwd, ".")?;
    let text = if porcelain {
        render_short(&st, None, branch, &specs)
    } else if short {
        render_short(&st, Some(&base), branch, &specs)
    } else {
        render_long(&st, &base, &specs)
    };
    write!(ctx.out, "{text}")?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Oid;
    use crate::tree::FlatEntry;

    fn flat(items: &[(&str, u32, u8)]) -> Flat {
        items
            .iter()
            .map(|(p, m, b)| (p.to_string(), FlatEntry { mode: *m, oid: Oid::from_bytes([*b; 20]) }))
            .collect()
    }

    #[test]
    fn tree_changes_classify_paths() {
        let old = flat(&[("a", mode::FILE, 1), ("b", mode::FILE, 2), ("c", mode::FILE, 3), ("d", mode::FILE, 4)]);
        let new = flat(&[("a", mode::FILE, 1), ("b", mode::EXEC, 2), ("c", mode::SYMLINK, 3), ("e", mode::FILE, 5)]);
        let got = tree_changes(&old, &new, false);
        let names: Vec<_> = got.iter().map(|(p, d)| (p.as_str(), d.clone())).collect();
        assert_eq!(
            names,
            [("b", Delta::Modified), ("c", Delta::TypeChange), ("d", Delta::Deleted), ("e", Delta::Added)]
        );
    }

    #[test]
    fn exact_renames_are_paired() {
        let old = flat(&[("old.txt", mode::FILE, 7), ("keep", mode::FILE, 1)]);
        let new = flat(&[("new.txt", mode::FILE, 7), ("keep", mode::FILE, 1)]);
        assert_eq!(tree_changes(&old, &new, true), [("new.txt".to_string(), Delta::Renamed("old.txt".into()))]);
        assert_eq!(tree_changes(&old, &new, false).len(), 2);
    }

    #[test]
    fn short_format_merges_both_columns() {
        let st = Status {
            head: Head::Branch("main".into()),
            unborn: false,
            staged: vec![("a".into(), Delta::Modified), ("n".into(), Delta::Added)],
            unstaged: vec![("a".into(), Delta::Modified), ("z".into(), Delta::Deleted)],
            untracked: vec!["u".into(), "dir/".into()],
            unmerged: vec![Unmerged { path: "m".into(), code: "UU", label: "both modified" }],
        };
        assert_eq!(
            render_short(&st, None, true, &[]),
            "## main\nMM a\nUU m\nA  n\n D z\n?? u\n?? dir/\n"
        );
        assert_eq!(render_short(&st, Some("dir"), false, &["a".to_string()]), "MM ../a\n");
    }

    #[test]
    fn long_format_summaries() {
        let clean = Status {
            head: Head::Branch("main".into()),
            unborn: false,
            staged: vec![],
            unstaged: vec![],
            untracked: vec![],
            unmerged: vec![],
        };
        assert_eq!(render_long(&clean, "", &[]), "On branch main\nnothing to commit, working tree clean\n");
        let unborn = Status { unborn: true, ..clean };
        assert_eq!(
            render_long(&unborn, "", &[]),
            "On branch main\n\nNo commits yet\n\nnothing to commit (create/copy files and use \"nanogit add\" to track)\n"
        );
    }

    #[test]
    fn unmerged_classification() {
        let mut idx = Index::new();
        let oid = Oid::from_bytes([1; 20]);
        for (path, stage) in [("both", 1), ("both", 2), ("both", 3), ("ours", 2), ("theirs", 3), ("dbt", 1), ("dbt", 2), ("dbu", 1), ("dbu", 3)] {
            let mut e = crate::index::IndexEntry::new(path, mode::FILE, oid);
            e.stage = stage;
            idx.entries.push(e);
        }
        let codes: BTreeMap<_, _> = unmerged_paths(&idx).into_iter().map(|u| (u.path, u.code)).collect();
        assert_eq!(codes["both"], "UU");
        assert_eq!(codes["ours"], "AU");
        assert_eq!(codes["theirs"], "UA");
        assert_eq!(codes["dbt"], "UD");
        assert_eq!(codes["dbu"], "DU");
    }
}
