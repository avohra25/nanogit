//! Moving between commits: tree switching with clobber protection, plus the
//! `branch` and `checkout` commands.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::cli::{usage_error, Ctx};
use crate::error::{Error, Result};
use crate::history::{is_ancestor, peel_to_commit, read_commit};
use crate::ignore::Ignore;
use crate::index::{Index, IndexEntry};
use crate::object::{mode, Object, Oid};
use crate::refs::{valid_ref_name, Head};
use crate::repo::Repo;
use crate::tree::{flatten, Flat, FlatEntry};
use crate::workdir::{bytes_to_path, pathspec_matches};

/// A reason a tree switch was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocker {
    /// A tracked file with local changes would be overwritten or removed.
    Local(String),
    /// An untracked file is in the way of a file the switch must create.
    Untracked(String),
}

/// Format blockers as git does.  `action` is `checkout` or `merge`; `advice`
/// finishes "before you ..." (`switch branches` / `merge`).
pub fn describe_blockers(blockers: &[Blocker], action: &str, advice: &str) -> String {
    let mut out = String::new();
    let local: Vec<_> = blockers.iter().filter_map(|b| if let Blocker::Local(p) = b { Some(p) } else { None }).collect();
    let untracked: Vec<_> =
        blockers.iter().filter_map(|b| if let Blocker::Untracked(p) = b { Some(p) } else { None }).collect();
    if !local.is_empty() {
        out.push_str(&format!("error: Your local changes to the following files would be overwritten by {action}:\n"));
        for p in local {
            out.push_str(&format!("\t{p}\n"));
        }
        out.push_str(&format!("Please commit your changes or stash them before you {advice}.\n"));
    }
    if !untracked.is_empty() {
        out.push_str(&format!("error: The following untracked working tree files would be overwritten by {action}:\n"));
        for p in untracked {
            out.push_str(&format!("\t{p}\n"));
        }
        out.push_str(&format!("Please move or remove them before you {advice}.\n"));
    }
    out.push_str("Aborting\n");
    out
}

