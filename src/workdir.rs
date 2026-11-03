//! Working-tree operations: reading files into the index, path handling and
//! comparing the index against `HEAD` and the working tree.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};
use crate::index::{normalize_mode, Index, IndexEntry};
use crate::object::{mode, Kind, Oid};
use crate::repo::Repo;
use crate::tree::{flatten, write_tree, Flat, FlatEntry};

/// Decides whether an untracked path is skipped while walking (`.gitignore`).
pub type SkipFn<'a> = &'a mut dyn FnMut(&str, bool) -> bool;

/// True if `path` is selected by `spec` (a path or directory; empty = everything).
pub fn pathspec_matches(spec: &str, path: &str) -> bool {
    spec.is_empty()
        || path == spec
        || (path.len() > spec.len() && path.starts_with(spec) && path.as_bytes()[spec.len()] == b'/')
}

/// Express `path` relative to the directory `base` (both work-tree relative),
/// using `../` where needed, like git does for commands run in a subdirectory.
pub fn relative_to(base: &str, path: &str) -> String {
    let b: Vec<&str> = base.split('/').filter(|c| !c.is_empty()).collect();
    let p: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let common = b.iter().zip(&p).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<&str> = vec![".."; b.len() - common];
    parts.extend(&p[common..]);
    parts.join("/")
}

/// What lives at a path in the working tree.
#[derive(Debug)]
pub struct WorkFile {
    /// Normalised git mode.
    pub mode: u32,
    /// File contents, or the link target for symlinks.
    pub data: Vec<u8>,
    /// The file's `lstat` data.
    pub meta: fs::Metadata,
}

impl Repo {
    /// Path of the index file.
    pub fn index_path(&self) -> PathBuf {
        self.git_dir.join("index")
    }

    /// Load the index (empty if the file does not exist yet).
    pub fn load_index(&self) -> Result<Index> {
        Index::read(&self.index_path())
    }

    /// Save the index.
    pub fn save_index(&self, index: &Index) -> Result<()> {
        index.write(&self.index_path())
    }

    /// Convert a user-supplied path (relative to `cwd`) into a slash-separated
    /// path relative to the work tree.  `.` yields the empty string.
    pub fn rel_path(&self, cwd: &Path, arg: &str) -> Result<String> {
        let base = fs::canonicalize(cwd)?;
        let joined = if Path::new(arg).is_absolute() { PathBuf::from(arg) } else { base.join(arg) };
        let mut parts: Vec<OsString> = Vec::new();
        for comp in joined.components() {
            match comp {
                Component::Prefix(_) | Component::RootDir => parts.clear(),
                Component::CurDir => {}
                Component::ParentDir => {
                    parts.pop();
                }
                Component::Normal(s) => parts.push(s.to_os_string()),
            }
        }
        let normalized: PathBuf = std::iter::once(OsString::from("/")).chain(parts).collect();
        let rel = normalized
            .strip_prefix(&self.work_tree)
            .map_err(|_| Error::fatal(format!("'{arg}' is outside repository at '{}'", self.work_tree.display())))?;
        let mut names = Vec::new();
        for c in rel.components() {
            let s = c
                .as_os_str()
                .to_str()
                .ok_or_else(|| Error::fatal(format!("'{arg}' is not valid UTF-8")))?;
            names.push(s.to_string());
        }
        if names.first().is_some_and(|n| n == ".git") {
            return Err(Error::fatal(format!("'{arg}' is inside the .git directory")));
        }
        Ok(names.join("/"))
    }

