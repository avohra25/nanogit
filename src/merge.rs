//! Merging: merge bases, a diff3-style three-way line merge, tree-level
//! merging and the `merge` / `merge-base` commands.

use std::collections::{BTreeSet, HashSet};
use std::fs;

use crate::checkout::{describe_blockers, Blocker};
use crate::cli::{usage_error, Ctx};
use crate::diff::{diff_lines, is_binary, matching_blocks, split_lines, Op};
use crate::error::{Error, Result};
use crate::history::{is_ancestor, peel_to_commit, read_commit};
use crate::ignore::Ignore;
use crate::index::{Index, IndexEntry};
use crate::object::{mode, Kind, ObjectStore, Oid};
use crate::refs::Head;
use crate::repo::Repo;
use crate::tree::{Flat, FlatEntry};

/// All best common ancestors of two commits, newest first.
pub fn merge_bases(odb: &ObjectStore, a: &Oid, b: &Oid) -> Result<Vec<Oid>> {
    let (a, b) = (peel_to_commit(odb, a)?, peel_to_commit(odb, b)?);
    if a == b {
        return Ok(vec![a]);
    }
    let mut from_a = HashSet::new();
    let mut stack = vec![a];
    while let Some(c) = stack.pop() {
        if from_a.insert(c) {
            stack.extend(read_commit(odb, &c)?.parents);
        }
    }
    // Walk back from `b`, stopping at the first commit `a` can also reach.
    let mut common = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![b];
    while let Some(c) = stack.pop() {
        if !seen.insert(c) {
            continue;
        }
        if from_a.contains(&c) {
            common.push(c);
        } else {
            stack.extend(read_commit(odb, &c)?.parents);
        }
    }
    let mut best = Vec::new();
    for c in &common {
        let mut shadowed = false;
        for d in &common {
            if d != c && is_ancestor(odb, c, d)? {
                shadowed = true;
                break;
            }
        }
        if !shadowed {
            best.push(*c);
        }
    }
    let mut dated = Vec::new();
    for c in best {
        dated.push((read_commit(odb, &c)?.committer.time, c));
    }
    dated.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
    Ok(dated.into_iter().map(|(_, c)| c).collect())
}

/// Result of merging three versions of a text.
#[derive(Debug, PartialEq, Eq)]
pub struct TextMerge {
    /// Merged text, with conflict markers where the sides disagree.
    pub text: Vec<u8>,
    /// Number of conflict blocks in `text`.
    pub conflicts: usize,
}

/// A stretch of lines all three versions agree on.
struct Sync {
    base: (usize, usize),
    ours: usize,
    theirs: usize,
}

fn sync_regions(ma: &[(usize, usize, usize)], mb: &[(usize, usize, usize)], lens: (usize, usize, usize)) -> Vec<Sync> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < ma.len() && j < mb.len() {
        let (ab, aa, al) = ma[i];
        let (bb, ba, bl) = mb[j];
        let lo = ab.max(bb);
        let hi = (ab + al).min(bb + bl);
        if lo < hi {
            out.push(Sync { base: (lo, hi), ours: aa + (lo - ab), theirs: ba + (lo - bb) });
        }
        if ab + al < bb + bl {
            i += 1;
        } else {
            j += 1;
        }
    }
    out.push(Sync { base: (lens.0, lens.0), ours: lens.1, theirs: lens.2 });
    out
}

fn push_lines(out: &mut Vec<u8>, lines: &[&[u8]]) {
    for l in lines {
        out.extend_from_slice(l);
    }
}

fn push_side(out: &mut Vec<u8>, lines: &[&[u8]]) {
    push_lines(out, lines);
    if lines.last().is_some_and(|l| !l.ends_with(b"\n")) {
        out.push(b'\n');
    }
}

