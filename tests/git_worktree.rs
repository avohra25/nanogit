//! `status`, `diff`, `.gitignore`, `branch` and `checkout` cross-checked
//! against real git on the very same repositories.

mod common;

use common::{git, ng, ng_env, TempDir};
use std::fs;
use std::os::unix::fs::PermissionsExt;

const DATE: [(&str, &str); 2] = [("GIT_AUTHOR_DATE", "1700000000 +0000"), ("GIT_COMMITTER_DATE", "1700000000 +0000")];

fn commit(t: &TempDir, msg: &str) {
    ng_env(t.path(), &["commit", "-m", msg], &DATE, b"").ok();
}

fn chmod(t: &TempDir, rel: &str, mode: u32) {
    fs::set_permissions(t.path().join(rel), fs::Permissions::from_mode(mode)).unwrap();
}

/// git's long status without hint lines, with our program name normalised.
fn comparable_long(text: &str) -> String {
    text.replace("nanogit", "git")
        .lines()
        .filter(|l| !l.starts_with("  ("))
        .collect::<Vec<_>>()
        .join("\n")
}

fn busy_repo(tag: &str) -> TempDir {
    let t = TempDir::new(tag);
    ng(t.path(), &["init"]).ok();
    t.write(".gitignore", "*.log\nbuild/\n!keep.log\n");
    t.write("a.txt", "alpha\nbeta\n");
    t.write("b.txt", "bee\n");
    t.write("c.txt", "see\n");
    t.write("dir/x.txt", "ex\n");
    t.write("dir/y.txt", "why\n");
    t.write("r.txt", "rename me please\nwith a few lines\nof content\n");
    t.write("run.sh", "#!/bin/sh\necho hi\n");
    chmod(&t, "run.sh", 0o755);
    ng(t.path(), &["add", "."]).ok();
    commit(&t, "base");
    // Staged changes.
    t.write("b.txt", "bee\nstaged edit\n");
    ng(t.path(), &["add", "b.txt"]).ok();
    ng(t.path(), &["rm", "c.txt"]).ok();
    t.write("s.txt", "brand new\n");
    ng(t.path(), &["add", "s.txt"]).ok();
    fs::rename(t.path().join("r.txt"), t.path().join("renamed.txt")).unwrap();
    ng(t.path(), &["add", "renamed.txt"]).ok();
    ng(t.path(), &["rm", "--cached", "r.txt"]).ok();
    // Unstaged changes (a.txt modified twice: staged then edited again).
    t.write("a.txt", "alpha\nbeta\ngamma\n");
    ng(t.path(), &["add", "a.txt"]).ok();
    t.write("a.txt", "alpha\nBETA\ngamma\n");
    fs::remove_file(t.path().join("dir/x.txt")).unwrap();
    chmod(&t, "run.sh", 0o644);
    // Untracked and ignored files.
    t.write("u.txt", "untracked\n");
    t.write("newdir/f.txt", "f\n");
    t.write("newdir/sub/g.txt", "g\n");
    t.write("noise.log", "ignored\n");
    t.write("build/out.o", "ignored\n");
    t.write("keep.log", "re-included\n");
    fs::create_dir_all(t.path().join("emptydir")).unwrap();
    t
}

#[test]
fn status_matches_git_in_every_format() {
    require_git!();
    let t = busy_repo("status");
    let want = git(t.path(), &["status", "--porcelain"]).ok();
    assert_eq!(ng(t.path(), &["status", "--porcelain"]).ok(), want);
    assert_eq!(ng(t.path(), &["status", "-s"]).ok(), want);
    assert_eq!(
        ng(t.path(), &["status", "--porcelain", "-b"]).ok(),
        git(t.path(), &["status", "--porcelain", "-b"]).ok()
    );
    let long_ng = ng(t.path(), &["status"]).ok();
    let long_git = git(t.path(), &["status"]).ok();
    assert_eq!(comparable_long(&long_ng), comparable_long(&long_git), "\n--- ours ---\n{long_ng}\n--- git ---\n{long_git}");
}

