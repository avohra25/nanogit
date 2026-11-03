//! References: `HEAD`, branches under `refs/heads/`, tags, and `packed-refs`.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use crate::error::{Error, Result};
use crate::object::Oid;
use crate::repo::Repo;

/// What `HEAD` currently points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    /// A branch (short name, e.g. `main`); it may not exist yet (unborn).
    Branch(String),
    /// A detached commit.
    Detached(Oid),
}

/// Check a full ref name (or branch short name when `branch` is true) against
/// the rules of `git check-ref-format`.
pub fn valid_ref_name(name: &str) -> bool {
    if name.is_empty() || name == "@" || name.starts_with('/') || name.ends_with('/') || name.ends_with('.') {
        return false;
    }
    if name.contains("..") || name.contains("@{") || name.contains("//") {
        return false;
    }
    if name.chars().any(|c| c.is_control() || " ~^:?*[\\".contains(c)) {
        return false;
    }
    name.split('/').all(|part| !part.starts_with('.') && !part.ends_with(".lock"))
}

fn parse_packed(text: &str) -> BTreeMap<String, Oid> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        if let Some((hex, name)) = line.split_once(' ') {
            if let Some(oid) = Oid::from_hex(hex) {
                map.insert(name.to_string(), oid);
            }
        }
    }
    map
}

impl Repo {
    fn packed_refs(&self) -> BTreeMap<String, Oid> {
        fs::read_to_string(self.git_dir.join("packed-refs"))
            .map(|t| parse_packed(&t))
            .unwrap_or_default()
    }

    /// Parse `.git/HEAD`.
    pub fn read_head(&self) -> Result<Head> {
        let text = fs::read_to_string(self.git_dir.join("HEAD"))?;
        let text = text.trim();
        if let Some(target) = text.strip_prefix("ref: ") {
            return match target.trim().strip_prefix("refs/heads/") {
                Some(branch) => Ok(Head::Branch(branch.to_string())),
                None => Err(Error::fatal("HEAD points outside refs/heads/")),
            };
        }
        Oid::from_hex(text)
            .map(Head::Detached)
            .ok_or_else(|| Error::corrupt("HEAD is neither a ref nor an object id"))
    }

    /// The commit `HEAD` resolves to, or `None` on an unborn branch.
    pub fn head_oid(&self) -> Result<Option<Oid>> {
        match self.read_head()? {
            Head::Branch(b) => self.read_ref(&format!("refs/heads/{b}")),
            Head::Detached(oid) => Ok(Some(oid)),
        }
    }

    /// Resolve a full ref name such as `refs/heads/main`.
    pub fn read_ref(&self, name: &str) -> Result<Option<Oid>> {
        let path = self.git_dir.join(name);
        if path.is_file() {
            let text = fs::read_to_string(&path)?;
            let text = text.trim();
            if let Some(target) = text.strip_prefix("ref: ") {
                return self.read_ref(target.trim());
            }
            return Oid::from_hex(text)
                .map(Some)
                .ok_or_else(|| Error::corrupt(format!("ref {name} does not contain an object id")));
        }
        Ok(self.packed_refs().get(name).copied())
    }

    /// Create or move a ref atomically (via a `.lock` file).
    pub fn update_ref(&self, name: &str, oid: &Oid) -> Result<()> {
        if !name.starts_with("refs/") || !valid_ref_name(name) {
            return Err(Error::fatal(format!("'{name}' is not a valid ref name")));
        }
        self.write_locked(&self.git_dir.join(name), &format!("{oid}\n"))
    }