/// Emit a conflict, first moving lines both sides share out of it (git's
/// "zealous" refinement) so each block holds only real disagreement.
fn emit_conflict(out: &mut Vec<u8>, count: &mut usize, ours: &[&[u8]], theirs: &[&[u8]], labels: (&str, &str)) {
    let ops = diff_lines(ours, theirs);
    let (mut i, mut j) = (0, 0);
    // Start of the pending run of disagreeing lines on each side.
    let (mut start_o, mut start_t) = (0, 0);
    let flush = |out: &mut Vec<u8>, count: &mut usize, (so, eo): (usize, usize), (st, et): (usize, usize)| {
        if so == eo && st == et {
            return;
        }
        *count += 1;
        out.extend_from_slice(format!("<<<<<<< {}\n", labels.0).as_bytes());
        push_side(out, &ours[so..eo]);
        out.extend_from_slice(b"=======\n");
        push_side(out, &theirs[st..et]);
        out.extend_from_slice(format!(">>>>>>> {}\n", labels.1).as_bytes());
    };
    for op in ops {
        match op {
            Op::Equal => {
                flush(out, count, (start_o, i), (start_t, j));
                out.extend_from_slice(ours[i]);
                i += 1;
                j += 1;
                (start_o, start_t) = (i, j);
            }
            Op::Delete => i += 1,
            Op::Insert => j += 1,
        }
    }
    flush(out, count, (start_o, i), (start_t, j));
}

/// Three-way merge of `ours` and `theirs` relative to their common `base`.
/// Changes that do not touch (or abut) each other combine cleanly; identical
/// changes on both sides count once; anything else becomes a conflict block.
pub fn merge_text(base: &[u8], ours: &[u8], theirs: &[u8], labels: (&str, &str)) -> TextMerge {
    let (b, o, t) = (split_lines(base), split_lines(ours), split_lines(theirs));
    let ma = matching_blocks(&diff_lines(&b, &o));
    let mb = matching_blocks(&diff_lines(&b, &t));
    let mut out = Vec::new();
    let mut conflicts = 0;
    let (mut iz, mut ia, mut ib) = (0, 0, 0);
    for s in sync_regions(&ma, &mb, (b.len(), o.len(), t.len())) {
        let (bs, os, ts) = (&b[iz..s.base.0], &o[ia..s.ours], &t[ib..s.theirs]);
        if !(bs.is_empty() && os.is_empty() && ts.is_empty()) {
            if os == bs && ts != bs {
                push_lines(&mut out, ts);
            } else if ts == bs || os == ts {
                push_lines(&mut out, os);
            } else {
                emit_conflict(&mut out, &mut conflicts, os, ts, labels);
            }
        }
        let len = s.base.1 - s.base.0;
        push_lines(&mut out, &o[s.ours..s.ours + len]);
        iz = s.base.1;
        ia = s.ours + len;
        ib = s.theirs + len;
    }
    TextMerge { text: out, conflicts }
}

/// Why a path could not be merged automatically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    /// Both sides changed the same region of a text file.
    Content,
    /// Both sides added the path with different contents.
    AddAdd,
    /// One side deleted what the other modified.
    ModifyDelete { deleted_by_ours: bool },
    /// Binary files, symlinks or submodules changed on both sides.
    Binary,
}

/// A path left conflicted by a tree merge.
#[derive(Clone, Debug)]
pub struct PathConflict {
    pub path: String,
    pub kind: ConflictKind,
    pub base: Option<FlatEntry>,
    pub ours: Option<FlatEntry>,
    pub theirs: Option<FlatEntry>,
    /// Mode and contents to leave in the work tree, when that is not simply
    /// what is already there.
    pub worktree: Option<(u32, Vec<u8>)>,
}

/// Outcome of merging two trees.
#[derive(Debug, Default)]
pub struct TreeMerge {
    /// Cleanly merged entries.
    pub merged: Flat,
    pub conflicts: Vec<PathConflict>,
    /// Progress lines in git's wording (`Auto-merging x`, `CONFLICT (...)`).
    pub notes: Vec<String>,
}

fn is_plain_file(m: u32) -> bool {
    m == mode::FILE || m == mode::EXEC
}