impl Repo {
    /// Create `rel` in the work tree with the given mode and contents,
    /// replacing whatever file is there.
    pub fn write_work_file(&self, rel: &str, file_mode: u32, data: &[u8]) -> Result<()> {
        let path = self.abs_path(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&path)?,
            Ok(_) => fs::remove_file(&path)?,
            Err(_) => {}
        }
        match file_mode {
            mode::SYMLINK => std::os::unix::fs::symlink(bytes_to_path(data), &path)?,
            mode::GITLINK => fs::create_dir_all(&path)?,
            _ => {
                let bits = if file_mode == mode::EXEC { 0o777 } else { 0o666 };
                let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(bits).open(&path)?;
                file.write_all(data)?;
                // Make the permission bits independent of any pre-existing file.
                let wanted = if file_mode == mode::EXEC { 0o755 } else { 0o644 };
                fs::set_permissions(&path, fs::Permissions::from_mode(wanted))?;
            }
        }
        Ok(())
    }

    pub(crate) fn blob_data(&self, entry: &FlatEntry) -> Result<Vec<u8>> {
        if entry.mode == mode::GITLINK {
            return Ok(Vec::new());
        }
        match self.odb().read(&entry.oid)? {
            Object::Blob(b) => Ok(b),
            _ => Err(Error::corrupt(format!("{} is not a blob", entry.oid))),
        }
    }

    /// Write `entry` to the work tree and record it (with fresh stat data) in the index.
    fn materialize(&self, index: &mut Index, path: &str, entry: &FlatEntry) -> Result<()> {
        let data = self.blob_data(entry)?;
        self.write_work_file(path, entry.mode, &data)?;
        match fs::symlink_metadata(self.abs_path(path)) {
            Ok(meta) if entry.mode != mode::GITLINK => {
                let mut e = IndexEntry::from_metadata(path, &meta, entry.oid);
                e.mode = entry.mode;
                index.add(e);
            }
            _ => index.add(IndexEntry::new(path, entry.mode, entry.oid)),
        }
        Ok(())
    }

    /// Move the index and work tree from the tree `from` (normally `HEAD`'s)
    /// to the tree `to`, like `git checkout`.  Local changes are never lost:
    /// when a file that would change has modifications, nothing is touched and
    /// the offending paths are returned.  With `force`, the index and tracked
    /// files are made to match `to` exactly.
    pub fn switch_tree(&self, index: &mut Index, from: &Flat, to: &Flat, force: bool) -> Result<Vec<Blocker>> {
        if force {
            let current = Repo::index_flat(index);
            let paths: BTreeSet<&String> = current.keys().chain(to.keys()).collect();
            let conflicted = index.conflicted_paths();
            for path in conflicted {
                index.remove(&path);
            }
            for path in paths {
                match to.get(path) {
                    None => {
                        self.remove_work_file(path)?;
                        index.remove(path);
                    }
                    Some(t) => {
                        let unchanged = current.get(path) == Some(t)
                            && index.get(path).is_some_and(|e| matches!(self.entry_delta(index, e), Ok(None)));
                        if !unchanged {
                            self.materialize(index, path, t)?;
                        }
                    }
                }
            }
            return Ok(Vec::new());
        }
        if index.has_conflicts() {
            return Err(Error::fatal("you need to resolve your current index first"));
        }
        let current = Repo::index_flat(index);
        let mut ignore = Ignore::new(self);
        let mut blockers = Vec::new();
        let mut plan: Vec<(&String, Option<&FlatEntry>)> = Vec::new();
        let paths: BTreeSet<&String> = from.keys().chain(to.keys()).chain(current.keys()).collect();
        for path in paths {
            let (h, t, i) = (from.get(path), to.get(path), current.get(path));
            if h == t || i == t {
                continue;
            }
            if i == h {
                match index.get(path) {
                    Some(entry) => {
                        let dirty = self.entry_delta(index, entry)?;
                        let missing = self.read_work_file(path)?.is_none();
                        if dirty.is_some() && !(t.is_none() && missing) {
                            blockers.push(Blocker::Local(path.clone()));
                            continue;
                        }
                    }
                    None => {
                        let in_the_way = fs::symlink_metadata(self.abs_path(path)).is_ok();
                        if in_the_way && !ignore.is_ignored(path, false) {
                            blockers.push(Blocker::Untracked(path.clone()));
                            continue;
                        }
                    }
                }
            } else {
                blockers.push(Blocker::Local(path.clone()));
                continue;
            }
            if t.is_some() {
                // A plain file sitting where a parent directory must be created.
                let mut end = 0;
                while let Some(pos) = path[end..].find('/') {
                    let ancestor = &path[..end + pos];
                    let blocked = fs::symlink_metadata(self.abs_path(ancestor)).is_ok_and(|m| !m.is_dir())
                        && !current.contains_key(ancestor);
                    if blocked {
                        blockers.push(Blocker::Untracked(ancestor.to_string()));
                        break;
                    }
                    end += pos + 1;
                }
            }
            plan.push((path, t));
        }
        if !blockers.is_empty() {
            blockers.dedup();
            return Ok(blockers);
        }
        for (path, t) in &plan {
            if t.is_none() {
                self.remove_work_file(path)?;
                index.remove(path);
            }
        }
        for (path, t) in &plan {
            if let Some(entry) = t {
                self.materialize(index, path, entry)?;
            }
        }
        Ok(Vec::new())
    }

    /// Flat listing of the tree of a commit-ish.
    pub fn commit_flat(&self, commit: &Oid) -> Result<Flat> {
        flatten(&self.odb(), &self.tree_of(commit)?)
    }
}

fn branch_exists(repo: &Repo, name: &str) -> Result<Option<Oid>> {
    repo.read_ref(&format!("refs/heads/{name}"))
}

fn check_branch_name(name: &str) -> Result<()> {
    if !valid_ref_name(&format!("refs/heads/{name}")) || name.starts_with('-') {
        return Err(Error::fatal(format!("'{name}' is not a valid branch name")));
    }
    Ok(())
}

