//! Repositories whose objects live in packfiles (after `git gc`/`git repack`).

mod common;

use std::fs;

use common::{assert_all_objects_match, git, history, loose_count, make_writable, ng, pack_count};

#[test]
fn reads_everything_after_git_gc() {
    require_git!();
    let t = history("pack-gc");
    git(t.path(), &["gc", "-q", "--aggressive", "--prune=now"]).ok();
    assert_eq!(loose_count(&t), 0, "gc should leave no loose objects");
    assert_eq!(pack_count(&t), 1);
    assert!(t.exists(".git/packed-refs"));
    let idx = fs::read_dir(t.path().join(".git/objects/pack"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .unwrap();
    let stat = git(t.path(), &["verify-pack", "-v", idx.to_str().unwrap()]).ok();
    assert!(stat.contains("chain length = 2"), "expected delta chains in the pack:\n{stat}");
    assert_all_objects_match(&t);

    assert_eq!(ng(t.path(), &["log", "--oneline", "--all"]).ok(), git(t.path(), &["log", "--oneline", "--all"]).ok());
    assert_eq!(ng(t.path(), &["status", "--porcelain"]).ok(), "");
    assert_eq!(
        ng(t.path(), &["diff", "HEAD~4", "HEAD"]).ok(),
        git(t.path(), &["diff", "--no-renames", "HEAD~4", "HEAD"]).ok()
    );
    assert_eq!(ng(t.path(), &["rev-parse", "v1"]).ok(), git(t.path(), &["rev-parse", "v1"]).ok());
}

#[test]
fn short_ids_resolve_inside_packs() {
    require_git!();
    let t = history("pack-prefix");
    git(t.path(), &["gc", "-q", "--prune=now"]).ok();
    let full = git(t.path(), &["rev-parse", "HEAD~2"]).ok();
    let short = &full[..9];
    assert_eq!(ng(t.path(), &["rev-parse", short]).ok(), full);
    assert_eq!(ng(t.path(), &["cat-file", "-t", short]).ok(), "commit\n");
    let missing = ng(t.path(), &["cat-file", "-e", "0000000000000000000000000000000000000001"]);
    assert_eq!(missing.code, 1);
}

#[test]
fn ref_deltas_and_index_v1() {
    require_git!();
    for (tag, args) in [
        ("pack-ref-delta", vec!["-c", "repack.useDeltaBaseOffset=false", "repack", "-q", "-a", "-d", "-f"]),
        ("pack-idx-v1", vec!["-c", "pack.indexVersion=1", "repack", "-q", "-a", "-d", "-f"]),
    ] {
        let t = history(tag);
        git(t.path(), &args).ok();
        git(t.path(), &["prune-packed"]).ok();
        git(t.path(), &["prune", "--expire=now"]).ok();
        assert_eq!(loose_count(&t), 0, "{tag}");
        assert_all_objects_match(&t);
    }
}

#[test]
fn mixes_loose_objects_and_several_packs() {
    require_git!();
    let t = history("pack-mixed");
    git(t.path(), &["repack", "-q", "-a", "-d"]).ok();
    git(t.path(), &["prune-packed"]).ok();
    // A second, incremental pack on top of the first.
    t.write("story.txt", "rewritten by a later commit\n");
    git(t.path(), &["commit", "-q", "-am", "later"]).ok();
    git(t.path(), &["repack", "-q", "-d"]).ok();
    git(t.path(), &["prune-packed"]).ok();
    assert_eq!(pack_count(&t), 2);

    // nanogit adds loose objects on top of the packed history.
    ng(t.path(), &["checkout", "side"]).ok();
    assert_eq!(fs::read_to_string(t.path().join("src/lib.rs")).unwrap().lines().next().unwrap(), "pub const ROUND: u32 = 4;");
    t.write("side.txt", "work on the side branch\n");
    ng(t.path(), &["add", "side.txt"]).ok();
    ng(t.path(), &["commit", "-m", "side work"]).ok();
    ng(t.path(), &["checkout", "main"]).ok();
    let merge = ng(t.path(), &["merge", "side", "-m", "merge side"]);
    assert_eq!(merge.code, 0, "{}{}", merge.stdout, merge.stderr);
    assert!(t.exists("side.txt"));
    assert_eq!(ng(t.path(), &["status", "--porcelain"]).ok(), "");
    assert_all_objects_match(&t);
    let fsck = git(t.path(), &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(fsck.code, 0, "fsck: {}{}", fsck.stdout, fsck.stderr);
}

#[test]
fn corrupt_packs_fail_cleanly() {
    require_git!();
    let t = history("pack-corrupt");
    git(t.path(), &["gc", "-q", "--prune=now"]).ok();
    let pack_dir = t.path().join(".git/objects/pack");
    let pack = fs::read_dir(&pack_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .unwrap();
    let mut bytes = fs::read(&pack).unwrap();
    // Damage the middle of the pack, leaving header and trailer intact.
    let mid = bytes.len() / 2;
    for b in &mut bytes[mid..mid + 64] {
        *b ^= 0x5a;
    }
    make_writable(&pack);
    fs::write(&pack, &bytes).unwrap();
    // Some object is now unreadable; nanogit must report it, not panic.
    let objects = String::from_utf8(
        std::process::Command::new("git")
            .args(["show-index"])
            .stdin(fs::File::open(pack.with_extension("idx")).unwrap())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let mut failures = 0;
    for line in objects.lines() {
        let oid = line.split(' ').nth(1).unwrap();
        let r = ng(t.path(), &["cat-file", "-p", oid]);
        assert!(!r.stderr.contains("panicked"), "panic reading {oid}: {}", r.stderr);
        if r.code != 0 {
            failures += 1;
        }
    }
    assert!(failures > 0, "damaging the pack should break at least one object");

    // A truncated index is rejected as a whole.
    let idx = pack.with_extension("idx");
    let data = fs::read(&idx).unwrap();
    make_writable(&idx);
    fs::write(&idx, &data[..data.len() - 7]).unwrap();
    let r = ng(t.path(), &["log", "--oneline"]);
    assert_ne!(r.code, 0);
    assert!(r.stderr.contains("pack index"), "{}", r.stderr);
}