impl Repo {
    /// Merge the trees `ours` and `theirs` relative to `base`.
    pub fn merge_trees(&self, base: &Flat, ours: &Flat, theirs: &Flat, names: (&str, &str)) -> Result<TreeMerge> {
        let mut res = TreeMerge::default();
        let paths: BTreeSet<&String> = base.keys().chain(ours.keys()).chain(theirs.keys()).collect();
        for path in paths {
            let (b, o, t) = (base.get(path).copied(), ours.get(path).copied(), theirs.get(path).copied());
            if o == t || b == t {
                if let Some(e) = o {
                    res.merged.insert(path.clone(), e);
                }
                continue;
            }
            if b == o {
                if let Some(e) = t {
                    res.merged.insert(path.clone(), e);
                }
                continue;
            }
            match (o, t) {
                (Some(o), Some(t)) => self.merge_file(&mut res, path, b, o, t, names)?,
                (None, Some(_)) | (Some(_), None) => {
                    let deleted_by_ours = o.is_none();
                    let (gone, kept) = if deleted_by_ours { (names.0, names.1) } else { (names.1, names.0) };
                    res.notes.push(format!(
                        "CONFLICT (modify/delete): {path} deleted in {gone} and modified in {kept}.  Version {kept} of {path} left in tree."
                    ));
                    let worktree = match t {
                        Some(e) if deleted_by_ours => Some((e.mode, self.blob_data(&e)?)),
                        _ => None,
                    };
                    res.conflicts.push(PathConflict {
                        path: path.clone(),
                        kind: ConflictKind::ModifyDelete { deleted_by_ours },
                        base: b,
                        ours: o,
                        theirs: t,
                        worktree,
                    });
                }
                (None, None) => {}
            }
        }
        let mut all: HashSet<&str> = res.merged.keys().map(String::as_str).collect();
        all.extend(res.conflicts.iter().map(|c| c.path.as_str()));
        for p in &all {
            let mut end = 0;
            while let Some(pos) = p[end..].find('/') {
                if all.contains(&p[..end + pos]) {
                    return Err(Error::fatal(format!(
                        "CONFLICT (file/directory): '{}' is both a file and a directory; this case is not supported",
                        &p[..end + pos]
                    )));
                }
                end += pos + 1;
            }
        }
        Ok(res)
    }

    fn merge_file(
        &self,
        res: &mut TreeMerge,
        path: &str,
        b: Option<FlatEntry>,
        o: FlatEntry,
        t: FlatEntry,
        names: (&str, &str),
    ) -> Result<()> {
        let conflict = |res: &mut TreeMerge, kind: ConflictKind, worktree| {
            res.conflicts.push(PathConflict { path: path.to_string(), kind, base: b, ours: Some(o), theirs: Some(t), worktree });
        };
        let (bd, od, td) = match b {
            Some(b) if is_plain_file(b.mode) => (self.blob_data(&b)?, self.blob_data(&o)?, self.blob_data(&t)?),
            Some(_) => (Vec::new(), self.blob_data(&o)?, self.blob_data(&t)?),
            None => (Vec::new(), self.blob_data(&o)?, self.blob_data(&t)?),
        };
        let mergeable = is_plain_file(o.mode) && is_plain_file(t.mode) && b.is_none_or(|b| is_plain_file(b.mode));
        if !mergeable || is_binary(&bd) || is_binary(&od) || is_binary(&td) {
            res.notes.push(format!("warning: Cannot merge binary files: {path} ({} vs. {})", names.0, names.1));
            res.notes.push(format!("CONFLICT (content): Merge conflict in {path}"));
            conflict(res, ConflictKind::Binary, None);
            return Ok(());
        }
        res.notes.push(format!("Auto-merging {path}"));
        let merged = merge_text(&bd, &od, &td, names);
        let file_mode = match b {
            _ if o.mode == t.mode => Some(o.mode),
            Some(b) if b.mode == o.mode => Some(t.mode),
            Some(b) if b.mode == t.mode => Some(o.mode),
            _ => None,
        };
        match (merged.conflicts, file_mode) {
            (0, Some(m)) => {
                let oid = self.odb().write(Kind::Blob, &merged.text)?;
                res.merged.insert(path.to_string(), FlatEntry { mode: m, oid });
            }
            (n, m) => {
                let (kind, what) = if b.is_none() { (ConflictKind::AddAdd, "add/add") } else { (ConflictKind::Content, "content") };
                if n == 0 {
                    res.notes.push(format!("CONFLICT (permissions): Merge conflict in {path}"));
                } else {
                    res.notes.push(format!("CONFLICT ({what}): Merge conflict in {path}"));
                }
                conflict(res, kind, Some((m.unwrap_or(o.mode), merged.text)));
            }
        }
        Ok(())
    }