/// `nanogit branch [-v] | <name> [<start>] | -d/-D <name>... | -m [<old>] <new> | --show-current`
pub fn branch(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "branch [-v] | <name> [<start>] | (-d | -D) <name>... | -m [<old>] <new> | --show-current";
    let (mut verbose, mut delete, mut force_delete, mut rename, mut show_current) = (false, false, false, false, false);
    let mut operands: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            "-v" | "--verbose" => verbose = true,
            "-d" | "--delete" => delete = true,
            "-D" => {
                delete = true;
                force_delete = true;
            }
            "-m" | "--move" => rename = true,
            "--show-current" => show_current = true,
            "--list" | "-l" => {}
            s if s.starts_with('-') && s.len() > 1 => return usage_error(ctx, USAGE),
            _ => operands.push(a),
        }
    }
    let repo = Repo::discover(ctx.cwd)?;
    let head = repo.read_head()?;
    if show_current {
        if let Head::Branch(b) = &head {
            writeln!(ctx.out, "{b}")?;
        }
        return Ok(0);
    }
    if delete {
        return delete_branches(&repo, &head, &operands, force_delete, ctx);
    }
    if rename {
        return rename_branch(&repo, &head, &operands, ctx);
    }
    if operands.is_empty() {
        let current = match &head {
            Head::Branch(b) => Some(b.as_str()),
            Head::Detached(_) => None,
        };
        let branches = repo.list_refs("refs/heads/")?;
        if let Head::Detached(oid) = &head {
            let subject = read_commit(&repo.odb(), oid)?.subject().to_string();
            if verbose {
                writeln!(ctx.out, "* (HEAD detached at {}) {} {subject}", oid.short(7), oid.short(7))?;
            } else {
                writeln!(ctx.out, "* (HEAD detached at {})", oid.short(7))?;
            }
        }
        let width = branches.iter().map(|(n, _)| n.len() - "refs/heads/".len()).max().unwrap_or(0);
        for (full, oid) in &branches {
            let name = &full["refs/heads/".len()..];
            let marker = if Some(name) == current { '*' } else { ' ' };
            if verbose {
                let subject = read_commit(&repo.odb(), oid)?.subject().to_string();
                writeln!(ctx.out, "{marker} {name:<width$} {} {subject}", oid.short(7))?;
            } else {
                writeln!(ctx.out, "{marker} {name}")?;
            }
        }
        return Ok(0);
    }
    if operands.len() > 2 {
        return usage_error(ctx, USAGE);
    }
    let name = operands[0];
    check_branch_name(name)?;
    if branch_exists(&repo, name)?.is_some() {
        return Err(Error::fatal(format!("a branch named '{name}' already exists")));
    }
    let start = match operands.get(1) {
        Some(rev) => repo.rev_parse(rev)?,
        None => repo.head_oid()?.ok_or_else(|| Error::fatal("not a valid object name: 'HEAD' (no commits yet)"))?,
    };
    let start = peel_to_commit(&repo.odb(), &start)?;
    repo.update_ref(&format!("refs/heads/{name}"), &start)?;
    Ok(0)
}

fn delete_branches(repo: &Repo, head: &Head, names: &[&String], force: bool, ctx: &mut Ctx<'_>) -> Result<i32> {
    if names.is_empty() {
        return usage_error(ctx, "branch (-d | -D) <name>...");
    }
    let mut status = 0;
    for name in names {
        let Some(oid) = branch_exists(repo, name)? else {
            writeln!(ctx.err, "error: branch '{name}' not found.")?;
            status = 1;
            continue;
        };
        if matches!(head, Head::Branch(b) if b == *name) {
            writeln!(ctx.err, "error: cannot delete branch '{name}' used by worktree at '{}'", repo.work_tree.display())?;
            status = 1;
            continue;
        }
        if !force {
            let merged = match repo.head_oid()? {
                Some(h) => is_ancestor(&repo.odb(), &oid, &h)?,
                None => false,
            };
            if !merged {
                writeln!(ctx.err, "error: the branch '{name}' is not fully merged.")?;
                writeln!(ctx.err, "If you are sure you want to delete it, run 'nanogit branch -D {name}'.")?;
                status = 1;
                continue;
            }
        }
        repo.delete_ref(&format!("refs/heads/{name}"))?;
        writeln!(ctx.out, "Deleted branch {name} (was {}).", oid.short(7))?;
    }
    Ok(status)
}