#[test]
fn status_from_a_subdirectory_uses_relative_paths() {
    require_git!();
    let t = busy_repo("status-sub");
    let sub = t.path().join("dir");
    assert_eq!(ng(&sub, &["status", "-s"]).ok(), git(&sub, &["status", "-s"]).ok());
    let want = git(t.path(), &["status", "--porcelain", "--", "dir"]).ok();
    assert_eq!(ng(t.path(), &["status", "--porcelain", "--", "dir"]).ok(), want);
}

#[test]
fn status_of_new_and_clean_repositories() {
    require_git!();
    let t = TempDir::new("status-new");
    ng(t.path(), &["init"]).ok();
    let (a, b) = (ng(t.path(), &["status"]).ok(), git(t.path(), &["status"]).ok());
    assert_eq!(comparable_long(&a), comparable_long(&b));
    t.write("one.txt", "1\n");
    ng(t.path(), &["add", "one.txt"]).ok();
    let (a, b) = (ng(t.path(), &["status"]).ok(), git(t.path(), &["status"]).ok());
    assert_eq!(comparable_long(&a), comparable_long(&b));
    assert_eq!(ng(t.path(), &["status", "-sb"]).ok(), git(t.path(), &["status", "-sb"]).ok());
    commit(&t, "first");
    let (a, b) = (ng(t.path(), &["status"]).ok(), git(t.path(), &["status"]).ok());
    assert_eq!(comparable_long(&a), comparable_long(&b));
    assert!(a.contains("nothing to commit, working tree clean"));
}

#[test]
fn diff_output_is_byte_identical_to_git() {
    require_git!();
    let t = busy_repo("diff");
    t.write("long.txt", (1..=40).map(|i| format!("line {i}\n")).collect::<String>());
    t.write("nonl.txt", "no newline at end");
    t.write("bin.dat", b"\x00\x01\x02binary\xff");
    ng(t.path(), &["add", "long.txt", "nonl.txt", "bin.dat"]).ok();
    commit(&t, "more files");
    let mut edited = (1..=40).map(|i| format!("line {i}\n")).collect::<Vec<_>>();
    edited[2] = "changed 3\n".into();
    edited[3] = "changed 4\n".into();
    edited[30] = "changed 31\n".into();
    edited.insert(20, "inserted\n".into());
    t.write("long.txt", edited.concat());
    t.write("nonl.txt", "no newline at end, now longer");
    t.write("bin.dat", b"\x00\x01\x03binary\xfe");
    std::os::unix::fs::symlink("a.txt", t.path().join("link")).unwrap();
    for args in [
        vec!["diff"],
        vec!["diff", "--cached"],
        vec!["diff", "HEAD"],
        vec!["diff", "-U1"],
        vec!["diff", "--unified=0", "long.txt"],
        vec!["diff", "HEAD~1", "HEAD"],
        vec!["diff", "HEAD", "HEAD~1"],
        vec!["diff", "--name-only"],
        vec!["diff", "--name-status", "HEAD"],
        vec!["diff", "--", "dir"],
    ] {
        let mut theirs = vec!["-c", "core.abbrev=7", "diff", "--no-renames"];
        theirs.extend(&args[1..]);
        let want = git(t.path(), &theirs).ok();
        let got = ng(t.path(), &args).ok();
        assert_eq!(got, want, "nanogit {args:?}");
    }
    // Add the symlink and compare again (new symlink and file->symlink changes).
    ng(t.path(), &["add", "link"]).ok();
    let want = git(t.path(), &["diff", "--cached", "--no-renames"]).ok();
    assert_eq!(ng(t.path(), &["diff", "--cached"]).ok(), want);
}

#[test]
fn diff_exit_codes_and_type_changes() {
    require_git!();
    let t = TempDir::new("diff-exit");
    ng(t.path(), &["init"]).ok();
    t.write("f", "content\n");
    ng(t.path(), &["add", "f"]).ok();
    commit(&t, "one");
    assert_eq!(ng(t.path(), &["diff", "--exit-code"]).code, 0);
    fs::remove_file(t.path().join("f")).unwrap();
    std::os::unix::fs::symlink("target", t.path().join("f")).unwrap();
    let run = ng(t.path(), &["diff", "--exit-code"]);
    assert_eq!(run.code, 1);
    assert_eq!(run.stdout, git(t.path(), &["diff"]).ok());
    assert_eq!(ng(t.path(), &["diff", "--quiet"]).stdout, "");
    let status = ng(t.path(), &["status", "-s"]).ok();
    assert_eq!(status, git(t.path(), &["status", "-s"]).ok());
    assert!(status.starts_with(" T f"));
}