    fn merge_state(&self, name: &str) -> std::path::PathBuf {
        self.git_dir.join(name)
    }

    /// The commit being merged while a merge is in progress.
    pub fn merge_head(&self) -> Result<Option<Oid>> {
        match fs::read_to_string(self.merge_state("MERGE_HEAD")) {
            Ok(s) => Oid::from_hex(s.trim()).map(Some).ok_or_else(|| Error::corrupt("bad MERGE_HEAD")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The saved commit message of an unfinished merge.
    pub fn merge_msg(&self) -> Option<String> {
        fs::read_to_string(self.merge_state("MERGE_MSG")).ok()
    }

    /// Forget any unfinished merge.
    pub fn clear_merge_state(&self) -> Result<()> {
        for name in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE"] {
            match fs::remove_file(self.merge_state(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn begin_merge(&self, theirs: &Oid, message: &str, orig_head: &Oid) -> Result<()> {
        fs::write(self.merge_state("MERGE_HEAD"), format!("{theirs}\n"))?;
        fs::write(self.merge_state("MERGE_MSG"), message)?;
        fs::write(self.merge_state("ORIG_HEAD"), format!("{orig_head}\n"))?;
        Ok(())
    }

    /// Paths where the index differs from `HEAD`.
    fn staged_blockers(&self, index: &Index) -> Result<Vec<Blocker>> {
        let (head, idx) = (self.head_flat()?, Repo::index_flat(index));
        let paths: BTreeSet<&String> = head.keys().chain(idx.keys()).collect();
        Ok(paths.into_iter().filter(|p| head.get(*p) != idx.get(*p)).map(|p| Blocker::Local(p.clone())).collect())
    }

    /// Would writing `conflicts` into the work tree destroy local work?
    fn conflict_blockers(&self, index: &Index, conflicts: &[PathConflict]) -> Result<Vec<Blocker>> {
        let mut ignore = Ignore::new(self);
        let mut blockers = Vec::new();
        for c in conflicts.iter().filter(|c| c.worktree.is_some()) {
            match index.get(&c.path) {
                Some(entry) => {
                    if self.entry_delta(index, entry)?.is_some() {
                        blockers.push(Blocker::Local(c.path.clone()));
                    }
                }
                None => {
                    if fs::symlink_metadata(self.abs_path(&c.path)).is_ok() && !ignore.is_ignored(&c.path, false) {
                        blockers.push(Blocker::Untracked(c.path.clone()));
                    }
                }
            }
        }
        Ok(blockers)
    }
}

fn merge_message(repo: &Repo, spec: &str, into: &Head) -> Result<String> {
    let what = if repo.read_ref(&format!("refs/heads/{spec}"))?.is_some() {
        "branch"
    } else if repo.read_ref(&format!("refs/tags/{spec}"))?.is_some() {
        "tag"
    } else {
        "commit"
    };
    let mut msg = format!("Merge {what} '{spec}'");
    match into {
        Head::Branch(b) if b == "main" || b == "master" => {}
        Head::Branch(b) => msg.push_str(&format!(" into {b}")),
        Head::Detached(_) => msg.push_str(" into HEAD"),
    }
    msg.push('\n');
    Ok(msg)
}

/// `nanogit merge-base [--all] [--is-ancestor] <commit> <commit>`
pub fn merge_base(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "merge-base [--all | --is-ancestor] <commit> <commit>";
    let (mut all, mut is_anc) = (false, false);
    let mut revs = Vec::new();
    for a in args {
        match a.as_str() {
            "--all" | "-a" => all = true,
            "--is-ancestor" => is_anc = true,
            s if s.starts_with('-') => return usage_error(ctx, USAGE),
            s => revs.push(s),
        }
    }
    let [a, b] = revs[..] else { return usage_error(ctx, USAGE) };
    let repo = Repo::discover(ctx.cwd)?;
    let odb = repo.odb();
    let (a, b) = (peel_to_commit(&odb, &repo.rev_parse(a)?)?, peel_to_commit(&odb, &repo.rev_parse(b)?)?);
    if is_anc {
        return Ok(if is_ancestor(&odb, &a, &b)? { 0 } else { 1 });
    }
    let bases = merge_bases(&odb, &a, &b)?;
    if bases.is_empty() {
        return Ok(1);
    }
    for base in bases.iter().take(if all { usize::MAX } else { 1 }) {
        writeln!(ctx.out, "{base}")?;
    }
    Ok(0)
}

/// Conflicted index paths that `target` lacks (modify/delete, add/add).  They
/// leave work files that `switch_tree` cannot see, so a hard reset must
/// remove them explicitly.
pub(crate) fn conflict_strays(index: &Index, target: &Flat) -> Vec<String> {
    index.entries.iter().filter(|e| e.stage > 0 && !target.contains_key(&e.path)).map(|e| e.path.clone()).collect()
}

impl Repo {
    /// Delete the work files at `paths` (files only, never directories).
    pub(crate) fn remove_strays(&self, paths: &[String]) -> Result<()> {
        for path in paths {
            let full = self.work_tree.join(path);
            if full.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
                fs::remove_file(full)?;
            }
        }
        Ok(())
    }
}

fn abort_merge(repo: &Repo, ctx: &mut Ctx<'_>) -> Result<i32> {
    if repo.merge_head()?.is_none() {
        return Err(Error::fatal("There is no merge to abort (MERGE_HEAD missing)."));
    }
    let head = repo.head_oid()?.ok_or_else(|| Error::fatal("no commit to return to"))?;
    let mut index = repo.load_index()?;
    let target = repo.commit_flat(&head)?;
    let stray = conflict_strays(&index, &target);
    repo.switch_tree(&mut index, &target, &target, true)?;
    repo.remove_strays(&stray)?;
    repo.save_index(&index)?;
    repo.clear_merge_state()?;
    writeln!(ctx.err, "Merge aborted.")?;
    Ok(0)
}

/// `nanogit merge [--no-ff | --ff-only] [--no-commit] [-m <msg>] <commit>` or `merge --abort`
pub fn merge(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "merge [--no-ff | --ff-only] [--no-commit] [-m <message>] <commit> | merge --abort";
    let (mut no_ff, mut ff_only, mut no_commit, mut abort) = (false, false, false, false);
    let (mut message, mut target): (Option<String>, Option<String>) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-ff" => no_ff = true,
            "--ff-only" => ff_only = true,
            "--no-commit" => no_commit = true,
            "--abort" => abort = true,
            "-m" | "--message" => message = Some(it.next().ok_or_else(|| Error::fatal("switch 'm' requires a value"))?.clone()),
            s if s.starts_with('-') => return usage_error(ctx, USAGE),
            s if target.is_none() => target = Some(s.to_string()),
            _ => return usage_error(ctx, USAGE),
        }
    }
    if no_ff && ff_only {
        return Err(Error::fatal("options '--no-ff' and '--ff-only' cannot be used together"));
    }
    let repo = Repo::discover(ctx.cwd)?;
    if abort {
        return abort_merge(&repo, ctx);
    }
    let Some(spec) = target else { return usage_error(ctx, USAGE) };
    if repo.merge_head()?.is_some() {
        writeln!(ctx.err, "fatal: You have not concluded your merge (MERGE_HEAD exists).")?;
        writeln!(ctx.err, "Please, commit your changes before you merge.")?;
        return Ok(128);
    }
    let odb = repo.odb();
    let Some(head) = repo.head_oid()? else {
        return Err(Error::fatal("cannot merge into an unborn branch"));
    };
    let Ok(theirs) = repo.rev_parse(&spec).and_then(|o| peel_to_commit(&odb, &o)) else {
        writeln!(ctx.err, "merge: {spec} - not something we can merge")?;
        return Ok(1);
    };
    let mut index = repo.load_index()?;
    if index.has_conflicts() {
        writeln!(ctx.err, "error: Merging is not possible because you have unmerged files.")?;
        writeln!(ctx.err, "fatal: Exiting because of an unresolved conflict.")?;
        return Ok(128);
    }
    let staged = repo.staged_blockers(&index)?;
    if !staged.is_empty() {
        write!(ctx.err, "{}", describe_blockers(&staged, "merge", "merge"))?;
        return Ok(1);
    }
    if is_ancestor(&odb, &theirs, &head)? {
        writeln!(ctx.out, "Already up to date.")?;
        return Ok(0);
    }
    let head_flat = repo.head_flat()?;
    if !no_ff && is_ancestor(&odb, &head, &theirs)? {
        let to = repo.commit_flat(&theirs)?;
        let blockers = repo.switch_tree(&mut index, &head_flat, &to, false)?;
        if !blockers.is_empty() {
            write!(ctx.err, "{}", describe_blockers(&blockers, "merge", "merge"))?;
            return Ok(1);
        }
        repo.save_index(&index)?;
        repo.update_head(&theirs)?;
        writeln!(ctx.out, "Updating {}..{}", head.short(7), theirs.short(7))?;
        writeln!(ctx.out, "Fast-forward")?;
        return Ok(0);
    }
    if ff_only {
        writeln!(ctx.err, "fatal: Not possible to fast-forward, aborting.")?;
        return Ok(128);
    }
    let Some(base) = merge_bases(&odb, &head, &theirs)?.first().copied() else {
        writeln!(ctx.err, "fatal: refusing to merge unrelated histories")?;
        return Ok(128);
    };
    let base_flat = repo.commit_flat(&base)?;
    let theirs_flat = repo.commit_flat(&theirs)?;
    let result = repo.merge_trees(&base_flat, &head_flat, &theirs_flat, ("HEAD", &spec))?;

    let blockers = repo.conflict_blockers(&index, &result.conflicts)?;
    if !blockers.is_empty() {
        write!(ctx.err, "{}", describe_blockers(&blockers, "merge", "merge"))?;
        return Ok(1);
    }
    let mut to = result.merged.clone();
    for c in &result.conflicts {
        if let Some(e) = c.ours {
            to.insert(c.path.clone(), e);
        }
    }
    let blockers = repo.switch_tree(&mut index, &head_flat, &to, false)?;
    if !blockers.is_empty() {
        write!(ctx.err, "{}", describe_blockers(&blockers, "merge", "merge"))?;
        return Ok(1);
    }
    for note in &result.notes {
        writeln!(ctx.out, "{note}")?;
    }
    for c in &result.conflicts {
        index.remove(&c.path);
        for (stage, entry) in [(1u8, c.base), (2, c.ours), (3, c.theirs)] {
            if let Some(e) = entry {
                let mut ie = IndexEntry::new(&c.path, e.mode, e.oid);
                ie.stage = stage;
                index.add(ie);
            }
        }
        if let Some((file_mode, data)) = &c.worktree {
            repo.write_work_file(&c.path, *file_mode, data)?;
        }
    }
    repo.save_index(&index)?;
    let message = match message {
        Some(m) => m,
        None => merge_message(&repo, &spec, &repo.read_head()?)?,
    };
    if !result.conflicts.is_empty() {
        repo.begin_merge(&theirs, &message, &head)?;
        writeln!(ctx.out, "Automatic merge failed; fix conflicts and then commit the result.")?;
        return Ok(1);
    }
    if no_commit {
        repo.begin_merge(&theirs, &message, &head)?;
        writeln!(ctx.out, "Automatic merge went well; stopped before committing as requested")?;
        return Ok(0);
    }
    repo.commit_index(&index, &message, &[theirs])?;
    writeln!(ctx.out, "Merge made by the 'three-way' strategy.")?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(base: &str, ours: &str, theirs: &str) -> (String, usize) {
        let r = merge_text(base.as_bytes(), ours.as_bytes(), theirs.as_bytes(), ("ours", "theirs"));
        (String::from_utf8(r.text).unwrap(), r.conflicts)
    }

    #[test]
    fn disjoint_edits_merge_cleanly() {
        let base = "a\nb\nc\nd\ne\nf\n";
        assert_eq!(m(base, "A\nb\nc\nd\ne\nf\n", "a\nb\nc\nd\ne\nF\n"), ("A\nb\nc\nd\ne\nF\n".into(), 0));
        assert_eq!(m(base, base, "x\n"), ("x\n".into(), 0));
        assert_eq!(m(base, "x\n", base), ("x\n".into(), 0));
    }

    #[test]
    fn identical_changes_are_not_conflicts() {
        assert_eq!(m("a\nb\nc\n", "a\nX\nc\n", "a\nX\nc\n"), ("a\nX\nc\n".into(), 0));
        assert_eq!(m("a\nb\nc\n", "a\nc\n", "a\nc\n"), ("a\nc\n".into(), 0));
    }

    #[test]
    fn overlapping_edits_conflict_with_markers() {
        let (text, n) = m("a\nb\nc\n", "a\nours\nc\n", "a\ntheirs\nc\n");
        assert_eq!(n, 1);
        assert_eq!(text, "a\n<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\nc\n");
    }

    #[test]
    fn adjacent_edits_conflict_like_git() {
        let (_, n) = m("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nb\nC\nd\n");
        assert_eq!(n, 1);
    }

    #[test]
    fn conflicts_are_trimmed_to_the_disagreement() {
        // Both sides rewrote lines 2-4; line 3 ends up equal on both.
        let (text, n) = m("a\nb\nc\nd\ne\n", "a\n1\nSAME\n2\ne\n", "a\nx\nSAME\ny\ne\n");
        assert_eq!(n, 2);
        assert_eq!(
            text,
            "a\n<<<<<<< ours\n1\n=======\nx\n>>>>>>> theirs\nSAME\n<<<<<<< ours\n2\n=======\ny\n>>>>>>> theirs\ne\n"
        );
    }

    #[test]
    fn insertions_at_the_same_spot_conflict_unless_equal() {
        assert_eq!(m("a\nb\n", "a\nx\nb\n", "a\ny\nb\n").1, 1);
        assert_eq!(m("a\nb\n", "a\nx\nb\n", "a\nx\nb\n"), ("a\nx\nb\n".into(), 0));
        assert_eq!(m("a\nb\n", "x\na\nb\n", "a\nb\ny\n"), ("x\na\nb\ny\n".into(), 0));
    }

    #[test]
    fn modify_versus_delete_conflicts() {
        let (text, n) = m("a\nb\nc\n", "a\nc\n", "a\nB\nc\n");
        assert_eq!(n, 1);
        assert_eq!(text, "a\n<<<<<<< ours\n=======\nB\n>>>>>>> theirs\nc\n");
    }

    #[test]
    fn missing_final_newline_gets_one_before_markers() {
        let (text, n) = m("a\nb", "a\nx", "a\ny");
        assert_eq!(n, 1);
        assert_eq!(text, "a\n<<<<<<< ours\nx\n=======\ny\n>>>>>>> theirs\n");
    }

    #[test]
    fn empty_inputs() {
        assert_eq!(m("", "", ""), (String::new(), 0));
        assert_eq!(m("", "new\n", ""), ("new\n".into(), 0));
        assert_eq!(m("", "one\n", "two\n").1, 1);
    }
}
