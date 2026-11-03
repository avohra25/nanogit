//! `diff`: patch generation between the index, commits and the working tree.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::cli::{usage_error, Ctx};
use crate::diff::{is_binary, unified_diff};
use crate::error::{Error, Result};
use crate::index::Index;
use crate::object::{hash_object, mode, Kind, Object, Oid};
use crate::repo::Repo;
use crate::status::{tree_changes, Delta};
use crate::tree::{flatten, Flat, FlatEntry};
use crate::workdir::pathspec_matches;

/// One side of a file comparison.
pub struct Side {
    /// File mode.
    pub mode: u32,
    /// Blob id.
    pub oid: Oid,
    /// Contents.
    pub data: Vec<u8>,
}

fn abbrev(oid: Option<&Side>) -> String {
    oid.map_or_else(|| Oid::ZERO.short(7), |s| s.oid.short(7))
}

/// Render the `git diff`-style patch for one path.  Either side may be absent
/// (added / deleted file); a change between file types is shown as a deletion
/// followed by an addition, as git does.
pub fn file_diff(path: &str, old: Option<&Side>, new: Option<&Side>, context: usize) -> Vec<u8> {
    if let (Some(o), Some(n)) = (old, new) {
        if (o.mode == mode::SYMLINK) != (n.mode == mode::SYMLINK) {
            let mut out = file_diff(path, old, None, context);
            out.extend(file_diff(path, None, new, context));
            return out;
        }
    }
    let mut head = format!("diff --git a/{path} b/{path}\n");
    let mut index_line = None;
    match (old, new) {
        (None, Some(n)) => {
            head.push_str(&format!("new file mode {:06o}\n", n.mode));
            index_line = Some(format!("index {}..{}\n", abbrev(None), abbrev(Some(n))));
        }
        (Some(o), None) => {
            head.push_str(&format!("deleted file mode {:06o}\n", o.mode));
            index_line = Some(format!("index {}..{}\n", abbrev(Some(o)), abbrev(None)));
        }
        (Some(o), Some(n)) => {
            let same_mode = o.mode == n.mode;
            if !same_mode {
                head.push_str(&format!("old mode {:06o}\nnew mode {:06o}\n", o.mode, n.mode));
            }
            if o.oid != n.oid {
                let suffix = if same_mode { format!(" {:06o}", o.mode) } else { String::new() };
                index_line = Some(format!("index {}..{}{suffix}\n", abbrev(Some(o)), abbrev(Some(n))));
            }
        }
        (None, None) => return Vec::new(),
    }
    if let Some(line) = index_line {
        head.push_str(&line);
    }
    let mut out = head.into_bytes();
    let old_data: &[u8] = old.map_or(&[], |s| &s.data);
    let new_data: &[u8] = new.map_or(&[], |s| &s.data);
    if old_data == new_data {
        return out;
    }
    let from = if old.is_some() { format!("a/{path}") } else { "/dev/null".to_string() };
    let to = if new.is_some() { format!("b/{path}") } else { "/dev/null".to_string() };
    if is_binary(old_data) || is_binary(new_data) {
        out.extend(format!("Binary files {from} and {to} differ\n").into_bytes());
        return out;
    }
    out.extend(format!("--- {from}\n+++ {to}\n").into_bytes());
    out.extend(unified_diff(old_data, new_data, context));
    out
}

/// A set of files at one moment, with contents loaded on demand.
struct Snapshot {
    flat: Flat,
    /// Contents already in memory (working tree files).
    loaded: HashMap<String, Vec<u8>>,
}

impl Snapshot {
    fn from_flat(flat: Flat) -> Snapshot {
        Snapshot { flat, loaded: HashMap::new() }
    }

    fn side(&self, repo: &Repo, path: &str) -> Result<Option<Side>> {
        let Some(entry) = self.flat.get(path) else { return Ok(None) };
        let data = match self.loaded.get(path) {
            Some(d) => d.clone(),
            None if entry.mode == mode::GITLINK => format!("Subproject commit {}\n", entry.oid).into_bytes(),
            None => match repo.odb().read(&entry.oid)? {
                Object::Blob(b) => b,
                _ => return Err(Error::corrupt(format!("{} is not a blob", entry.oid))),
            },
        };
        Ok(Some(Side { mode: entry.mode, oid: entry.oid, data }))
    }
}

