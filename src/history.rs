//! Walking commit history.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};

use crate::error::{Error, Result};
use crate::object::{Commit, Object, ObjectStore, Oid};

/// Read a commit, following annotated tags.
pub fn read_commit(odb: &ObjectStore, oid: &Oid) -> Result<Commit> {
    match odb.read(oid)? {
        Object::Commit(c) => Ok(c),
        Object::Tag(t) => read_commit(odb, &t.object),
        _ => Err(Error::fatal(format!("{oid} is not a commit"))),
    }
}

/// The commit id a (possibly tag) object ultimately refers to.
pub fn peel_to_commit(odb: &ObjectStore, oid: &Oid) -> Result<Oid> {
    match odb.read(oid)? {
        Object::Commit(_) => Ok(*oid),
        Object::Tag(t) => peel_to_commit(odb, &t.object),
        _ => Err(Error::fatal(format!("{oid} is not a commit"))),
    }
}

/// Commits reachable from `starts`, newest committer date first (ties keep
/// discovery order), like `git log`.
pub fn log(odb: &ObjectStore, starts: &[Oid], limit: Option<usize>) -> Result<Vec<(Oid, Commit)>> {
    let mut heap = BinaryHeap::new();
    let mut seen = HashSet::new();
    let mut seq = 0usize;
    let mut cache = Vec::new();
    for start in starts {
        let oid = peel_to_commit(odb, start)?;
        if seen.insert(oid) {
            let c = read_commit(odb, &oid)?;
            heap.push((c.committer.time, Reverse(seq), cache.len()));
            cache.push((oid, c));
            seq += 1;
        }
    }
    let mut out = Vec::new();
    while let Some((_, _, slot)) = heap.pop() {
        if limit.is_some_and(|n| out.len() >= n) {
            break;
        }
        let (oid, commit) = cache[slot].clone();
        for parent in &commit.parents {
            if seen.insert(*parent) {
                let c = read_commit(odb, parent)?;
                heap.push((c.committer.time, Reverse(seq), cache.len()));
                cache.push((*parent, c));
                seq += 1;
            }
        }
        out.push((oid, commit));
    }
    Ok(out)
}

/// True if `ancestor` is reachable from `descendant` (a commit is its own ancestor).
pub fn is_ancestor(odb: &ObjectStore, ancestor: &Oid, descendant: &Oid) -> Result<bool> {
    let mut seen = HashSet::new();
    let mut stack = vec![*descendant];
    while let Some(oid) = stack.pop() {
        if oid == *ancestor {
            return Ok(true);
        }
        if seen.insert(oid) {
            stack.extend(read_commit(odb, &oid)?.parents);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Signature;

    fn make(odb: &ObjectStore, parents: &[Oid], time: i64, msg: &str) -> Oid {
        let sig = Signature { name: "t".into(), email: "t@t".into(), time, tz: "+0000".into() };
        let tree = odb.write(crate::object::Kind::Tree, b"").unwrap();
        odb.write_object(&Object::Commit(Commit {
            tree,
            parents: parents.to_vec(),
            author: sig.clone(),
            committer: sig,
            extra: vec![],
            message: format!("{msg}\n"),
        }))
        .unwrap()
    }

    fn store(tag: &str) -> ObjectStore {
        let dir = std::env::temp_dir().join(format!("nanogit-hist-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ObjectStore::new(dir)
    }

    #[test]
    fn log_orders_by_date_across_merges() {
        let odb = store("log");
        let root = make(&odb, &[], 100, "root");
        let a = make(&odb, &[root], 200, "a");
        let b = make(&odb, &[root], 300, "b");
        let merge = make(&odb, &[a, b], 400, "merge");
        let order: Vec<String> =
            log(&odb, &[merge], None).unwrap().iter().map(|(_, c)| c.subject().to_string()).collect();
        assert_eq!(order, ["merge", "b", "a", "root"]);
        assert_eq!(log(&odb, &[merge], Some(2)).unwrap().len(), 2);
        std::fs::remove_dir_all(odb.dir()).unwrap();
    }

    #[test]
    fn ancestry() {
        let odb = store("anc");
        let root = make(&odb, &[], 1, "root");
        let a = make(&odb, &[root], 2, "a");
        let b = make(&odb, &[root], 3, "b");
        assert!(is_ancestor(&odb, &root, &a).unwrap());
        assert!(is_ancestor(&odb, &a, &a).unwrap());
        assert!(!is_ancestor(&odb, &a, &b).unwrap());
        assert!(!is_ancestor(&odb, &b, &root).unwrap());
        std::fs::remove_dir_all(odb.dir()).unwrap();
    }

    #[test]
    fn non_commits_are_rejected() {
        let odb = store("bad");
        let blob = odb.write(crate::object::Kind::Blob, b"x").unwrap();
        assert!(read_commit(&odb, &blob).is_err());
        assert!(peel_to_commit(&odb, &blob).is_err());
        std::fs::remove_dir_all(odb.dir()).unwrap();
    }
}