#[test]
fn patches_from_diff_apply_with_git() {
    require_git!();
    let t = busy_repo("apply");
    let patch = ng(t.path(), &["diff", "--cached"]).ok();
    // Reverse-check: the staged patch applies cleanly to a checkout of HEAD.
    let base = TempDir::new("apply-base");
    git(base.path(), &["init", "-q"]).ok();
    for rel in ["a.txt", "b.txt", "c.txt", "dir/x.txt", "dir/y.txt", "r.txt", ".gitignore"] {
        let content = git(t.path(), &["show", &format!("HEAD:{rel}")]).ok();
        base.write(rel, content);
    }
    base.write("run.sh", git(t.path(), &["show", "HEAD:run.sh"]).ok());
    chmod(&base, "run.sh", 0o755);
    base.write("patch.diff", &patch);
    let checked = git(base.path(), &["apply", "--check", "patch.diff"]);
    assert_eq!(checked.code, 0, "{}\n{patch}", checked.stderr);
    git(base.path(), &["apply", "patch.diff"]).ok();
    assert_eq!(base.read("b.txt"), "bee\nstaged edit\n");
    assert!(!base.exists("c.txt"));
    assert_eq!(base.read("s.txt"), "brand new\n");
}

fn check_ignore_both(t: &TempDir, paths: &[&str]) {
    for path in paths {
        let ours = ng(t.path(), &["check-ignore", "-q", path]).code;
        let theirs = git(t.path(), &["check-ignore", "-q", path]).code;
        assert_eq!(ours, theirs, "path {path:?}: nanogit={ours} git={theirs}");
    }
}

#[test]
fn gitignore_semantics_match_git() {
    require_git!();
    let t = TempDir::new("ignore");
    ng(t.path(), &["init"]).ok();
    t.write(
        ".gitignore",
        "# comment\n*.o\n*.py[co]\n/rootonly\ncache/\ndocs/*.tmp\n**/deep\nlogs/**\nout/*\n!out/keep\nfile?.txt\nstar\\*name\n!important.o\n",
    );
    t.write("sub/.gitignore", "*.txt\n!sub-keep.txt\n/local\n");
    t.write(".git/info/exclude", "excluded-by-info\n");
    let files: Vec<&str> = vec![
        "main.o", "x/y/z.o", "important.o", "a.pyc", "a.pyo", "a.pyx", "rootonly", "d/rootonly", "cache/data", "d/cache/data",
        "docs/a.tmp", "docs/sub/a.tmp", "x/docs/a.tmp", "deep", "p/q/deep", "logs/a", "logs/b/c", "out/a", "out/keep", "file1.txt",
        "file12.txt", "star*name", "starxname", "sub/a.txt", "sub/sub-keep.txt", "sub/deeper/b.txt", "sub/local", "sub/x/local",
        "other/a.txt", "excluded-by-info", "q/excluded-by-info", "plain.rs",
    ];
    for f in &files {
        t.write(f, "x\n");
    }
    check_ignore_both(&t, &files);
    // Whole-tree listing of untracked, non-ignored files.
    let want = git(t.path(), &["ls-files", "-o", "--exclude-standard"]).ok();
    assert_eq!(ng(t.path(), &["ls-files", "-o", "--exclude-standard"]).ok(), want);
    let want = git(t.path(), &["ls-files", "-o", "-i", "--exclude-standard"]).ok();
    assert_eq!(ng(t.path(), &["ls-files", "-o", "-i", "--exclude-standard"]).ok(), want);
    // `add .` stages exactly the non-ignored files.
    ng(t.path(), &["add", "."]).ok();
    let staged = ng(t.path(), &["ls-files"]).ok();
    git(t.path(), &["reset", "-q"]).ok();
    git(t.path(), &["add", "."]).ok();
    assert_eq!(staged, git(t.path(), &["ls-files"]).ok());
}

