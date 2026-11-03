//! Repository layout: creating, locating and opening a `.git` directory.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::object::ObjectStore;

/// An opened repository.
#[derive(Clone, Debug)]
pub struct Repo {
    /// Root of the working tree.
    pub work_tree: PathBuf,
    /// The `.git` directory.
    pub git_dir: PathBuf,
}

impl Repo {
    /// Create (or re-initialise) a repository in `dir`.
    ///
    /// Returns the repository and whether one already existed.
    pub fn init(dir: &Path, initial_branch: &str) -> Result<(Repo, bool)> {
        let git_dir = dir.join(".git");
        let existed = git_dir.join("HEAD").is_file();
        for sub in ["objects", "refs/heads", "refs/tags", "info"] {
            fs::create_dir_all(git_dir.join(sub))?;
        }
        if !existed {
            fs::write(git_dir.join("HEAD"), format!("ref: refs/heads/{initial_branch}\n"))?;
            fs::write(
                git_dir.join("config"),
                "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = false\n",
            )?;
            fs::write(
                git_dir.join("description"),
                "Unnamed repository; edit this file 'description' to name the repository.\n",
            )?;
        }
        let work_tree = fs::canonicalize(dir)?;
        Ok((Repo { git_dir: work_tree.join(".git"), work_tree }, existed))
    }

    /// Open the repository whose working tree is exactly `work_tree`.
    pub fn open(work_tree: &Path) -> Result<Repo> {
        let git_dir = work_tree.join(".git");
        if !git_dir.join("HEAD").is_file() {
            return Err(Error::fatal("not a nanogit repository (no .git/HEAD)"));
        }
        Ok(Repo { work_tree: work_tree.to_path_buf(), git_dir })
    }

    /// Find the repository containing `start` by walking up to the root.
    pub fn discover(start: &Path) -> Result<Repo> {
        let start = fs::canonicalize(start)?;
        let mut dir: &Path = &start;
        loop {
            if dir.join(".git").join("HEAD").is_file() {
                return Repo::open(dir);
            }
            dir = dir
                .parent()
                .ok_or_else(|| Error::fatal("not a git repository (or any of the parent directories): .git"))?;
        }
    }

    /// The loose object store.
    pub fn odb(&self) -> ObjectStore {
        ObjectStore::new(self.git_dir.join("objects"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nanogit-repo-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn init_creates_layout_and_is_idempotent() {
        let dir = temp("init");
        let (repo, existed) = Repo::init(&dir, "main").unwrap();
        assert!(!existed);
        assert_eq!(fs::read_to_string(repo.git_dir.join("HEAD")).unwrap(), "ref: refs/heads/main\n");
        assert!(repo.git_dir.join("objects").is_dir());
        assert!(repo.git_dir.join("refs/heads").is_dir());
        fs::write(repo.git_dir.join("HEAD"), "ref: refs/heads/other\n").unwrap();
        let (_, existed) = Repo::init(&dir, "main").unwrap();
        assert!(existed);
        assert_eq!(fs::read_to_string(repo.git_dir.join("HEAD")).unwrap(), "ref: refs/heads/other\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn discover_walks_up() {
        let dir = temp("discover");
        let (repo, _) = Repo::init(&dir, "main").unwrap();
        let nested = dir.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let found = Repo::discover(&nested).unwrap();
        assert_eq!(found.work_tree, repo.work_tree);
        let outside = temp("discover-outside");
        // A temp dir may itself live inside a repository; only assert on error shape.
        if let Err(e) = Repo::discover(&outside) {
            assert!(e.to_string().contains("not a git repository"));
        }
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }
}
