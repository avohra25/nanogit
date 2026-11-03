//! Helpers shared by the integration tests: temp dirs and driving the
//! `nanogit` binary and (when installed) the real `git`.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// True if a working `git` binary is on PATH.
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skip the current test (with a note) when git is missing.
#[macro_export]
macro_rules! require_git {
    () => {
        if !common::git_available() {
            eprintln!("git not found; skipping");
            return;
        }
    };
}

/// A scratch directory removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nanogit-it-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn write(&self, rel: &str, contents: impl AsRef<[u8]>) {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, contents).unwrap();
    }

    pub fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.0.join(rel)).unwrap()
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.0.join(rel).exists()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Result of running a command.
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub raw: Vec<u8>,
    pub stderr: String,
}

impl Run {
    fn from(o: Output) -> Run {
        Run {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            raw: o.stdout,
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    /// Stdout, asserting that the command succeeded.
    pub fn ok(self) -> String {
        assert_eq!(self.code, 0, "command failed: stdout={:?} stderr={:?}", self.stdout, self.stderr);
        self.stdout
    }
}

fn base_env(cmd: &mut Command) {
    cmd.env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "Test Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
}

/// Run `nanogit` in `dir`.
pub fn ng(dir: &Path, args: &[&str]) -> Run {
    ng_env(dir, args, &[], b"")
}

/// Run `nanogit` with extra environment variables and stdin.
pub fn ng_env(dir: &Path, args: &[&str], env: &[(&str, &str)], stdin: &[u8]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nanogit"));
    cmd.args(args).current_dir(dir);
    base_env(&mut cmd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn nanogit");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    Run::from(child.wait_with_output().unwrap())
}

/// Run real `git` in `dir`.
pub fn git(dir: &Path, args: &[&str]) -> Run {
    git_stdin(dir, args, b"")
}

/// Run real `git` with stdin.
pub fn git_stdin(dir: &Path, args: &[&str], stdin: &[u8]) -> Run {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir);
    base_env(&mut cmd);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn git");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    Run::from(child.wait_with_output().unwrap())
}
