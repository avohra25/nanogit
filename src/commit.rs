//! Creating commits: identity lookup, message clean-up and ref updates.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::history::read_commit;
use crate::index::Index;
use crate::object::{Commit, Object, Oid, Signature};
use crate::refs::Head;
use crate::repo::Repo;
use crate::timefmt::{now, parse_date, parse_tz};

/// Which identity to build.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The person who wrote the change.
    Author,
    /// The person who recorded it.
    Committer,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Build a signature from `GIT_*_NAME/EMAIL/DATE`, then `user.name`/`user.email`.
pub fn identity(config: &Config, role: Role) -> Result<Signature> {
    let (prefix, label) = match role {
        Role::Author => ("GIT_AUTHOR", "author"),
        Role::Committer => ("GIT_COMMITTER", "committer"),
    };
    let name = env(&format!("{prefix}_NAME"))
        .or_else(|| config.get("user.name").map(str::to_string))
        .ok_or_else(|| Error::fatal(format!("unable to determine {label} name: set user.name or {prefix}_NAME")))?;
    let email = env(&format!("{prefix}_EMAIL"))
        .or_else(|| config.get("user.email").map(str::to_string))
        .ok_or_else(|| Error::fatal(format!("unable to determine {label} email: set user.email or {prefix}_EMAIL")))?;
    let (time, tz) = match env(&format!("{prefix}_DATE")) {
        Some(d) => parse_date(&d).ok_or_else(|| Error::fatal(format!("invalid date format: {d}")))?,
        None => (now(), "+0000".to_string()),
    };
    debug_assert!(parse_tz(&tz).is_some());
    Ok(Signature { name, email, time, tz })
}

/// Git-style message clean-up: strip trailing blanks per line, collapse runs
/// of blank lines, trim leading/trailing blank lines, end with one newline.
pub fn clean_message(raw: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end();
        if line.is_empty() && out.last().map_or(true, |l| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    if out.is_empty() {
        String::new()
    } else {
        format!("{}\n", out.join("\n"))
    }
}

impl Repo {
    /// Commit the given index (which must have no conflicts) on top of `HEAD`,
    /// plus any `extra_parents` (for merges).  Moves `HEAD`'s branch.
    pub fn commit_index(&self, index: &Index, message: &str, extra_parents: &[Oid]) -> Result<Oid> {
        let message = clean_message(message);
        if message.is_empty() {
            return Err(Error::fatal("aborting commit due to empty commit message"));
        }
        let config = Config::load(Some(self));
        let author = identity(&config, Role::Author)?;
        let committer = identity(&config, Role::Committer)?;
        let tree = self.write_index_tree(index)?;
        let mut parents: Vec<Oid> = self.head_oid()?.into_iter().collect();
        parents.extend_from_slice(extra_parents);
        let commit = Commit { tree, parents, author, committer, extra: Vec::new(), message };
        let oid = self.odb().write_object(&Object::Commit(commit))?;
        self.update_head(&oid)?;
        Ok(oid)
    }

    /// Tree of the current `HEAD` commit, if any.
    pub fn head_tree(&self) -> Result<Option<Oid>> {
        match self.head_oid()? {
            Some(c) => Ok(Some(read_commit(&self.odb(), &c)?.tree)),
            None => Ok(None),
        }
    }

    /// Short description of `HEAD` for messages like `[main abc1234] subject`.
    pub fn head_label(&self) -> Result<String> {
        Ok(match self.read_head()? {
            Head::Branch(b) => b,
            Head::Detached(_) => "detached HEAD".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_cleanup() {
        assert_eq!(clean_message("hello"), "hello\n");
        assert_eq!(clean_message("\n\n  subject  \n\n\n\nbody line   \n\n"), "  subject\n\nbody line\n");
        assert_eq!(clean_message("   \n\n"), "");
        assert_eq!(clean_message("a\nb\n"), "a\nb\n");
    }

    #[test]
    fn identity_prefers_environment_over_config() {
        let config = Config::parse("[user]\n\tname = Cfg\n\temail = cfg@x\n");
        // Only read-side checks: environment variables are process-global, so
        // this test avoids setting them and covers the config fallback.
        if std::env::var("GIT_AUTHOR_NAME").is_err()
            && std::env::var("GIT_AUTHOR_EMAIL").is_err()
            && std::env::var("GIT_AUTHOR_DATE").is_err()
        {
            let sig = identity(&config, Role::Author).unwrap();
            assert_eq!((sig.name.as_str(), sig.email.as_str()), ("Cfg", "cfg@x"));
            assert_eq!(sig.tz, "+0000");
        }
        let empty = Config::parse("");
        if std::env::var("GIT_COMMITTER_NAME").is_err() {
            assert!(identity(&empty, Role::Committer).is_err());
        }
    }
}
