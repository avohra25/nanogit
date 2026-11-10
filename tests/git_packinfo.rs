//! `verify-pack` and `count-objects` output compared with git's.

mod common;

use std::fs;

use common::{git, history, make_writable, ng, TempDir};

fn idx_of(t: &TempDir) -> String {
    let mut idx: Vec<_> = fs::read_dir(t.path().join(".git/objects/pack"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "idx"))
        .collect();
    assert_eq!(idx.len(), 1);
    let rel = idx.pop().unwrap();
    rel.strip_prefix(t.path()).unwrap().to_string_lossy().into_owned()
}

fn same_verify_output(t: &TempDir, what: &str) {
    let idx = idx_of(t);
    for flags in [&["-v"][..], &["-s"][..], &[][..]] {
        let mut args = vec!["verify-pack"];
        args.extend_from_slice(flags);
        args.push(&idx);
        let ours = ng(t.path(), &args).ok();
        let theirs = git(t.path(), &args).ok();
        assert_eq!(ours, theirs, "{what}: verify-pack {flags:?}");
    }
    // Also accepted: the .pack name, or no extension at all.
    let pack = idx.replace(".idx", ".pack");
    assert_eq!(ng(t.path(), &["verify-pack", "-v", &pack]).ok(), git(t.path(), &["verify-pack", "-v", &pack]).ok());
}

#[test]
fn verify_pack_matches_git_for_git_packs() {
    require_git!();
    let t = history("vp-git");
    git(t.path(), &["gc", "-q", "--aggressive", "--prune=now"]).ok();
    same_verify_output(&t, "git gc --aggressive");

    let r = history("vp-ref");
    git(r.path(), &["-c", "repack.useDeltaBaseOffset=false", "repack", "-q", "-a", "-d", "-f"]).ok();
    same_verify_output(&r, "REF_DELTA pack");
}

#[test]
fn verify_pack_matches_git_for_nanogit_packs() {
    require_git!();
    let t = history("vp-ng");
    ng(t.path(), &["gc", "-q"]).ok();
    same_verify_output(&t, "nanogit gc");
    let out = ng(t.path(), &["verify-pack", "-v", &idx_of(&t)]).ok();
    assert!(out.contains("chain length = 1:"), "{out}");
}

#[test]
fn verify_pack_reports_damage() {
    require_git!();
    let t = history("vp-bad");
    ng(t.path(), &["gc", "-q"]).ok();
    let idx = idx_of(&t);
    let pack = t.path().join(idx.replace(".idx", ".pack"));
    make_writable(&pack);
    let mut bytes = fs::read(&pack).unwrap();
    let mid = bytes.len() / 3;
    bytes[mid] ^= 0x20;
    fs::write(&pack, &bytes).unwrap();
    let r = ng(t.path(), &["verify-pack", "-v", &idx]);
    assert_eq!(r.code, 1);
    assert!(r.stdout.ends_with(": bad\n"), "{}", r.stdout);
    assert!(r.stderr.contains("checksum"), "{}", r.stderr);
    assert_ne!(git(t.path(), &["verify-pack", &idx]).code, 0, "git agrees the pack is damaged");
    assert_eq!(ng(t.path(), &["verify-pack"]).code, 129);
    assert_eq!(ng(t.path(), &["verify-pack", "missing.idx"]).code, 1);
}

#[test]
fn count_objects_matches_git() {
    require_git!();
    let t = history("co");
    let check = |t: &TempDir, when: &str| {
        assert_eq!(ng(t.path(), &["count-objects", "-v"]).ok(), git(t.path(), &["count-objects", "-v"]).ok(), "{when}");
        assert_eq!(ng(t.path(), &["count-objects"]).ok(), git(t.path(), &["count-objects"]).ok(), "{when}");
    };
    check(&t, "all loose");
    ng(t.path(), &["repack"]).ok();
    check(&t, "packed but not pruned");
    ng(t.path(), &["gc", "-q"]).ok();
    check(&t, "after gc");
    t.write("later.txt", "written after gc\n");
    ng(t.path(), &["add", "later.txt"]).ok();
    ng(t.path(), &["commit", "-m", "later"]).ok();
    check(&t, "loose on top of a pack");
    fs::write(t.path().join(".git/objects/pack/stray.tmp"), "x").unwrap();
    check(&t, "garbage in the pack directory");
}