    fn write_locked(&self, path: &Path, contents: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut lock_name = path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock = std::path::PathBuf::from(lock_name);
        match fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(mut f) => {
                use std::io::Write;
                f.write_all(contents.as_bytes())?;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                return Err(Error::fatal(format!(
                    "unable to create '{}': File exists (another process running?)",
                    lock.display()
                )));
            }
            Err(e) => return Err(e.into()),
        }
        fs::rename(&lock, path)?;
        Ok(())
    }

    /// Delete a ref (loose and packed).  Returns whether it existed.
    pub fn delete_ref(&self, name: &str) -> Result<bool> {
        let mut existed = false;
        let path = self.git_dir.join(name);
        if path.is_file() {
            fs::remove_file(&path)?;
            existed = true;
            let stop = self.git_dir.join("refs");
            let mut dir = path.parent();
            while let Some(d) = dir {
                if d == stop || !d.starts_with(&stop) || fs::remove_dir(d).is_err() {
                    break;
                }
                dir = d.parent();
            }
        }
        let packed_path = self.git_dir.join("packed-refs");
        if let Ok(text) = fs::read_to_string(&packed_path) {
            let mut out = String::new();
            let mut skip_peeled = false;
            for line in text.lines() {
                if skip_peeled && line.starts_with('^') {
                    continue;
                }
                skip_peeled = line.split_once(' ').is_some_and(|(_, n)| n == name);
                if skip_peeled {
                    existed = true;
                } else {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            if existed {
                fs::write(&packed_path, out)?;
            }
        }
        Ok(existed)
    }

    /// All refs below `prefix` (e.g. `refs/heads/`), sorted by name.
    pub fn list_refs(&self, prefix: &str) -> Result<Vec<(String, Oid)>> {
        let mut found: BTreeMap<String, Oid> = self
            .packed_refs()
            .into_iter()
            .filter(|(n, _)| n.starts_with(prefix))
            .collect();
        let root = self.git_dir.join(prefix.trim_end_matches('/'));
        if root.is_dir() {
            self.collect_loose(&root, prefix.trim_end_matches('/'), &mut found)?;
        }
        Ok(found.into_iter().collect())
    }

    fn collect_loose(&self, dir: &Path, name: &str, out: &mut BTreeMap<String, Oid>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let full = format!("{name}/{file_name}");
            let path = entry.path();
            if path.is_dir() {
                self.collect_loose(&path, &full, out)?;
            } else if !file_name.ends_with(".lock") {
                if let Some(oid) = self.read_ref(&full)? {
                    out.insert(full, oid);
                }
            }
        }
        Ok(())
    }

    /// Point `HEAD` at a branch.
    pub fn set_head_branch(&self, branch: &str) -> Result<()> {
        self.write_locked(&self.git_dir.join("HEAD"), &format!("ref: refs/heads/{branch}\n"))
    }

    /// Detach `HEAD` at a commit.
    pub fn set_head_detached(&self, oid: &Oid) -> Result<()> {
        self.write_locked(&self.git_dir.join("HEAD"), &format!("{oid}\n"))
    }

    /// Advance whatever `HEAD` points at (the branch, or `HEAD` itself).
    pub fn update_head(&self, oid: &Oid) -> Result<()> {
        match self.read_head()? {
            Head::Branch(b) => self.update_ref(&format!("refs/heads/{b}"), oid),
            Head::Detached(_) => self.set_head_detached(oid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{hash_object, Kind};

    fn repo(tag: &str) -> (Repo, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("nanogit-refs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (Repo::init(&dir, "main").unwrap().0, dir)
    }

    fn oid(n: u8) -> Oid {
        hash_object(Kind::Blob, &[n])
    }

    #[test]
    fn ref_name_rules() {
        for ok in ["main", "feature/x", "a-b_c", "v1.0", "refs/heads/x"] {
            assert!(valid_ref_name(ok), "{ok}");
        }
        for bad in ["", "@", "/x", "x/", "a..b", "a b", "x.lock", "a/.hidden", "x.", "a~1", "a:b", "a//b", "a@{1}", "a*"] {
            assert!(!valid_ref_name(bad), "{bad}");
        }
    }

    #[test]
    fn head_branch_and_unborn() {
        let (repo, dir) = repo("head");
        assert_eq!(repo.read_head().unwrap(), Head::Branch("main".into()));
        assert_eq!(repo.head_oid().unwrap(), None);
        repo.update_head(&oid(1)).unwrap();
        assert_eq!(repo.head_oid().unwrap(), Some(oid(1)));
        assert_eq!(repo.read_ref("refs/heads/main").unwrap(), Some(oid(1)));
        repo.set_head_detached(&oid(2)).unwrap();
        assert_eq!(repo.read_head().unwrap(), Head::Detached(oid(2)));
        repo.update_head(&oid(3)).unwrap();
        assert_eq!(repo.head_oid().unwrap(), Some(oid(3)));
        repo.set_head_branch("topic/one").unwrap();
        assert_eq!(repo.read_head().unwrap(), Head::Branch("topic/one".into()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn list_delete_and_nested_refs() {
        let (repo, dir) = repo("list");
        repo.update_ref("refs/heads/a", &oid(1)).unwrap();
        repo.update_ref("refs/heads/feature/x", &oid(2)).unwrap();
        repo.update_ref("refs/tags/v1", &oid(3)).unwrap();
        let heads = repo.list_refs("refs/heads/").unwrap();
        let names: Vec<&str> = heads.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["refs/heads/a", "refs/heads/feature/x"]);
        assert!(repo.delete_ref("refs/heads/feature/x").unwrap());
        assert!(!repo.git_dir.join("refs/heads/feature").exists(), "empty dir pruned");
        assert!(!repo.delete_ref("refs/heads/feature/x").unwrap());
        assert_eq!(repo.read_ref("refs/heads/feature/x").unwrap(), None);
        assert!(repo.update_ref("refs/heads/bad name", &oid(1)).is_err());
        assert!(repo.update_ref("heads/nope", &oid(1)).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn packed_refs_are_read_and_deleted() {
        let (repo, dir) = repo("packed");
        let packed = format!(
            "# pack-refs with: peeled fully-peeled sorted\n{} refs/heads/packed\n{} refs/tags/t\n^{}\n",
            oid(1),
            oid(2),
            oid(3)
        );
        fs::write(repo.git_dir.join("packed-refs"), packed).unwrap();
        assert_eq!(repo.read_ref("refs/heads/packed").unwrap(), Some(oid(1)));
        repo.update_ref("refs/heads/packed", &oid(9)).unwrap();
        assert_eq!(repo.read_ref("refs/heads/packed").unwrap(), Some(oid(9)), "loose overrides packed");
        assert_eq!(repo.list_refs("refs/tags/").unwrap().len(), 1);
        assert!(repo.delete_ref("refs/tags/t").unwrap());
        let text = fs::read_to_string(repo.git_dir.join("packed-refs")).unwrap();
        assert!(!text.contains("refs/tags/t") && !text.contains('^'), "{text}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lock_files_block_concurrent_updates() {
        let (repo, dir) = repo("lock");
        fs::create_dir_all(repo.git_dir.join("refs/heads")).unwrap();
        fs::write(repo.git_dir.join("refs/heads/x.lock"), "").unwrap();
        let err = repo.update_ref("refs/heads/x", &oid(1)).unwrap_err();
        assert!(err.to_string().contains("File exists"));
        fs::remove_dir_all(dir).unwrap();
    }
}
