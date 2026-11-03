//! `merge`, `merge-base` and `reset` cross-checked against real git: both
//! tools run on copies of the same repository and must agree.

mod common;

use common::{copy_dir, git, git_env, ng, ng_env, TempDir};
use std::fs;
use std::os::unix::fs::PermissionsExt;

/// Fixed dates so commit ids can be compared between the two tools.
fn dates(n: u32) -> Vec<(&'static str, String)> {
    let d = format!("{} +0000", 1_700_000_000 + n * 60);
    vec![("GIT_AUTHOR_DATE", d.clone()), ("GIT_COMMITTER_DATE", d)]
}

fn run_ng(t: &TempDir, args: &[&str], n: u32) -> common::Run {
    let d = dates(n);
    let env: Vec<(&str, &str)> = d.iter().map(|(k, v)| (*k, v.as_str())).collect();
    ng_env(t.path(), args, &env, b"")
}

fn run_git(t: &TempDir, args: &[&str], n: u32) -> common::Run {
    let d = dates(n);
    let env: Vec<(&str, &str)> = d.iter().map(|(k, v)| (*k, v.as_str())).collect();
    git_env(t.path(), args, &env)
}

fn commit_all(t: &TempDir, msg: &str, n: u32) {
    ng(t.path(), &["add", "-A"]).ok();
    run_ng(t, &["commit", "--allow-empty", "-m", msg], n).ok();
}

fn poem(lines: &[(usize, &str)]) -> String {
    let mut v: Vec<String> = (1..=12).map(|i| format!("line {i}\n")).collect();
    for (i, text) in lines {
        v[i - 1] = format!("{text}\n");
    }
    v.concat()
}