/// The tracked files of `index` as they are in the working tree.
fn work_snapshot(repo: &Repo, index: &Index) -> Result<Snapshot> {
    let mut snap = Snapshot::from_flat(Flat::new());
    for entry in index.entries.iter().filter(|e| e.stage == 0) {
        if let Some(wf) = repo.read_work_file(&entry.path)? {
            let oid = hash_object(Kind::Blob, &wf.data);
            snap.flat.insert(entry.path.clone(), FlatEntry { mode: wf.mode, oid });
            snap.loaded.insert(entry.path.clone(), wf.data);
        }
    }
    Ok(snap)
}

fn tree_flat(repo: &Repo, rev: &str) -> Result<Flat> {
    let oid = repo.rev_parse(rev)?;
    flatten(&repo.odb(), &repo.tree_of(&oid)?)
}

/// What the user asked to see.
enum Show {
    Patch,
    NameOnly,
    NameStatus,
}

/// `nanogit diff [--cached] [-U<n>] [--name-only | --name-status] [--exit-code | --quiet] [<commit> [<commit>]] [--] [<path>...]`
pub fn diff(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "diff [--cached] [-U<n>] [--name-only | --name-status] [--exit-code | --quiet] [<commit> [<commit>]] [--] [<path>...]";
    let (mut cached, mut exit_code, mut quiet) = (false, false, false);
    let mut show = Show::Patch;
    let mut context = 3usize;
    let mut operands: Vec<&String> = Vec::new();
    let mut explicit_paths: Vec<&String> = Vec::new();
    let mut dashdash = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let number = |s: &str| -> Result<usize> {
            s.parse().map_err(|_| Error::fatal(format!("'{s}' is not a number")))
        };
        match a.as_str() {
            _ if dashdash => explicit_paths.push(a),
            "--" => dashdash = true,
            "--cached" | "--staged" => cached = true,
            "--name-only" => show = Show::NameOnly,
            "--name-status" => show = Show::NameStatus,
            "--exit-code" => exit_code = true,
            "--quiet" | "-q" => {
                quiet = true;
                exit_code = true;
            }
            "--no-color" | "--no-renames" | "--no-ext-diff" | "-p" | "--patch" => {}
            "-U" | "--unified" => context = number(it.next().ok_or_else(|| Error::fatal("option '-U' needs a number"))?)?,
            s if s.starts_with("--unified=") => context = number(&s["--unified=".len()..])?,
            s if s.starts_with("-U") => context = number(&s[2..])?,
            s if s.starts_with('-') && s.len() > 1 => return usage_error(ctx, USAGE),
            _ => operands.push(a),
        }
    }
    let repo = Repo::discover(ctx.cwd)?;
    // Leading operands that resolve as revisions are revisions; the rest are paths.
    let mut revs: Vec<&String> = Vec::new();
    let mut paths = explicit_paths;
    for (i, op) in operands.iter().enumerate() {
        if !dashdash && revs.len() < 2 && paths.is_empty() && repo.rev_parse(op).is_ok() {
            revs.push(op);
        } else if dashdash && revs.len() < 2 && i == revs.len() {
            repo.rev_parse(op)?;
            revs.push(op);
        } else if dashdash {
            return Err(Error::fatal(format!("ambiguous argument '{op}': unknown revision or path not in the working tree.")));
        } else if repo.abs_path(&repo.rel_path(ctx.cwd, op)?).symlink_metadata().is_err() && !repo.load_index()?.contains(op) {
            return Err(Error::fatal(format!("ambiguous argument '{op}': unknown revision or path not in the working tree.")));
        } else {
            paths.push(op);
        }
    }
    let specs = paths.iter().map(|p| repo.rel_path(ctx.cwd, p)).collect::<Result<Vec<_>>>()?;
    if cached && revs.len() > 1 {
        return usage_error(ctx, USAGE);
    }
    let index = repo.load_index()?;
    let (old, new) = match (cached, revs.as_slice()) {
        (false, []) => (Snapshot::from_flat(Repo::index_flat(&index)), work_snapshot(&repo, &index)?),
        (true, []) => (Snapshot::from_flat(repo.head_flat()?), Snapshot::from_flat(Repo::index_flat(&index))),
        (true, [rev]) => (Snapshot::from_flat(tree_flat(&repo, rev)?), Snapshot::from_flat(Repo::index_flat(&index))),
        (false, [rev]) => (Snapshot::from_flat(tree_flat(&repo, rev)?), work_snapshot(&repo, &index)?),
        (false, [a, b]) => (Snapshot::from_flat(tree_flat(&repo, a)?), Snapshot::from_flat(tree_flat(&repo, b)?)),
        _ => return usage_error(ctx, USAGE),
    };
    let mut changes = tree_changes(&old.flat, &new.flat, false);
    changes.retain(|(p, _)| specs.is_empty() || specs.iter().any(|s| pathspec_matches(s, p)));
    // Conflicted paths are not compared against the work tree.
    let conflicted: BTreeSet<String> = index.conflicted_paths().into_iter().collect();
    let in_index_mode = !cached && revs.is_empty();
    if in_index_mode {
        changes.retain(|(p, _)| !conflicted.contains(p));
    }
    let differs = !changes.is_empty() || (in_index_mode && !conflicted.is_empty());
    if quiet {
        return Ok(i32::from(differs));
    }
    if in_index_mode {
        for path in conflicted.iter().filter(|p| specs.is_empty() || specs.iter().any(|s| pathspec_matches(s, p))) {
            writeln!(ctx.out, "* Unmerged path {path}")?;
        }
    }
    let mut named: BTreeMap<&str, &Delta> = BTreeMap::new();
    for (path, delta) in &changes {
        named.insert(path, delta);
    }
    for (path, delta) in &named {
        match show {
            Show::NameOnly => writeln!(ctx.out, "{path}")?,
            Show::NameStatus => {
                let code = match delta {
                    Delta::Added => 'A',
                    Delta::Deleted => 'D',
                    Delta::TypeChange => 'T',
                    _ => 'M',
                };
                writeln!(ctx.out, "{code}\t{path}")?;
            }
            Show::Patch => {
                let (o, n) = (old.side(&repo, path)?, new.side(&repo, path)?);
                ctx.out.write_all(&file_diff(path, o.as_ref(), n.as_ref(), context))?;
            }
        }
    }
    Ok(i32::from(exit_code && differs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(mode: u32, data: &str) -> Side {
        Side { mode, oid: hash_object(Kind::Blob, data.as_bytes()), data: data.as_bytes().to_vec() }
    }

    #[test]
    fn modified_file_header_and_hunk() {
        let (a, b) = (side(mode::FILE, "one\ntwo\n"), side(mode::FILE, "one\n2\n"));
        let text = String::from_utf8(file_diff("f.txt", Some(&a), Some(&b), 3)).unwrap();
        let want = format!(
            "diff --git a/f.txt b/f.txt\nindex {}..{} 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+2\n",
            a.oid.short(7),
            b.oid.short(7)
        );
        assert_eq!(text, want);
    }

    #[test]
    fn added_deleted_and_empty_files() {
        let n = side(mode::FILE, "x\n");
        let text = String::from_utf8(file_diff("n", None, Some(&n), 3)).unwrap();
        assert!(text.starts_with("diff --git a/n b/n\nnew file mode 100644\nindex 0000000.."));
        assert!(text.ends_with("--- /dev/null\n+++ b/n\n@@ -0,0 +1 @@\n+x\n"));
        let empty = side(mode::FILE, "");
        let text = String::from_utf8(file_diff("e", None, Some(&empty), 3)).unwrap();
        assert_eq!(text, "diff --git a/e b/e\nnew file mode 100644\nindex 0000000..e69de29\n");
        let text = String::from_utf8(file_diff("n", Some(&n), None, 3)).unwrap();
        assert!(text.contains("deleted file mode 100644\n") && text.ends_with("--- a/n\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n"));
    }

    #[test]
    fn mode_change_and_binary() {
        let (a, b) = (side(mode::FILE, "same\n"), side(mode::EXEC, "same\n"));
        let text = String::from_utf8(file_diff("run.sh", Some(&a), Some(&b), 3)).unwrap();
        assert_eq!(text, "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n");
        let bin = Side { mode: mode::FILE, oid: hash_object(Kind::Blob, b"\0\x01"), data: vec![0, 1] };
        let text = String::from_utf8(file_diff("b.bin", None, Some(&bin), 3)).unwrap();
        assert!(text.ends_with("Binary files /dev/null and b/b.bin differ\n"));
    }

    #[test]
    fn symlink_type_change_is_delete_then_add() {
        let (a, b) = (side(mode::FILE, "x\n"), side(mode::SYMLINK, "target"));
        let text = String::from_utf8(file_diff("l", Some(&a), Some(&b), 3)).unwrap();
        let del = text.find("deleted file mode").unwrap();
        let add = text.find("new file mode 120000").unwrap();
        assert!(del < add);
        assert!(text.contains("\\ No newline at end of file"));
    }
}