#[test]
fn add_refuses_ignored_paths_unless_forced() {
    let t = TempDir::new("ignore-add");
    ng(t.path(), &["init"]).ok();
    t.write(".gitignore", "*.log\n");
    t.write("a.log", "x\n");
    t.write("b.txt", "y\n");
    let run = ng(t.path(), &["add", "a.log", "b.txt"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("ignored by one of your .gitignore files") && run.stderr.contains("a.log"));
    assert_eq!(ng(t.path(), &["ls-files"]).ok(), "b.txt\n");
    ng(t.path(), &["add", "-f", "a.log"]).ok();
    assert_eq!(ng(t.path(), &["ls-files"]).ok(), "a.log\nb.txt\n");
    // A tracked file stays tracked even though a pattern matches it.
    t.write("a.log", "changed\n");
    assert_eq!(ng(t.path(), &["status", "-s"]).ok(), "AM a.log\nA  b.txt\n?? .gitignore\n");
}

fn two_branches(tag: &str) -> TempDir {
    let t = TempDir::new(tag);
    ng(t.path(), &["init"]).ok();
    t.write("shared.txt", "shared\n");
    t.write("only-main.txt", "main\n");
    t.write("dir/nested.txt", "nested\n");
    ng(t.path(), &["add", "."]).ok();
    commit(&t, "base");
    ng(t.path(), &["checkout", "-b", "feature"]).ok();
    t.write("shared.txt", "shared on feature\n");
    t.write("feature.txt", "feature only\n");
    t.write("dir/deeper/new.txt", "deep\n");
    fs::remove_file(t.path().join("only-main.txt")).unwrap();
    chmod(&t, "dir/nested.txt", 0o755);
    ng(t.path(), &["add", "-A"]).ok();
    commit(&t, "feature work");
    t
}

#[test]
fn checkout_switches_trees_and_git_agrees() {
    require_git!();
    let t = two_branches("checkout");
    assert_eq!(ng(t.path(), &["branch"]).ok(), "* feature\n  main\n");
    let run = ng(t.path(), &["checkout", "main"]);
    assert_eq!(run.code, 0);
    assert!(run.stderr.contains("Switched to branch 'main'"));
    assert_eq!(t.read("shared.txt"), "shared\n");
    assert!(t.exists("only-main.txt"));
    assert!(!t.exists("feature.txt") && !t.exists("dir/deeper"), "files and empty dirs of the old branch are removed");
    assert_eq!(git(t.path(), &["status", "--porcelain"]).ok(), "");
    assert_eq!(git(t.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).ok(), "main\n");
    ng(t.path(), &["checkout", "feature"]).ok();
    assert_eq!(git(t.path(), &["status", "--porcelain"]).ok(), "");
    assert!(fs::metadata(t.path().join("dir/nested.txt")).unwrap().permissions().mode() & 0o111 != 0);
    git(t.path(), &["fsck", "--strict"]).ok();
    // Branch management is visible to git.
    ng(t.path(), &["branch", "topic", "main"]).ok();
    assert_eq!(git(t.path(), &["rev-parse", "topic"]).ok(), git(t.path(), &["rev-parse", "main"]).ok());
    ng(t.path(), &["branch", "-m", "topic", "renamed"]).ok();
    assert_eq!(git(t.path(), &["branch", "--format=%(refname:short)"]).ok(), "feature\nmain\nrenamed\n");
    assert!(ng(t.path(), &["branch", "-d", "renamed"]).stdout.starts_with("Deleted branch renamed (was "));
    assert_eq!(ng(t.path(), &["branch", "--show-current"]).ok(), "feature\n");
    // Branches created by git are usable too.
    git(t.path(), &["branch", "from-git", "main"]).ok();
    ng(t.path(), &["checkout", "from-git"]).ok();
    assert_eq!(git(t.path(), &["symbolic-ref", "--short", "HEAD"]).ok(), "from-git\n");
}

#[test]
fn checkout_refuses_to_clobber_local_changes() {
    require_git!();
    let t = two_branches("clobber");
    ng(t.path(), &["checkout", "main"]).ok();
    // Unstaged edit to a file that differs between the branches.
    t.write("shared.txt", "my precious edit\n");
    let run = ng(t.path(), &["checkout", "feature"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("Your local changes to the following files would be overwritten by checkout:\n\tshared.txt\n"), "{}", run.stderr);
    assert_eq!(t.read("shared.txt"), "my precious edit\n");
    assert_eq!(git(t.path(), &["symbolic-ref", "--short", "HEAD"]).ok(), "main\n");
    // A staged change is protected as well.
    ng(t.path(), &["add", "shared.txt"]).ok();
    assert_eq!(ng(t.path(), &["checkout", "feature"]).code, 1);
    // An edit to a file both branches share is carried over.
    ng(t.path(), &["checkout", "main", "--", "shared.txt"]).ok();
    t.write("dir/nested.txt", "same on both? no, mode differs\n");
    t.write("shared.txt", "shared\n");
    assert_eq!(ng(t.path(), &["checkout", "feature"]).code, 1, "dir/nested.txt differs in mode between branches");
    ng(t.path(), &["checkout", "--", "dir/nested.txt"]).ok();
    assert_eq!(t.read("dir/nested.txt"), "nested\n");
    // Untracked files in the way.
    t.write("feature.txt", "I was here first\n");
    let run = ng(t.path(), &["checkout", "feature"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("The following untracked working tree files would be overwritten by checkout:\n\tfeature.txt\n"));
    assert_eq!(t.read("feature.txt"), "I was here first\n");
    // Unrelated untracked files and edits to files identical on both branches survive.
    fs::remove_file(t.path().join("feature.txt")).unwrap();
    t.write("scratch.txt", "keep me\n");
    ng(t.path(), &["checkout", "feature"]).ok();
    assert_eq!(t.read("scratch.txt"), "keep me\n");
    // Force discards local changes to tracked files.
    t.write("shared.txt", "doomed\n");
    ng(t.path(), &["checkout", "-f", "main"]).ok();
    assert_eq!(t.read("shared.txt"), "shared\n");
    assert_eq!(git(t.path(), &["status", "--porcelain"]).ok(), "?? scratch.txt\n");
}

#[test]
fn checkout_paths_detach_and_new_branches() {
    require_git!();
    let t = two_branches("paths");
    // Restore a path from another commit; it also lands in the index.
    ng(t.path(), &["checkout", "main", "--", "only-main.txt"]).ok();
    assert_eq!(t.read("only-main.txt"), "main\n");
    assert_eq!(git(t.path(), &["status", "--porcelain"]).ok(), "A  only-main.txt\n");
    ng(t.path(), &["rm", "-f", "only-main.txt"]).ok();
    // Restore from the index discards worktree edits.
    t.write("shared.txt", "scribble\n");
    ng(t.path(), &["checkout", "--", "shared.txt"]).ok();
    assert_eq!(t.read("shared.txt"), "shared on feature\n");
    let run = ng(t.path(), &["checkout", "--", "missing.txt"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("did not match any file(s) known to git"));
    // Detached HEAD.
    let first = git(t.path(), &["rev-parse", "HEAD~1"]).ok();
    let run = ng(t.path(), &["checkout", "HEAD~1"]);
    assert!(run.stderr.contains("HEAD is now at "));
    assert_eq!(git(t.path(), &["rev-parse", "HEAD"]).ok(), first);
    assert_eq!(git(t.path(), &["symbolic-ref", "-q", "HEAD"]).code, 1, "HEAD is detached");
    assert_eq!(ng(t.path(), &["status", "-sb"]).ok(), git(t.path(), &["status", "-sb"]).ok());
    // New branch from a start point.
    ng(t.path(), &["checkout", "-b", "from-detached"]).ok();
    assert_eq!(git(t.path(), &["rev-parse", "from-detached"]).ok(), first);
    ng(t.path(), &["checkout", "-b", "at-feature", "feature"]).ok();
    assert!(t.exists("feature.txt"));
    assert_eq!(ng(t.path(), &["checkout", "-b", "feature"]).code, 128);
    assert_eq!(ng(t.path(), &["checkout", "no-such-thing"]).code, 128);
    assert_eq!(ng(t.path(), &["branch", "bad..name"]).code, 128);
}