/// main and feature diverge from a shared base.  Returns the repo on `main`.
fn diverged(tag: &str, conflicting: bool) -> TempDir {
    let t = TempDir::new(tag);
    ng(t.path(), &["init"]).ok();
    t.write("poem.txt", poem(&[]));
    t.write("conf.txt", "alpha\nbeta\ngamma\n");
    t.write("gone.txt", "to be deleted\nsecond line\n");
    t.write("run.sh", "#!/bin/sh\necho one\necho two\necho three\n");
    t.write("bin.dat", b"\x00\x01\x02base");
    t.write("keep/me.txt", "unchanged\n");
    commit_all(&t, "base", 0);
    ng(t.path(), &["checkout", "-b", "feature"]).ok();
    t.write("poem.txt", poem(&[(10, "feature ten")]));
    t.write("feature-only.txt", "only on feature\n");
    t.write("run.sh", "#!/bin/sh\necho one\necho two\necho three\necho four\n");
    if conflicting {
        t.write("conf.txt", "alpha\nfeature beta\ngamma\n");
        t.write("both-add.txt", "added by feature\n");
        t.write("gone.txt", "to be deleted\nfeature edit\n");
        t.write("bin.dat", b"\x00\x01\x02feature");
    }
    commit_all(&t, "feature work", 1);
    ng(t.path(), &["checkout", "main"]).ok();
    t.write("poem.txt", poem(&[(2, "main two")]));
    t.write("main-only.txt", "only on main\n");
    fs::set_permissions(t.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    if conflicting {
        t.write("conf.txt", "alpha\nmain beta\ngamma\n");
        t.write("both-add.txt", "added by main\n");
        fs::remove_file(t.path().join("gone.txt")).unwrap();
        t.write("bin.dat", b"\x00\x01\x02main");
    }
    commit_all(&t, "main work", 2);
    t
}

fn twin(t: &TempDir, tag: &str) -> TempDir {
    let copy = TempDir::new(tag);
    copy_dir(t.path(), copy.path());
    copy
}

#[test]
fn clean_merge_creates_the_same_commit_as_git() {
    require_git!();
    let a = diverged("merge-clean-ng", false);
    let b = twin(&a, "merge-clean-git");
    let out = run_ng(&a, &["merge", "feature"], 3).ok();
    assert!(out.contains("Merge made by"), "{out}");
    run_git(&b, &["merge", "--no-edit", "feature"], 3).ok();
    let want = git(b.path(), &["rev-parse", "HEAD"]).ok();
    assert_eq!(git(a.path(), &["rev-parse", "HEAD"]).ok(), want, "merge commits differ");
    assert_eq!(git(a.path(), &["log", "-1", "--format=%P"]).ok().trim().matches(' ').count(), 1, "two parents");
    assert_eq!(ng(a.path(), &["ls-files", "-s"]).ok(), git(b.path(), &["ls-files", "-s"]).ok());
    for f in ["poem.txt", "feature-only.txt", "main-only.txt", "run.sh"] {
        assert_eq!(a.read(f), b.read(f), "{f}");
    }
    let poem = a.read("poem.txt");
    assert!(poem.contains("main two") && poem.contains("feature ten"));
    assert_eq!(git(a.path(), &["status", "--porcelain"]).ok(), "");
    git(a.path(), &["fsck", "--strict"]).ok();
    // The history is readable by both tools.
    assert_eq!(ng(a.path(), &["log", "--oneline"]).ok(), git(a.path(), &["log", "--oneline", "--abbrev=7"]).ok());
}

#[test]
fn conflicted_merge_matches_git_stages_and_markers() {
    require_git!();
    let a = diverged("merge-conflict-ng", true);
    let b = twin(&a, "merge-conflict-git");
    let run = run_ng(&a, &["merge", "feature"], 3);
    assert_eq!(run.code, 1, "{}", run.stderr);
    assert!(run.stdout.contains("CONFLICT (content): Merge conflict in conf.txt"), "{}", run.stdout);
    assert!(run.stdout.contains("Automatic merge failed; fix conflicts and then commit the result."));
    assert_eq!(run_git(&b, &["merge", "--no-edit", "feature"], 3).code, 1);
    assert_eq!(ng(a.path(), &["ls-files", "-s"]).ok(), git(b.path(), &["ls-files", "-s"]).ok());
    assert_eq!(git(a.path(), &["status", "--porcelain"]).ok(), git(b.path(), &["status", "--porcelain"]).ok());
    assert_eq!(ng(a.path(), &["status", "--porcelain"]).ok(), git(a.path(), &["status", "--porcelain"]).ok());
    for f in ["conf.txt", "both-add.txt", "gone.txt", "bin.dat", "poem.txt", "run.sh"] {
        assert_eq!(fs::read(a.path().join(f)).ok(), fs::read(b.path().join(f)).ok(), "work tree file {f}");
    }
    assert_eq!(a.read("conf.txt"), "alpha\n<<<<<<< HEAD\nmain beta\n=======\nfeature beta\n>>>>>>> feature\ngamma\n");
    // The merge cannot be committed until every conflict is resolved.
    let blocked = ng(a.path(), &["commit", "-m", "too early"]);
    assert_ne!(blocked.code, 0);
    assert!(ng(a.path(), &["status"]).ok().contains("You have unmerged paths."));
    // Resolve like a user would, then conclude the merge.
    for f in ["conf.txt", "both-add.txt", "gone.txt", "bin.dat"] {
        a.write(f, format!("resolved {f}\n"));
        ng(a.path(), &["add", f]).ok();
    }
    assert!(ng(a.path(), &["status"]).ok().contains("All conflicts fixed but you are still merging."));
    let out = run_ng(&a, &["commit"], 4).ok();
    assert!(out.contains("Merge branch 'feature'"), "{out}");
    let parents = git(a.path(), &["rev-list", "--parents", "-n", "1", "HEAD"]).ok();
    assert_eq!(parents.split_whitespace().count(), 3, "{parents}");
    assert!(!a.path().join(".git/MERGE_HEAD").exists());
    assert_eq!(git(a.path(), &["status", "--porcelain"]).ok(), "");
    git(a.path(), &["fsck", "--strict"]).ok();
}

#[test]
fn merge_abort_restores_the_pre_merge_state() {
    require_git!();
    let a = diverged("merge-abort", true);
    let before = git(a.path(), &["rev-parse", "HEAD"]).ok();
    assert_eq!(run_ng(&a, &["merge", "feature"], 3).code, 1);
    assert_eq!(run_ng(&a, &["merge", "feature"], 3).code, 128, "a merge is already in progress");
    ng(a.path(), &["merge", "--abort"]).ok();
    assert_eq!(git(a.path(), &["rev-parse", "HEAD"]).ok(), before);
    assert_eq!(git(a.path(), &["status", "--porcelain"]).ok(), "");
    assert_eq!(a.read("conf.txt"), "alpha\nmain beta\ngamma\n");
    assert!(!a.exists("feature-only.txt") && !a.path().join(".git/MERGE_HEAD").exists());
    assert_ne!(ng(a.path(), &["merge", "--abort"]).code, 0);
}

#[test]
fn hard_reset_ends_a_conflicted_merge() {
    require_git!();
    let a = diverged("merge-reset", true);
    assert_eq!(run_ng(&a, &["merge", "feature"], 3).code, 1);
    assert_eq!(ng(a.path(), &["reset", "--soft", "HEAD"]).code, 128, "soft reset refused mid-merge");
    ng(a.path(), &["reset", "--hard"]).ok();
    assert_eq!(git(a.path(), &["status", "--porcelain"]).ok(), "");
    assert!(!a.path().join(".git/MERGE_HEAD").exists() && !a.exists("feature-only.txt"));
    assert_eq!(a.read("conf.txt"), "alpha\nmain beta\ngamma\n");
}

#[test]
fn fast_forward_and_no_ff() {
    require_git!();
    let a = TempDir::new("merge-ff");
    ng(a.path(), &["init"]).ok();
    a.write("one.txt", "1\n");
    commit_all(&a, "one", 0);
    ng(a.path(), &["checkout", "-b", "topic"]).ok();
    a.write("two.txt", "2\n");
    commit_all(&a, "two", 1);
    ng(a.path(), &["checkout", "main"]).ok();
    let b = twin(&a, "merge-ff-git");
    let c = twin(&a, "merge-noff");
    let out = ng(a.path(), &["merge", "topic"]).ok();
    assert!(out.contains("Fast-forward"), "{out}");
    run_git(&b, &["merge", "topic"], 5).ok();
    assert_eq!(git(a.path(), &["rev-parse", "HEAD"]).ok(), git(b.path(), &["rev-parse", "HEAD"]).ok());
    assert_eq!(a.read("two.txt"), "2\n");
    assert_eq!(ng(a.path(), &["merge", "topic"]).ok(), "Already up to date.\n");
    // --no-ff forces a merge commit that git reproduces exactly.
    run_ng(&c, &["merge", "--no-ff", "topic"], 5).ok();
    let d = twin(&b, "merge-noff-git");
    git(d.path(), &["reset", "--hard", "-q", "HEAD~1"]).ok();
    run_git(&d, &["merge", "--no-ff", "--no-edit", "topic"], 5).ok();
    assert_eq!(git(c.path(), &["rev-parse", "HEAD"]).ok(), git(d.path(), &["rev-parse", "HEAD"]).ok());
    // --ff-only refuses a diverged history.
    let e = diverged("merge-ffonly", false);
    let run = ng(e.path(), &["merge", "--ff-only", "feature"]);
    assert_eq!(run.code, 128);
    assert!(run.stderr.contains("Not possible to fast-forward"));
}

#[test]
fn merge_protects_local_changes() {
    require_git!();
    let a = diverged("merge-dirty", false);
    // An unrelated edit survives the merge...
    a.write("keep/me.txt", "my local edit\n");
    ng(a.path(), &["merge", "--no-commit", "feature"]).ok();
    assert_eq!(a.read("keep/me.txt"), "my local edit\n");
    ng(a.path(), &["merge", "--abort"]).ok();
    // ...but an edit to a file the merge changes is refused.
    a.write("feature-only.txt", "squatter\n");
    let run = ng(a.path(), &["merge", "feature"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("untracked working tree files would be overwritten by merge"), "{}", run.stderr);
    assert_eq!(a.read("feature-only.txt"), "squatter\n");
    fs::remove_file(a.path().join("feature-only.txt")).unwrap();
    a.write("poem.txt", "edited, not committed\n");
    let run = ng(a.path(), &["merge", "feature"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("Your local changes to the following files would be overwritten by merge"));
    assert_eq!(a.read("poem.txt"), "edited, not committed\n");
    // Staged changes also block merging.
    ng(a.path(), &["add", "poem.txt"]).ok();
    assert_eq!(ng(a.path(), &["merge", "feature"]).code, 1);
    assert_eq!(ng(a.path(), &["merge", "nonexistent"]).code, 1);
}

#[test]
fn merge_base_agrees_with_git() {
    require_git!();
    //   a - b - c - m1 (main)         criss-cross: m1 merges x1, m2 merges c
    //        \   \  /
    //         x1 - x2 ...
    let t = TempDir::new("merge-base");
    ng(t.path(), &["init"]).ok();
    t.write("f", "0\n");
    commit_all(&t, "a", 0);
    t.write("f", "1\n");
    commit_all(&t, "b", 1);
    ng(t.path(), &["branch", "side"]).ok();
    t.write("f", "2\n");
    commit_all(&t, "c", 2);
    ng(t.path(), &["checkout", "side"]).ok();
    t.write("g", "side\n");
    commit_all(&t, "x1", 3);
    ng(t.path(), &["branch", "side2"]).ok();
    t.write("g", "side again\n");
    commit_all(&t, "x2", 4);
    ng(t.path(), &["checkout", "main"]).ok();
    run_ng(&t, &["merge", "side2"], 5).ok();
    ng(t.path(), &["checkout", "side"]).ok();
    run_ng(&t, &["merge", "main"], 6).ok();
    for (x, y) in [("main", "side"), ("side", "main"), ("main", "side2"), ("HEAD", "HEAD~1"), ("HEAD~2", "main")] {
        let want = git(t.path(), &["merge-base", x, y]);
        let got = ng(t.path(), &["merge-base", x, y]);
        assert_eq!(got.code, want.code, "{x} {y}");
        assert_eq!(got.stdout, want.stdout, "{x} {y}");
        let want = git(t.path(), &["merge-base", "--all", x, y]).ok();
        let mut got: Vec<_> = ng(t.path(), &["merge-base", "--all", x, y]).ok().lines().map(String::from).collect();
        let mut want: Vec<_> = want.lines().map(String::from).collect();
        got.sort();
        want.sort();
        assert_eq!(got, want, "--all {x} {y}");
        let code = git(t.path(), &["merge-base", "--is-ancestor", x, y]).code;
        assert_eq!(ng(t.path(), &["merge-base", "--is-ancestor", x, y]).code, code, "ancestor {x} {y}");
    }
}

#[test]
fn reset_modes_match_git() {
    require_git!();
    let a = TempDir::new("reset");
    ng(a.path(), &["init"]).ok();
    a.write("a.txt", "one\n");
    a.write("b.txt", "bee\n");
    commit_all(&a, "first", 0);
    a.write("a.txt", "two\n");
    a.write("c.txt", "new file\n");
    commit_all(&a, "second", 1);
    let second = git(a.path(), &["rev-parse", "HEAD"]).ok();
    let first = git(a.path(), &["rev-parse", "HEAD~1"]).ok();
    a.write("b.txt", "dirty\n");
    // Mixed (default): the branch moves and the index follows, the work tree stays.
    ng(a.path(), &["reset", "HEAD~1"]).ok();
    assert_eq!(git(a.path(), &["rev-parse", "HEAD"]).ok(), first);
    assert_eq!(git(a.path(), &["status", "--porcelain"]).ok(), " M a.txt\n M b.txt\n?? c.txt\n");
    // Soft keeps the index.
    ng(a.path(), &["add", "-A"]).ok();
    ng(a.path(), &["reset", "--soft", second.trim()]).ok();
    assert_eq!(git(a.path(), &["rev-parse", "HEAD"]).ok(), second);
    assert_eq!(git(a.path(), &["status", "--porcelain"]).ok(), "M  b.txt\n");
    // Paths: unstage one file only.
    ng(a.path(), &["reset", "--", "b.txt"]).ok();
    assert_eq!(git(a.path(), &["status", "--porcelain"]).ok(), " M b.txt\n");
    assert!(ng(a.path(), &["reset", "--hard", "--", "b.txt"]).code != 0);
    // Hard discards everything tracked and returns to the commit.
    a.write("a.txt", "scribble\n");
    let out = ng(a.path(), &["reset", "--hard", first.trim()]).ok();
    assert!(out.starts_with("HEAD is now at ") && out.contains("first"), "{out}");
    assert_eq!(a.read("a.txt"), "one\n");
    assert_eq!(a.read("b.txt"), "bee\n");
    assert!(!a.exists("c.txt"));
    assert_eq!(git(a.path(), &["status", "--porcelain"]).ok(), "");
    // A hard reset also drops a half-finished merge.
    git(a.path(), &["fsck", "--strict"]).ok();
}