    /// Absolute path of a work-tree-relative path.
    pub fn abs_path(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.work_tree.clone()
        } else {
            self.work_tree.join(rel)
        }
    }

    /// Read what is at `rel` in the working tree (`None` if absent or a directory).
    pub fn read_work_file(&self, rel: &str) -> Result<Option<WorkFile>> {
        let path = self.abs_path(rel);
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let ft = meta.file_type();
        let data = if ft.is_symlink() {
            fs::read_link(&path)?.into_os_string().into_vec()
        } else if ft.is_file() {
            fs::read(&path)?
        } else {
            return Ok(None);
        };
        use std::os::unix::fs::MetadataExt;
        Ok(Some(WorkFile { mode: normalize_mode(meta.mode()), data, meta }))
    }

    /// Hash and store the working-tree file at `rel`, adding it to `index`.
    pub fn stage_file(&self, index: &mut Index, rel: &str) -> Result<()> {
        let wf = self
            .read_work_file(rel)?
            .ok_or_else(|| Error::fatal(format!("pathspec '{rel}' did not match any files")))?;
        let oid = self.odb().write(Kind::Blob, &wf.data)?;
        index.add(IndexEntry::from_metadata(rel, &wf.meta, oid));
        Ok(())
    }

    /// List files below `dir` (relative, may be empty) in sorted order.
    /// `skip(path, is_dir)` can prune entries; `.git` is always skipped.
    pub fn walk_files(&self, dir: &str, skip: &mut dyn FnMut(&str, bool) -> bool) -> Result<Vec<String>> {
        let mut out = Vec::new();
        self.walk_into(dir, skip, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn walk_into(&self, dir: &str, skip: &mut dyn FnMut(&str, bool) -> bool, out: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(self.abs_path(dir))? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|n| Error::fatal(format!("{n:?} is not valid UTF-8")))?;
            if name == ".git" {
                continue;
            }
            let rel = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            let is_dir = entry.file_type()?.is_dir();
            if skip(&rel, is_dir) {
                continue;
            }
            if is_dir {
                self.walk_into(&rel, skip, out)?;
            } else {
                out.push(rel);
            }
        }
        Ok(())
    }

    /// `git add`: stage every pathspec (files, directories, deletions).
    pub fn add_paths(&self, index: &mut Index, specs: &[String], mut ignored: Option<SkipFn<'_>>) -> Result<()> {
        for spec in specs {
            let full = self.abs_path(spec);
            match fs::symlink_metadata(&full) {
                Ok(m) if m.is_dir() => {
                    let files = {
                        let tracked = &*index;
                        let mut skip = |p: &str, is_dir: bool| {
                            let is_tracked = if is_dir { tracked.contains_dir(p) } else { tracked.contains(p) };
                            !is_tracked && ignored.as_mut().is_some_and(|f| f(p, is_dir))
                        };
                        self.walk_files(spec, &mut skip)?
                    };
                    for f in &files {
                        self.stage_file(index, f)?;
                    }
                    let gone: Vec<String> = index
                        .entries
                        .iter()
                        .filter(|e| pathspec_matches(spec, &e.path) && self.read_work_file(&e.path).is_ok_and(|w| w.is_none()))
                        .map(|e| e.path.clone())
                        .collect();
                    for p in gone {
                        index.remove(&p);
                    }
                }
                Ok(_) => self.stage_file(index, spec)?,
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                    let tracked: Vec<String> = index
                        .entries
                        .iter()
                        .filter(|e| pathspec_matches(spec, &e.path))
                        .map(|e| e.path.clone())
                        .collect();
                    if tracked.is_empty() {
                        return Err(Error::fatal(format!("pathspec '{spec}' did not match any files")));
                    }
                    for p in tracked {
                        index.remove(&p);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Flattened tree of `HEAD` (empty before the first commit).
    pub fn head_flat(&self) -> Result<Flat> {
        match self.head_oid()? {
            Some(c) => flatten(&self.odb(), &self.tree_of(&c)?),
            None => Ok(Flat::new()),
        }
    }

    /// Stage-0 index entries as a flat listing.
    pub fn index_flat(index: &Index) -> Flat {
        index
            .entries
            .iter()
            .filter(|e| e.stage == 0)
            .map(|e| (e.path.clone(), FlatEntry { mode: e.mode, oid: e.oid }))
            .collect()
    }

    /// Write the index as tree objects; refuses while conflicts are unresolved.
    pub fn write_index_tree(&self, index: &Index) -> Result<Oid> {
        if index.has_conflicts() {
            let list = index.conflicted_paths().join("\n\t");
            return Err(Error::fatal(format!("cannot write a tree: unmerged paths:\n\t{list}")));
        }
        write_tree(
            &self.odb(),
            index.entries.iter().map(|e| (e.path.as_str(), FlatEntry { mode: e.mode, oid: e.oid })),
        )
    }

    /// `git rm`: drop paths from the index (and the work tree unless `cached`).
    /// Refuses to lose local modifications unless `force`.
    pub fn remove_paths(
        &self,
        index: &mut Index,
        specs: &[String],
        recursive: bool,
        cached: bool,
        force: bool,
    ) -> Result<Vec<String>> {
        let head = self.head_flat()?;
        let mut targets = BTreeSet::new();
        for spec in specs {
            let matched: Vec<&IndexEntry> =
                index.entries.iter().filter(|e| pathspec_matches(spec, &e.path)).collect();
            if matched.is_empty() {
                return Err(Error::fatal(format!("pathspec '{spec}' did not match any files")));
            }
            if matched.iter().any(|e| e.path != *spec) && !recursive {
                return Err(Error::fatal(format!("not removing '{spec}' recursively without -r")));
            }
            targets.extend(matched.iter().map(|e| e.path.clone()));
        }
        if !force {
            for path in &targets {
                let Some(entry) = index.get(path) else { continue };
                let work = self.read_work_file(path)?;
                let staged_differs = head.get(path).map(|h| h.oid) != Some(entry.oid);
                let work_differs = work.as_ref().is_some_and(|w| crate::object::hash_object(Kind::Blob, &w.data) != entry.oid);
                if cached {
                    if staged_differs && work_differs {
                        return Err(Error::fatal(format!(
                            "'{path}' has staged content different from both the file and the HEAD (use -f to force removal)"
                        )));
                    }
                } else if staged_differs {
                    return Err(Error::fatal(format!(
                        "'{path}' has changes staged in the index (use --cached to keep the file, or -f to force removal)"
                    )));
                } else if work_differs {
                    return Err(Error::fatal(format!(
                        "'{path}' has local modifications (use --cached to keep the file, or -f to force removal)"
                    )));
                }
            }
        }
        let removed: Vec<String> = targets.into_iter().collect();
        for path in &removed {
            index.remove(path);
            if !cached {
                self.remove_work_file(path)?;
            }
        }
        Ok(removed)
    }

    /// Delete a work-tree file and prune directories that became empty.
    pub fn remove_work_file(&self, rel: &str) -> Result<()> {
        match fs::remove_file(self.abs_path(rel)) {
            Ok(()) => {}
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            Err(e) => return Err(e.into()),
        }
        let mut dir = self.abs_path(rel);
        while dir.pop() && dir != self.work_tree {
            if fs::remove_dir(&dir).is_err() {
                break;
            }
        }
        Ok(())
    }

    /// Re-stat every tracked file, refreshing changed entries and dropping
    /// deleted ones (`git commit -a`).
    pub fn stage_tracked_changes(&self, index: &mut Index) -> Result<()> {
        let paths: Vec<String> = index.entries.iter().filter(|e| e.stage == 0).map(|e| e.path.clone()).collect();
        for path in paths {
            match self.read_work_file(&path)? {
                None => {
                    index.remove(&path);
                }
                Some(wf) => {
                    let current = index.get(&path).cloned();
                    if let Some(entry) = current {
                        if !entry.stat_matches(&wf.meta) {
                            let oid = self.odb().write(Kind::Blob, &wf.data)?;
                            index.add(IndexEntry::from_metadata(&path, &wf.meta, oid));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Bytes of a symlink target or file as a path (used by checkout code).
pub fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

/// True for the mode of a regular, non-link blob.
pub fn is_regular(m: u32) -> bool {
    m == mode::FILE || m == mode::EXEC
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(tag: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!("nanogit-wd-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Repo::init(&dir, "main").unwrap().0
    }

    fn put(repo: &Repo, rel: &str, data: &str) {
        let p = repo.abs_path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    #[test]
    fn pathspec_semantics() {
        assert!(pathspec_matches("", "a/b"));
        assert!(pathspec_matches("a", "a"));
        assert!(pathspec_matches("a", "a/b"));
        assert!(!pathspec_matches("a", "ab"));
        assert!(!pathspec_matches("a/b", "a"));
    }

    #[test]
    fn relative_display() {
        assert_eq!(relative_to("", "a/b"), "a/b");
        assert_eq!(relative_to("a", "a/b/c"), "b/c");
        assert_eq!(relative_to("a", "README"), "../README");
        assert_eq!(relative_to("a/b", "a/c/d"), "../c/d");
        assert_eq!(relative_to("a/b", "x"), "../../x");
    }

    #[test]
    fn rel_path_normalises() {
        let repo = repo("rel");
        let sub = repo.work_tree.join("src/deep");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(repo.rel_path(&sub, "x.rs").unwrap(), "src/deep/x.rs");
        assert_eq!(repo.rel_path(&sub, "../y.rs").unwrap(), "src/y.rs");
        assert_eq!(repo.rel_path(&sub, ".").unwrap(), "src/deep");
        assert_eq!(repo.rel_path(&repo.work_tree, ".").unwrap(), "");
        assert_eq!(repo.rel_path(&sub, "../..").unwrap(), "");
        assert!(repo.rel_path(&sub, "../../..").is_err());
        assert!(repo.rel_path(&sub, "../../.git/config").is_err());
        let abs = repo.work_tree.join("z.txt");
        assert_eq!(repo.rel_path(&sub, abs.to_str().unwrap()).unwrap(), "z.txt");
        fs::remove_dir_all(&repo.work_tree).unwrap();
    }

    #[test]
    fn add_directory_and_detect_deletions() {
        let repo = repo("add");
        put(&repo, "a.txt", "a");
        put(&repo, "d/b.txt", "b");
        put(&repo, "d/e/c.txt", "c");
        let mut idx = Index::new();
        repo.add_paths(&mut idx, &[String::new()], None).unwrap();
        let paths: Vec<&str> = idx.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "d/b.txt", "d/e/c.txt"]);
        fs::remove_file(repo.abs_path("d/b.txt")).unwrap();
        repo.add_paths(&mut idx, &["d".to_string()], None).unwrap();
        assert!(idx.get("d/b.txt").is_none());
        assert!(idx.get("d/e/c.txt").is_some());
        assert!(repo.add_paths(&mut idx, &["missing".to_string()], None).is_err());
        let mut skip = |p: &str, _d: bool| p.ends_with(".log");
        put(&repo, "x.log", "log");
        repo.add_paths(&mut idx, &[String::new()], Some(&mut skip)).unwrap();
        assert!(idx.get("x.log").is_none());
        fs::remove_dir_all(&repo.work_tree).unwrap();
    }

    #[test]
    fn symlinks_are_stored_as_targets() {
        let repo = repo("link");
        put(&repo, "real.txt", "data");
        std::os::unix::fs::symlink("real.txt", repo.abs_path("link")).unwrap();
        let mut idx = Index::new();
        repo.stage_file(&mut idx, "link").unwrap();
        let e = idx.get("link").unwrap();
        assert_eq!(e.mode, mode::SYMLINK);
        assert_eq!(e.oid, crate::object::hash_object(Kind::Blob, b"real.txt"));
        fs::remove_dir_all(&repo.work_tree).unwrap();
    }

    #[test]
    fn tracked_changes_are_restaged() {
        let repo = repo("tracked");
        put(&repo, "keep", "1");
        put(&repo, "gone", "1");
        let mut idx = Index::new();
        repo.add_paths(&mut idx, &[String::new()], None).unwrap();
        let before = idx.get("keep").unwrap().oid;
        std::thread::sleep(std::time::Duration::from_millis(5));
        put(&repo, "keep", "22");
        fs::remove_file(repo.abs_path("gone")).unwrap();
        repo.stage_tracked_changes(&mut idx).unwrap();
        assert_ne!(idx.get("keep").unwrap().oid, before);
        assert!(idx.get("gone").is_none());
        fs::remove_dir_all(&repo.work_tree).unwrap();
    }
}
