//! Index, tree, commit and log behaviour cross-checked against real git.

mod common;

use common::{git, ng, ng_env, TempDir};

const DATE1: (&str, &str) = ("GIT_AUTHOR_DATE", "1700000000 +0000");
const DATE1C: (&str, &str) = ("GIT_COMMITTER_DATE", "1700000000 +0000");
const DATE2: (&str, &str) = ("GIT_AUTHOR_DATE", "1700003600 +0000");
const DATE2C: (&str, &str) = ("GIT_COMMITTER_DATE", "1700003600 +0000");

fn files(t: &TempDir) {
    t.write("README.md", "# demo\n\nsome text\n");
    t.write("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n");
    t.write("src/util/mod.rs", "pub fn id(x: u32) -> u32 { x }\n");
    t.write("a b.txt", "space in name\n");
    t.write("z.txt", "last\n");
    std::fs::set_permissions(
        t.path().join("src/main.rs"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink("README.md", t.path().join("link")).unwrap();
}

#[test]
fn identical_inputs_give_identical_object_ids() {
    require_git!();
    let a = TempDir::new("same-ng");
    let b = TempDir::new("same-git");
    for t in [&a, &b] {
        files(t);
    }
    ng(a.path(), &["init"]).ok();
    ng(a.path(), &["add", "."]).ok();
    let out = ng_env(a.path(), &["commit", "-m", "first commit"], &[DATE1, DATE1C], b"").ok();
    assert!(out.starts_with("[main (root-commit) "), "{out}");

    git(b.path(), &["init", "-q", "-b", "main"]).ok();
    git(b.path(), &["add", "."]).ok();
    let gdates = [DATE1, DATE1C];
    let mut cmd = std::process::Command::new("git");
    cmd.args(["commit", "-q", "-m", "first commit"]).current_dir(b.path());
    for (k, v) in gdates {
        cmd.env(k, v);
    }
    cmd.env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "Test Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    assert!(cmd.status().unwrap().success());

    let ours = ng(a.path(), &["rev-parse", "HEAD"]).ok();
    let theirs = git(b.path(), &["rev-parse", "HEAD"]).ok();
    assert_eq!(ours, theirs, "commit ids must match byte for byte");
    assert_eq!(
        ng(a.path(), &["ls-files", "-s"]).ok(),
        git(b.path(), &["ls-files", "-s"]).ok()
    );
    assert_eq!(ng(a.path(), &["write-tree"]).ok(), git(b.path(), &["write-tree"]).ok());
}

#[test]
fn git_accepts_what_nanogit_writes() {
    require_git!();
    let t = TempDir::new("ng-writes");
    ng(t.path(), &["init"]).ok();
    files(&t);
    ng(t.path(), &["add", "."]).ok();
    ng_env(t.path(), &["commit", "-m", "one"], &[DATE1, DATE1C], b"").ok();
    t.write("z.txt", "last, changed\n");
    t.write("new/dir/file.txt", "new\n");
    ng(t.path(), &["add", "z.txt", "new"]).ok();
    ng(t.path(), &["rm", "--cached", "a b.txt"]).ok();
    ng_env(
        t.path(),
        &["commit", "-m", "two\n\nbody paragraph\nsecond line"],
        &[DATE2, DATE2C],
        b"",
    )
    .ok();

    let fsck = git(t.path(), &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(fsck.code, 0, "fsck: {}{}", fsck.stdout, fsck.stderr);
    assert_eq!(
        git(t.path(), &["log", "--format=%s|%an|%ae|%at|%P"]).ok().lines().count(),
        2
    );
    assert_eq!(ng(t.path(), &["log"]).ok(), git(t.path(), &["log"]).ok());
    assert_eq!(ng(t.path(), &["log", "--oneline"]).ok(), git(t.path(), &["log", "--oneline"]).ok());
    // The index we wrote is valid for git: it sees only the removed file as untracked.
    let status = git(t.path(), &["status", "--porcelain"]).ok();
    assert_eq!(status, "?? a b.txt\n".replace("a b.txt", "\"a b.txt\""), "{status}");
    assert_eq!(
        ng(t.path(), &["ls-files", "-s"]).ok(),
        git(t.path(), &["ls-files", "-s"]).ok()
    );
}

#[test]
fn nanogit_reads_git_repositories() {
    require_git!();
    let t = TempDir::new("git-writes");
    git(t.path(), &["init", "-q", "-b", "main"]).ok();
    files(&t);
    git(t.path(), &["add", "."]).ok();
    let commit = |msg: &str, date: &str| {
        let out = std::process::Command::new("git")
            .args(["commit", "-q", "-m", msg])
            .current_dir(t.path())
            .env("GIT_AUTHOR_NAME", "G Author")
            .env("GIT_AUTHOR_EMAIL", "g@example.com")
            .env("GIT_COMMITTER_NAME", "G Author")
            .env("GIT_COMMITTER_EMAIL", "g@example.com")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    commit("first", "1700000000 -0800");
    t.write("src/main.rs", "fn main() {}\n");
    git(t.path(), &["add", "-A"]).ok();
    commit("second: changes main", "1700003600 +0530");
    git(t.path(), &["rm", "-q", "z.txt"]).ok();
    commit("third\n\nwith a body", "1700007200 +0000");

    assert_eq!(ng(t.path(), &["log"]).ok(), git(t.path(), &["log"]).ok());
    assert_eq!(ng(t.path(), &["log", "--oneline", "-n", "2"]).ok(), git(t.path(), &["log", "--oneline", "-n", "2"]).ok());
    assert_eq!(ng(t.path(), &["ls-files"]).ok(), git(t.path(), &["ls-files"]).ok());
    assert_eq!(ng(t.path(), &["ls-files", "-s"]).ok(), git(t.path(), &["ls-files", "-s"]).ok());
    assert_eq!(ng(t.path(), &["write-tree"]).ok(), git(t.path(), &["write-tree"]).ok());
    for rev in ["HEAD", "HEAD~1", "HEAD~2", "HEAD^", "HEAD^{tree}", "HEAD~2:README.md", "main", "HEAD:src/util/mod.rs"] {
        assert_eq!(
            ng(t.path(), &["rev-parse", rev]).ok(),
            git(t.path(), &["rev-parse", rev]).ok(),
            "rev-parse {rev}"
        );
    }
    let short = git(t.path(), &["rev-parse", "--short", "HEAD"]).ok();
    assert_eq!(ng(t.path(), &["rev-parse", "--short", "HEAD"]).ok(), short);
}

#[test]
fn ls_files_is_relative_to_the_current_directory() {
    require_git!();
    let t = TempDir::new("lsfiles");
    ng(t.path(), &["init"]).ok();
    files(&t);
    ng(t.path(), &["add", "-A"]).ok();
    let sub = t.path().join("src");
    assert_eq!(ng(&sub, &["ls-files"]).ok(), "main.rs\nutil/mod.rs\n");
    assert_eq!(ng(&sub, &["ls-files", "util"]).ok(), "util/mod.rs\n");
    assert_eq!(
        ng(&sub, &["ls-files", "..", "--", "../z.txt"]).ok().lines().last(),
        Some("../z.txt"),
        "paths outside the cwd are shown with ../"
    );
    let gsub = git(&sub, &["ls-files", "..", "--", "../z.txt"]).ok();
    assert_eq!(ng(&sub, &["ls-files", "..", "--", "../z.txt"]).ok(), gsub);
}

#[test]
fn rm_refuses_to_lose_work_unless_forced() {
    let t = TempDir::new("rm");
    ng(t.path(), &["init"]).ok();
    t.write("a.txt", "one\n");
    t.write("d/b.txt", "two\n");
    ng(t.path(), &["add", "."]).ok();
    let r = ng(t.path(), &["rm", "a.txt"]);
    assert_eq!(r.code, 128, "newly added file has staged changes");
    assert!(r.stderr.contains("changes staged in the index"), "{}", r.stderr);
    ng_env(t.path(), &["commit", "-m", "base"], &[], b"").ok();

    t.write("a.txt", "edited\n");
    let r = ng(t.path(), &["rm", "a.txt"]);
    assert_eq!(r.code, 128);
    assert!(r.stderr.contains("local modifications"), "{}", r.stderr);
    assert!(t.exists("a.txt"));

    let r = ng(t.path(), &["rm", "d"]);
    assert!(r.stderr.contains("without -r"), "{}", r.stderr);
    assert_eq!(ng(t.path(), &["rm", "-r", "d"]).ok(), "rm 'd/b.txt'\n");
    assert!(!t.exists("d"), "empty directory is pruned");

    ng(t.path(), &["rm", "--cached", "-f", "a.txt"]).ok();
    assert!(t.exists("a.txt"));
    assert_eq!(ng(t.path(), &["ls-files"]).ok(), "");
    assert_eq!(ng(t.path(), &["rm", "nope"]).code, 128);
}

#[test]
fn commit_all_empty_commits_and_errors() {
    let t = TempDir::new("commit");
    ng(t.path(), &["init"]).ok();
    let r = ng(t.path(), &["commit", "-m", "x"]);
    assert_eq!(r.code, 1, "nothing to commit on an empty repository");
    assert_eq!(ng(t.path(), &["log"]).code, 128);
    t.write("f.txt", "1\n");
    ng(t.path(), &["add", "f.txt"]).ok();
    ng(t.path(), &["commit", "-m", "one"]).ok();
    assert_eq!(ng(t.path(), &["commit", "-m", "again"]).code, 1);
    ng(t.path(), &["commit", "--allow-empty", "-m", "empty one"]).ok();
    t.write("f.txt", "2\n");
    t.write("untracked.txt", "u\n");
    ng(t.path(), &["commit", "-am", "tracked change"]).ok();
    assert_eq!(ng(t.path(), &["log", "--oneline"]).ok().lines().count(), 3);
    assert_eq!(ng(t.path(), &["ls-files"]).ok(), "f.txt\n", "-a never adds untracked files");
    let r = ng(t.path(), &["commit"]);
    assert_eq!(r.code, 128, "message required");
}

#[test]
fn config_round_trip_and_identity_from_config() {
    let t = TempDir::new("config");
    ng(t.path(), &["init"]).ok();
    ng(t.path(), &["config", "user.name", "Config Person"]).ok();
    ng(t.path(), &["config", "user.email", "cfg@example.com"]).ok();
    assert_eq!(ng(t.path(), &["config", "user.name"]).ok(), "Config Person\n");
    assert_eq!(ng(t.path(), &["config", "no.such"]).code, 1);
    assert!(ng(t.path(), &["config", "--list"]).ok().contains("user.email=cfg@example.com"));
    t.write("a", "a\n");
    ng(t.path(), &["add", "a"]).ok();
    let clear = [("GIT_AUTHOR_NAME", ""), ("GIT_AUTHOR_EMAIL", ""), ("GIT_COMMITTER_NAME", ""), ("GIT_COMMITTER_EMAIL", "")];
    ng_env(t.path(), &["commit", "-m", "m"], &clear, b"").ok();
    let log = ng(t.path(), &["log"]).ok();
    assert!(log.contains("Author: Config Person <cfg@example.com>"), "{log}");
}
