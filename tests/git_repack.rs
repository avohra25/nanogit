//! Packs and packed refs written by nanogit, checked with real git.

mod common;

use std::fs;

use common::{assert_all_objects_match, copy_dir, git, history, loose_count, ng, pack_count, TempDir};

fn only_idx(t: &TempDir) -> String {
    fs::read_dir(t.path().join(".git/objects/pack"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn gc_output_is_accepted_by_git() {
    require_git!();
    let t = history("rp-gc");
    let pristine = TempDir::new("rp-gc-pristine");
    copy_dir(t.path(), pristine.path());
    let out = ng(t.path(), &["gc"]).ok();
    assert!(out.starts_with("Total "), "{out}");
    assert_eq!(loose_count(&t), 0);
    assert_eq!(pack_count(&t), 1);
    assert!(t.exists(".git/packed-refs"));
    assert!(!t.exists(".git/refs/heads/main"));

    let stat = git(t.path(), &["verify-pack", "-v", &only_idx(&t)]).ok();
    assert!(stat.contains("chain length = 1"), "nanogit should have written deltas:\n{stat}");
    let fsck = git(t.path(), &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(fsck.code, 0, "fsck: {}{}", fsck.stdout, fsck.stderr);

    // Same objects and refs as before the gc, read by git from our pack.
    let list = git(pristine.path(), &["rev-list", "--objects", "--all"]).ok();
    for line in list.lines() {
        let oid = line.split(' ').next().unwrap();
        let kind = git(pristine.path(), &["cat-file", "-t", oid]).ok();
        assert_eq!(
            git(t.path(), &["cat-file", kind.trim(), oid]).raw,
            git(pristine.path(), &["cat-file", kind.trim(), oid]).raw,
            "{oid}"
        );
    }
    assert_eq!(git(t.path(), &["show-ref", "-d"]).ok(), git(pristine.path(), &["show-ref", "-d"]).ok());
    assert_all_objects_match(&t);
    assert_eq!(ng(t.path(), &["status", "--porcelain"]).ok(), "");
}

#[test]
fn packed_refs_match_git_byte_for_byte() {
    require_git!();
    let t = history("rp-refs");
    git(t.path(), &["tag", "light", "HEAD~2"]).ok();
    git(t.path(), &["tag", "-a", "nested", "-m", "tag of a tag", "v1"]).ok();
    let theirs = TempDir::new("rp-refs-git");
    copy_dir(t.path(), theirs.path());
    git(theirs.path(), &["pack-refs", "--all"]).ok();
    ng(t.path(), &["pack-refs", "--all"]).ok();
    assert_eq!(t.read(".git/packed-refs"), theirs.read(".git/packed-refs"));
    assert_eq!(git(t.path(), &["show-ref", "-d"]).ok(), git(theirs.path(), &["show-ref", "-d"]).ok());

    // Tags only: branches stay loose.
    let u = history("rp-refs-tags");
    ng(u.path(), &["pack-refs"]).ok();
    assert!(u.exists(".git/refs/heads/main"));
    assert!(!u.exists(".git/refs/tags/v1"));
    assert!(u.read(".git/packed-refs").contains(" refs/tags/v1\n^"));
}

#[test]
fn packed_branches_keep_working() {
    require_git!();
    let t = history("rp-branches");
    ng(t.path(), &["gc", "-q"]).ok();
    // Commit on a packed branch: the loose ref now shadows the packed one.
    t.write("new.txt", "after gc\n");
    ng(t.path(), &["add", "new.txt"]).ok();
    ng(t.path(), &["commit", "-m", "after gc"]).ok();
    assert_eq!(ng(t.path(), &["rev-parse", "main"]).ok(), git(t.path(), &["rev-parse", "main"]).ok());
    assert_eq!(git(t.path(), &["log", "-1", "--format=%s"]).ok(), "after gc\n");
    // Delete a branch that only exists in packed-refs.
    ng(t.path(), &["branch", "-D", "side"]).ok();
    assert!(!t.read(".git/packed-refs").contains("refs/heads/side"));
    assert_eq!(git(t.path(), &["branch", "--list", "side"]).ok(), "");
    // A second gc folds the new loose objects into a fresh single pack.
    ng(t.path(), &["gc", "-q"]).ok();
    assert_eq!(loose_count(&t), 0);
    assert_eq!(pack_count(&t), 1);
    let fsck = git(t.path(), &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(fsck.code, 0, "fsck: {}{}", fsck.stdout, fsck.stderr);
}

#[test]
fn repack_never_drops_objects() {
    require_git!();
    let t = TempDir::new("rp-keep");
    ng(t.path(), &["init"]).ok();
    t.write("a.txt", "a\n");
    ng(t.path(), &["add", "a.txt"]).ok();
    ng(t.path(), &["commit", "-m", "one"]).ok();
    // A committed blob written again, an unreachable loose blob and a
    // staged-but-uncommitted blob.
    let committed = ng(t.path(), &["hash-object", "-w", "a.txt"]).ok();
    fs::write(t.path().join("loose.txt"), "nobody refers to me\n").unwrap();
    let orphan = ng(t.path(), &["hash-object", "-w", "loose.txt"]).ok();
    fs::remove_file(t.path().join("loose.txt")).unwrap();
    t.write("staged.txt", "staged only\n");
    ng(t.path(), &["add", "staged.txt"]).ok();
    let staged = git(t.path(), &["rev-parse", ":staged.txt"]).ok();

    let first = ng(t.path(), &["repack", "-d"]).ok();
    assert_eq!(pack_count(&t), 1);
    // The orphan stays loose; everything reachable (and the index) is packed.
    assert_eq!(loose_count(&t), 1);
    assert_eq!(ng(t.path(), &["cat-file", "-p", orphan.trim()]).ok(), "nobody refers to me\n");
    assert_eq!(ng(t.path(), &["cat-file", "-p", staged.trim()]).ok(), "staged only\n");
    assert_eq!(ng(t.path(), &["cat-file", "-t", committed.trim()]).ok(), "blob\n");
    // Repacking again is a no-op that produces the same pack.
    let second = ng(t.path(), &["repack", "-d"]).ok();
    assert_eq!(first.lines().next(), second.lines().next());
    assert_eq!(pack_count(&t), 1);
    ng(t.path(), &["commit", "-m", "two"]).ok();
    assert_eq!(git(t.path(), &["log", "--format=%s"]).ok(), "two\none\n");
    // Clone it with git to be sure the pack and refs are usable remotely.
    let c = TempDir::new("rp-keep-clone");
    let url = format!("file://{}", t.path().display());
    git(c.path(), &["clone", "-q", &url, "copy"]).ok();
    assert_eq!(fs::read_to_string(c.path().join("copy/staged.txt")).unwrap(), "staged only\n");
}

#[test]
fn repack_options_are_validated() {
    require_git!();
    let t = history("rp-opts");
    let r = ng(t.path(), &["repack", "--window=abc"]);
    assert_eq!(r.code, 128);
    assert!(r.stderr.contains("expects a number"), "{}", r.stderr);
    assert_eq!(ng(t.path(), &["repack", "--bogus"]).code, 129);
    // --window=0 disables deltas entirely.
    ng(t.path(), &["repack", "-d", "--window=0"]).ok();
    let stat = git(t.path(), &["verify-pack", "-v", &only_idx(&t)]).ok();
    assert!(!stat.contains("chain length"), "{stat}");
    ng(t.path(), &["repack", "-d", "--depth", "1"]).ok();
    let stat = git(t.path(), &["verify-pack", "-v", &only_idx(&t)]).ok();
    assert!(stat.contains("chain length = 1") && !stat.contains("chain length = 2"), "{stat}");
}
