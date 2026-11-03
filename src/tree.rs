//! Helpers for moving between tree objects and flat `path -> entry` maps.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::object::{mode, Object, ObjectStore, Oid, TreeEntry};

/// A leaf (file, symlink or submodule) of a flattened tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlatEntry {
    /// Git file mode.
    pub mode: u32,
    /// Blob (or gitlink commit) id.
    pub oid: Oid,
}

/// Every leaf of a tree keyed by its full slash-separated path.
pub type Flat = BTreeMap<String, FlatEntry>;

/// Recursively list all files under `tree`.
pub fn flatten(odb: &ObjectStore, tree: &Oid) -> Result<Flat> {
    let mut out = Flat::new();
    flatten_into(odb, tree, "", &mut out)?;
    Ok(out)
}

fn flatten_into(odb: &ObjectStore, tree: &Oid, prefix: &str, out: &mut Flat) -> Result<()> {
    let Object::Tree(entries) = odb.read(tree)? else {
        return Err(Error::fatal(format!("{tree} is not a tree")));
    };
    for e in entries {
        let path = format!("{prefix}{}", e.name);
        if mode::is_tree(e.mode) {
            flatten_into(odb, &e.oid, &format!("{path}/"), out)?;
        } else {
            out.insert(path, FlatEntry { mode: e.mode, oid: e.oid });
        }
    }
    Ok(())
}

enum Node {
    Leaf(FlatEntry),
    Dir(BTreeMap<String, Node>),
}

fn insert(dir: &mut BTreeMap<String, Node>, parts: &[&str], leaf: FlatEntry, full: &str) -> Result<()> {
    match parts {
        [] => Err(Error::fatal("empty path in tree")),
        [name] => {
            if matches!(dir.get(*name), Some(Node::Dir(_))) {
                return Err(Error::fatal(format!("'{full}' is both a file and a directory")));
            }
            dir.insert((*name).to_string(), Node::Leaf(leaf));
            Ok(())
        }
        [name, rest @ ..] => {
            let child = dir
                .entry((*name).to_string())
                .or_insert_with(|| Node::Dir(BTreeMap::new()));
            match child {
                Node::Dir(sub) => insert(sub, rest, leaf, full),
                Node::Leaf(_) => Err(Error::fatal(format!("'{full}' is both a file and a directory"))),
            }
        }
    }
}

fn write_dir(odb: &ObjectStore, dir: &BTreeMap<String, Node>) -> Result<Oid> {
    let mut entries = Vec::with_capacity(dir.len());
    for (name, node) in dir {
        entries.push(match node {
            Node::Leaf(f) => TreeEntry { mode: f.mode, name: name.clone(), oid: f.oid },
            Node::Dir(sub) => TreeEntry { mode: mode::TREE, name: name.clone(), oid: write_dir(odb, sub)? },
        });
    }
    odb.write_object(&Object::Tree(entries))
}

/// Build (and store) the tree objects for a flat file listing; returns the root.
pub fn write_tree<'a>(
    odb: &ObjectStore,
    files: impl IntoIterator<Item = (&'a str, FlatEntry)>,
) -> Result<Oid> {
    let mut root = BTreeMap::new();
    for (path, leaf) in files {
        let parts: Vec<&str> = path.split('/').collect();
        insert(&mut root, &parts, leaf, path)?;
    }
    write_dir(odb, &root)
}

/// Look up one path inside a tree; returns its mode and id.
pub fn lookup(odb: &ObjectStore, tree: &Oid, path: &str) -> Result<Option<(u32, Oid)>> {
    let mut current = *tree;
    let mut parts = path.split('/').filter(|p| !p.is_empty()).peekable();
    if parts.peek().is_none() {
        return Ok(Some((mode::TREE, current)));
    }
    while let Some(part) = parts.next() {
        let Object::Tree(entries) = odb.read(&current)? else {
            return Ok(None);
        };
        let Some(e) = entries.into_iter().find(|e| e.name == part) else {
            return Ok(None);
        };
        if parts.peek().is_none() {
            return Ok(Some((e.mode, e.oid)));
        }
        if !mode::is_tree(e.mode) {
            return Ok(None);
        }
        current = e.oid;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Kind;

    fn store(tag: &str) -> ObjectStore {
        let dir = std::env::temp_dir().join(format!("nanogit-tree-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ObjectStore::new(dir)
    }

    fn leaf(odb: &ObjectStore, data: &str) -> FlatEntry {
        FlatEntry { mode: mode::FILE, oid: odb.write(Kind::Blob, data.as_bytes()).unwrap() }
    }

    #[test]
    fn write_flatten_round_trip() {
        let odb = store("rt");
        let files = [
            ("README", leaf(&odb, "r")),
            ("src/main.rs", leaf(&odb, "m")),
            ("src/lib/mod.rs", leaf(&odb, "l")),
            ("a.b", leaf(&odb, "ab")),
            ("a/b", leaf(&odb, "a/b")),
        ];
        let root = write_tree(&odb, files.iter().map(|(p, f)| (*p, *f))).unwrap();
        let flat = flatten(&odb, &root).unwrap();
        assert_eq!(flat.len(), files.len());
        for (p, f) in &files {
            assert_eq!(flat[*p], *f, "{p}");
        }
        assert_eq!(lookup(&odb, &root, "src/lib/mod.rs").unwrap().unwrap().1, files[2].1.oid);
        assert!(lookup(&odb, &root, "src/nope").unwrap().is_none());
        assert!(lookup(&odb, &root, "README/x").unwrap().is_none());
        assert_eq!(lookup(&odb, &root, "").unwrap().unwrap().1, root);
        std::fs::remove_dir_all(odb.dir()).unwrap();
    }

    #[test]
    fn empty_tree_has_the_well_known_id() {
        let odb = store("empty");
        let root = write_tree(&odb, std::iter::empty()).unwrap();
        assert_eq!(root.to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
        std::fs::remove_dir_all(odb.dir()).unwrap();
    }

    #[test]
    fn file_directory_clash_is_an_error() {
        let odb = store("clash");
        let f = leaf(&odb, "x");
        assert!(write_tree(&odb, [("a", f), ("a/b", f)]).is_err());
        assert!(write_tree(&odb, [("a/b", f), ("a", f)]).is_err());
        std::fs::remove_dir_all(odb.dir()).unwrap();
    }
}