fn rename_branch(repo: &Repo, head: &Head, names: &[&String], ctx: &mut Ctx<'_>) -> Result<i32> {
    let (old, new) = match names {
        [new] => match head {
            Head::Branch(b) => (b.clone(), (*new).clone()),
            Head::Detached(_) => return Err(Error::fatal("cannot rename the current branch while not on any")),
        },
        [old, new] => ((*old).clone(), (*new).clone()),
        _ => return usage_error(ctx, "branch -m [<old>] <new>"),
    };
    check_branch_name(&new)?;
    let unborn_current = matches!(head, Head::Branch(b) if *b == old);
    match branch_exists(repo, &old)? {
        Some(oid) => {
            if old != new && branch_exists(repo, &new)?.is_some() {
                return Err(Error::fatal(format!("a branch named '{new}' already exists")));
            }
            if old != new {
                repo.update_ref(&format!("refs/heads/{new}"), &oid)?;
                repo.delete_ref(&format!("refs/heads/{old}"))?;
            }
        }
        None if unborn_current => {}
        None => return Err(Error::fatal(format!("no branch named '{old}'"))),
    }
    if unborn_current {
        repo.set_head_branch(&new)?;
    }
    Ok(0)
}

/// Report where `HEAD` ended up (stderr, like git).
fn announce(ctx: &mut Ctx<'_>, line: &str) -> Result<()> {
    writeln!(ctx.err, "{line}")?;
    Ok(())
}

/// `nanogit checkout [-f] [-b <new>] [--detach] <branch | commit>` or `checkout [<commit>] -- <path>...`
pub fn checkout(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "checkout [-f] [-b <new-branch>] [--detach] [<branch> | <commit>]  |  checkout [<commit>] [--] <path>...";
    let (mut force, mut detach) = (false, false);
    let mut new_branch: Option<String> = None;
    let mut before: Vec<&String> = Vec::new();
    let mut after: Option<Vec<&String>> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(paths) = after.as_mut() {
            paths.push(a);
            continue;
        }
        match a.as_str() {
            "--" => after = Some(Vec::new()),
            "-f" | "--force" => force = true,
            "--detach" | "-d" => detach = true,
            "-b" => new_branch = Some(it.next().ok_or_else(|| Error::fatal("switch 'b' requires a value"))?.clone()),
            s if s.starts_with('-') && s.len() > 1 => return usage_error(ctx, USAGE),
            _ => before.push(a),
        }
    }
    let repo = Repo::discover(ctx.cwd)?;
    let mut index = repo.load_index()?;

    // Path mode: restore files from the index or a commit.
    if let Some(paths) = &after {
        let rev = match before.as_slice() {
            [] => None,
            [rev] => Some(*rev),
            _ => return usage_error(ctx, USAGE),
        };
        if new_branch.is_some() || detach {
            return Err(Error::fatal("'--' cannot be combined with -b or --detach"));
        }
        return restore_paths(&repo, &mut index, rev, paths, ctx);
    }
    let target_name = match before.as_slice() {
        [] => None,
        [one] => Some(*one),
        _ => return usage_error(ctx, USAGE),
    };

    // `checkout <name>` where <name> is neither a branch nor a commit: a path.
    if let (Some(name), None) = (target_name, &new_branch) {
        let is_branch = branch_exists(&repo, name)?.is_some();
        if !is_branch && repo.rev_parse(name).is_err() {
            let rel = repo.rel_path(ctx.cwd, name)?;
            if index.contains(&rel) || index.contains_dir(&rel) {
                return restore_paths(&repo, &mut index, None, &[name], ctx);
            }
            return Err(Error::fatal(format!("invalid reference: {name}")));
        }
    }

    let current = repo.read_head()?;
    let head_oid = repo.head_oid()?;
    let from = repo.head_flat()?;

    // Work out where we are going.
    enum Dest {
        Branch(String, Oid),
        NewBranch(String, Oid),
        Detached(Oid),
    }
    let dest = if let Some(name) = new_branch {
        check_branch_name(&name)?;
        if branch_exists(&repo, &name)?.is_some() {
            return Err(Error::fatal(format!("a branch named '{name}' already exists")));
        }
        match target_name {
            Some(rev) => {
                let oid = peel_to_commit(&repo.odb(), &repo.rev_parse(rev)?)?;
                Dest::NewBranch(name, oid)
            }
            None if head_oid.is_none() => {
                repo.set_head_branch(&name)?;
                announce(ctx, &format!("Switched to a new branch '{name}'"))?;
                return Ok(0);
            }
            None => Dest::NewBranch(name, head_oid.ok_or_else(|| Error::fatal("no commits yet"))?),
        }
    } else {
        match target_name {
            None => return usage_error(ctx, USAGE),
            Some(name) => match branch_exists(&repo, name)? {
                Some(oid) if !detach => Dest::Branch(name.clone(), oid),
                _ => Dest::Detached(peel_to_commit(&repo.odb(), &repo.rev_parse(name)?)?),
            },
        }
    };
    let target_commit = match &dest {
        Dest::Branch(_, oid) | Dest::Detached(oid) | Dest::NewBranch(_, oid) => *oid,
    };

    if let (Dest::Branch(name, _), Head::Branch(cur)) = (&dest, &current) {
        if name == cur {
            announce(ctx, &format!("Already on '{name}'"))?;
            return Ok(0);
        }
    }
    let to = repo.commit_flat(&target_commit)?;
    let blockers = repo.switch_tree(&mut index, &from, &to, force)?;
    if !blockers.is_empty() {
        write!(ctx.err, "{}", describe_blockers(&blockers, "checkout", "switch branches"))?;
        return Ok(1);
    }
    repo.save_index(&index)?;
    match dest {
        Dest::Branch(name, _) => {
            repo.set_head_branch(&name)?;
            announce(ctx, &format!("Switched to branch '{name}'"))?;
        }
        Dest::NewBranch(name, _) => {
            repo.update_ref(&format!("refs/heads/{name}"), &target_commit)?;
            repo.set_head_branch(&name)?;
            announce(ctx, &format!("Switched to a new branch '{name}'"))?;
        }
        Dest::Detached(oid) => {
            repo.set_head_detached(&oid)?;
            let subject = read_commit(&repo.odb(), &oid)?.subject().to_string();
            announce(ctx, &format!("HEAD is now at {} {subject}", oid.short(7)))?;
        }
    }
    Ok(0)
}

