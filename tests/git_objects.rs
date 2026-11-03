//! Cross-checks of the object store against real git (skipped without git).

mod common;

use common::{git, git_stdin, ng, ng_env, TempDir};
use nanogit::prng::Rng;

#[test]
fn nanogit_objects_are_readable_by_git() {
    require_git!();
    let t = TempDir::new("obj-ng2git");
    ng(t.path(), &["init"]).ok();
    t.write("hello.txt", "hello\n");
    t.write("big.txt", "the quick brown fox jumps over the lazy dog\n".repeat(500));
    let small = ng(t.path(), &["hash-object", "-w", "hello.txt"]).ok();
    let big = ng(t.path(), &["hash-object", "-w", "big.txt"]).ok();
    assert_eq!(git(t.path(), &["cat-file", "-p", small.trim()]).ok(), "hello\n");
    assert_eq!(git(t.path(), &["cat-file", "-p", big.trim()]).ok(), t.read("big.txt"));
    assert_eq!(git(t.path(), &["cat-file", "-t", small.trim()]).ok(), "blob\n");
    // git's own consistency check accepts what we wrote.
    let fsck = git(t.path(), &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(fsck.code, 0, "fsck: {}{}", fsck.stdout, fsck.stderr);
}

#[test]
fn git_objects_are_readable_by_nanogit() {
    require_git!();
    let t = TempDir::new("obj-git2ng");
    git(t.path(), &["init", "-q", "-b", "main"]).ok();
    // Repetitive text makes real zlib emit dynamic Huffman blocks.
    let mut rng = Rng::new(3);
    let mut text = String::new();
    for _ in 0..2000 {
        let words = ["alpha", "beta", "gamma", "delta", "epsilon"];
        text.push_str(*rng.pick(&words));
        text.push(' ');
    }
    let noise = rng.bytes(3000);
    for (name, data) in [("text", text.into_bytes()), ("noise", noise), ("empty", Vec::new())] {
        let oid = git_stdin(t.path(), &["hash-object", "-w", "--stdin"], &data).ok();
        let oid = oid.trim();
        let size = ng(t.path(), &["cat-file", "-s", oid]).ok();
        assert_eq!(size.trim(), data.len().to_string(), "{name}");
        let out = ng_env(t.path(), &["cat-file", "blob", oid], &[], b"");
        assert_eq!(out.code, 0, "{name}");
        assert_eq!(out.raw, data, "{name}");
    }
}

#[test]
fn trees_and_commits_made_by_git_parse() {
    require_git!();
    let t = TempDir::new("obj-tree");
    git(t.path(), &["init", "-q", "-b", "main"]).ok();
    t.write("a.txt", "one\n");
    t.write("dir/b.txt", "two\n");
    t.write("dir/sub/c.sh", "#!/bin/sh\n");
    git(t.path(), &["add", "."]).ok();
    git(t.path(), &["commit", "-q", "-m", "first\n\nbody"]).ok();
    let head = git(t.path(), &["rev-parse", "HEAD"]).ok();
    let head = head.trim();
    let commit = ng(t.path(), &["cat-file", "-p", head]).ok();
    assert!(commit.contains("author Test Author <author@example.com> "), "{commit}");
    assert!(commit.ends_with("first\n\nbody\n"), "{commit}");
    let tree_line = commit.lines().next().unwrap();
    let tree = tree_line.strip_prefix("tree ").unwrap();
    let listing = ng(t.path(), &["cat-file", "-p", tree]).ok();
    let expected = git(t.path(), &["ls-tree", "HEAD"]).ok();
    assert_eq!(listing, expected);
}

#[test]
fn hash_object_matches_git_on_random_inputs() {
    require_git!();
    let t = TempDir::new("obj-hash");
    for seed in 0..25u64 {
        let mut rng = Rng::new(seed + 1000);
        let len = if seed == 0 { 0 } else { rng.below(3000) as usize };
        let data = if seed % 2 == 0 { rng.bytes(len) } else { rng.text(len, 5) };
        let ours = ng_env(t.path(), &["hash-object", "--stdin"], &[], &data).ok();
        let theirs = git_stdin(t.path(), &["hash-object", "--stdin"], &data).ok();
        assert_eq!(ours, theirs, "seed {seed}");
    }
}

#[test]
fn hash_object_types_match_git() {
    require_git!();
    let t = TempDir::new("obj-types");
    t.write("f", "tree-ish junk\n");
    let ours = ng(t.path(), &["hash-object", "-t", "blob", "f"]).ok();
    let theirs = git(t.path(), &["hash-object", "-t", "blob", "f"]).ok();
    assert_eq!(ours, theirs);
}