/// `checkout [<commit>] -- <path>...`: overwrite files from the index (or a
/// commit, which also updates the index).
fn restore_paths<S: AsRef<str>>(repo: &Repo, index: &mut Index, rev: Option<&String>, paths: &[S], ctx: &mut Ctx<'_>) -> Result<i32> {
    let source: Flat = match rev {
        Some(rev) => repo.commit_flat(&peel_to_commit(&repo.odb(), &repo.rev_parse(rev)?)?)?,
        None => Repo::index_flat(index),
    };
    if paths.is_empty() {
        return usage_error(ctx, "checkout [<commit>] [--] <path>...");
    }
    let mut missing = false;
    for spec in paths {
        let spec = spec.as_ref();
        let rel = repo.rel_path(ctx.cwd, spec)?;
        let matched: Vec<(&String, &FlatEntry)> = source.iter().filter(|(p, _)| pathspec_matches(&rel, p)).collect();
        if matched.is_empty() {
            writeln!(ctx.err, "error: pathspec '{spec}' did not match any file(s) known to git")?;
            missing = true;
            continue;
        }
        for (path, entry) in matched {
            if rev.is_some() {
                repo.materialize(index, path, entry)?;
            } else {
                // Restore from the index; keep the entry (and refresh its stat data).
                let data = repo.blob_data(entry)?;
                repo.write_work_file(path, entry.mode, &data)?;
                if let Ok(meta) = fs::symlink_metadata(repo.abs_path(path)) {
                    let mut e = IndexEntry::from_metadata(path, &meta, entry.oid);
                    e.mode = entry.mode;
                    index.add(e);
                }
            }
        }
    }
    repo.save_index(index)?;
    Ok(i32::from(missing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocker_messages_match_git_wording() {
        let text = describe_blockers(
            &[Blocker::Local("a.txt".into()), Blocker::Untracked("b.txt".into())],
            "checkout",
            "switch branches",
        );
        assert_eq!(
            text,
            "error: Your local changes to the following files would be overwritten by checkout:\n\ta.txt\n\
             Please commit your changes or stash them before you switch branches.\n\
             error: The following untracked working tree files would be overwritten by checkout:\n\tb.txt\n\
             Please move or remove them before you switch branches.\nAborting\n"
        );
    }

    #[test]
    fn branch_names_are_validated() {
        assert!(check_branch_name("feature/x").is_ok());
        assert!(check_branch_name("-bad").is_err());
        assert!(check_branch_name("a..b").is_err());
        assert!(check_branch_name("x.lock").is_err());
        assert!(check_branch_name("").is_err());
    }
}
