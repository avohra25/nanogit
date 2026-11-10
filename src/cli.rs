//! The `nanogit` command line.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::checkout;
use crate::cmds;
use crate::merge;
use crate::reset;
use crate::error::{Error, Result};
use crate::object::{hash_object, Kind, Object};
use crate::repo::Repo;
use crate::difftool;
use crate::status;

/// Everything a command needs from its environment.
pub struct Ctx<'a> {
    /// Directory the command was started in.
    pub cwd: &'a Path,
    /// Standard output.
    pub out: &'a mut dyn Write,
    /// Standard error.
    pub err: &'a mut dyn Write,
    /// Standard input.
    pub input: &'a mut dyn Read,
}

const USAGE: &str = "usage: nanogit <command> [<args>]

Commands:
  init [-b <branch>] [<dir>]       create an empty repository
  hash-object [-w] [-t <type>] (--stdin | <file>...)
  cat-file (-t | -s | -p | -e | <type>) <object>
  add [-A | -u] [-f] [--] <path>...  stage files, directories and deletions
  rm [-r] [-f] [--cached] <path>...
  ls-files [-s] [-o] [-i] [--exclude-standard] [<path>...]
  check-ignore [-q] <path>...      show which paths .gitignore excludes
  status [-s | --porcelain] [-b]   show the working tree status
  branch [-v] [<name> [<start>]] | -d/-D <name> | -m [<old>] <new>
  checkout [-f] [-b <new>] [--detach] <branch|commit>  |  checkout [<commit>] -- <path>...
  diff [--cached] [-U<n>] [--name-only | --name-status] [<commit> [<commit>]] [--] [<path>...]
  merge [--no-ff | --ff-only] [--no-commit] [-m <msg>] <commit>  |  merge --abort
  merge-base [--all | --is-ancestor] <commit> <commit>
  reset [--soft | --mixed | --hard] [<commit>] [--] [<path>...]
  write-tree                       write the index as a tree object
  commit [-a] [--allow-empty] -m <message>
  log [--oneline] [--all] [-n <N>] [<rev>...]
  rev-parse [--short[=N]] <rev>... also --show-toplevel, --git-dir
  config [--list | <key> [<value>]]
  repack [-d] [--window=<n>] [--depth=<n>]  pack all objects (with deltas) into one pack
  pack-refs [--all]                move tags (or all refs) into packed-refs
  gc [--aggressive] [-q]           pack-refs --all, then repack -d
";

/// Run one command line and return the process exit code.
pub fn run(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    let Some((cmd, rest)) = args.split_first() else {
        write!(ctx.err, "{USAGE}")?;
        return Ok(129);
    };
    match cmd.as_str() {
        "init" => cmd_init(rest, ctx),
        "hash-object" => cmd_hash_object(rest, ctx),
        "cat-file" => cmd_cat_file(rest, ctx),
        "add" => cmds::add(rest, ctx),
        "rm" => cmds::rm(rest, ctx),
        "ls-files" => cmds::ls_files(rest, ctx),
        "write-tree" => cmds::write_tree(rest, ctx),
        "commit" => cmds::commit(rest, ctx),
        "log" => cmds::log(rest, ctx),
        "check-ignore" => cmds::check_ignore(rest, ctx),
        "diff" => difftool::diff(rest, ctx),
        "branch" => checkout::branch(rest, ctx),
        "checkout" => checkout::checkout(rest, ctx),
        "status" => status::status(rest, ctx),
        "merge" => merge::merge(rest, ctx),
        "merge-base" => merge::merge_base(rest, ctx),
        "reset" => reset::reset(rest, ctx),
        "rev-parse" => cmds::rev_parse(rest, ctx),
        "config" => cmds::config(rest, ctx),
        "repack" => crate::repack::cmd_repack(rest, ctx),
        "pack-refs" => crate::repack::cmd_pack_refs(rest, ctx),
        "gc" => crate::repack::cmd_gc(rest, ctx),
        "help" | "--help" | "-h" => {
            write!(ctx.out, "{USAGE}")?;
            Ok(0)
        }
        other => {
            writeln!(ctx.err, "nanogit: '{other}' is not a nanogit command.\n")?;
            write!(ctx.err, "{USAGE}")?;
            Ok(129)
        }
    }
}

pub(crate) fn usage_error(ctx: &mut Ctx<'_>, line: &str) -> Result<i32> {
    writeln!(ctx.err, "usage: nanogit {line}")?;
    Ok(129)
}

pub(crate) fn resolve_path(cwd: &Path, arg: &str) -> PathBuf {
    let p = Path::new(arg);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

fn cmd_init(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    let mut branch = "main".to_string();
    let mut dir = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-b" | "--initial-branch" => {
                branch = it.next().ok_or_else(|| Error::fatal("option '-b' needs a branch name"))?.clone();
            }
            s if s.starts_with('-') => return usage_error(ctx, "init [-b <branch>] [<dir>]"),
            s => dir = Some(s),
        }
    }
    let target = dir.map_or_else(|| ctx.cwd.to_path_buf(), |d| resolve_path(ctx.cwd, d));
    fs::create_dir_all(&target)?;
    let (repo, existed) = Repo::init(&target, &branch)?;
    let verb = if existed { "Reinitialized existing" } else { "Initialized empty" };
    writeln!(ctx.out, "{verb} Git repository in {}/", repo.git_dir.display())?;
    Ok(0)
}

fn cmd_hash_object(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    let (mut write, mut stdin, mut kind) = (false, false, Kind::Blob);
    let mut files = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-w" => write = true,
            "--stdin" => stdin = true,
            "-t" => {
                let name = it.next().ok_or_else(|| Error::fatal("option '-t' needs a type"))?;
                kind = Kind::parse(name).ok_or_else(|| Error::fatal(format!("invalid object type \"{name}\"")))?;
            }
            s if s.starts_with('-') && s != "-" => {
                return usage_error(ctx, "hash-object [-w] [-t <type>] (--stdin | <file>...)");
            }
            s => files.push(s.to_string()),
        }
    }
    if !stdin && files.is_empty() {
        return usage_error(ctx, "hash-object [-w] [-t <type>] (--stdin | <file>...)");
    }
    let repo = if write { Some(Repo::discover(ctx.cwd)?) } else { None };
    let mut inputs: Vec<Vec<u8>> = Vec::new();
    if stdin {
        let mut data = Vec::new();
        ctx.input.read_to_end(&mut data)?;
        inputs.push(data);
    }
    for f in &files {
        let path = resolve_path(ctx.cwd, f);
        let data = fs::read(&path).map_err(|e| Error::fatal(format!("could not open '{f}': {e}")))?;
        inputs.push(data);
    }
    for data in inputs {
        let oid = match &repo {
            Some(r) => r.odb().write(kind, &data)?,
            None => hash_object(kind, &data),
        };
        writeln!(ctx.out, "{oid}")?;
    }
    Ok(0)
}

fn cmd_cat_file(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE_LINE: &str = "cat-file (-t | -s | -p | -e | <type>) <object>";
    let [mode_arg, spec] = args else {
        return usage_error(ctx, USAGE_LINE);
    };
    let repo = Repo::discover(ctx.cwd)?;
    let oid = match repo.rev_parse(spec) {
        Ok(oid) => oid,
        Err(e) if mode_arg == "-e" => {
            let _ = e;
            return Ok(1);
        }
        Err(e) => return Err(e),
    };
    let odb = repo.odb();
    if mode_arg == "-e" {
        return Ok(if odb.contains(&oid) { 0 } else { 1 });
    }
    let (kind, body) = odb.read_raw(&oid)?;
    match mode_arg.as_str() {
        "-t" => writeln!(ctx.out, "{}", kind.as_str())?,
        "-s" => writeln!(ctx.out, "{}", body.len())?,
        "-p" => pretty_print(ctx, kind, &body)?,
        want => {
            let want_kind =
                Kind::parse(want).ok_or_else(|| Error::fatal(format!("invalid object type \"{want}\"")))?;
            if want_kind != kind {
                return Err(Error::fatal(format!("{oid} is a {}, not a {want}", kind.as_str())));
            }
            ctx.out.write_all(&body)?;
        }
    }
    Ok(0)
}

fn pretty_print(ctx: &mut Ctx<'_>, kind: Kind, body: &[u8]) -> Result<()> {
    match Object::parse(kind, body)? {
        Object::Tree(entries) => {
            for e in entries {
                writeln!(ctx.out, "{:06o} {} {}\t{}", e.mode, e.kind().as_str(), e.oid, e.name)?;
            }
        }
        _ => ctx.out.write_all(body)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{mode, TreeEntry};

    fn run_in(dir: &Path, args: &[&str], stdin: &[u8]) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut input = stdin;
        let mut ctx = Ctx { cwd: dir, out: &mut out, err: &mut err, input: &mut input };
        let code = match run(&args, &mut ctx) {
            Ok(c) => c,
            Err(e) => {
                let _ = writeln!(ctx.err, "fatal: {e}");
                128
            }
        };
        (code, String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&err).into_owned())
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nanogit-cli-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn hash_object_without_repo() {
        let dir = temp("hash");
        let (code, out, _) = run_in(&dir, &["hash-object", "--stdin"], b"hello\n");
        assert_eq!(code, 0);
        assert_eq!(out.trim(), "ce013625030ba8dba906f756967f9e9ca394464a");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn init_hash_write_and_cat_file() {
        let dir = temp("flow");
        let (code, out, _) = run_in(&dir, &["init"], b"");
        assert_eq!(code, 0);
        assert!(out.starts_with("Initialized empty Git repository in "));
        let (_, out, _) = run_in(&dir, &["init"], b"");
        assert!(out.starts_with("Reinitialized"));

        fs::write(dir.join("a.txt"), "hello\n").unwrap();
        let (_, out, _) = run_in(&dir, &["hash-object", "-w", "a.txt"], b"");
        let oid = out.trim().to_string();
        assert_eq!(oid, "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(run_in(&dir, &["cat-file", "-t", &oid], b"").1, "blob\n");
        assert_eq!(run_in(&dir, &["cat-file", "-s", &oid], b"").1, "6\n");
        assert_eq!(run_in(&dir, &["cat-file", "-p", &oid[..10]], b"").1, "hello\n");
        assert_eq!(run_in(&dir, &["cat-file", "blob", &oid], b"").1, "hello\n");
        assert_eq!(run_in(&dir, &["cat-file", "-e", &oid], b"").0, 0);
        assert_eq!(run_in(&dir, &["cat-file", "-e", "0123456789abcdef0123456789abcdef01234567"], b"").0, 1);
        let (code, _, err) = run_in(&dir, &["cat-file", "tree", &oid], b"");
        assert_eq!(code, 128);
        assert!(err.contains("is a blob, not a tree"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tree_pretty_printing() {
        let dir = temp("tree");
        run_in(&dir, &["init"], b"");
        let repo = Repo::discover(&dir).unwrap();
        let blob = repo.odb().write(Kind::Blob, b"x").unwrap();
        let tree = Object::Tree(vec![TreeEntry {
            mode: mode::FILE,
            name: "x.txt".into(),
            oid: blob,
        }]);
        let tid = repo.odb().write_object(&tree).unwrap();
        let (_, out, _) = run_in(&dir, &["cat-file", "-p", &tid.to_hex()], b"");
        assert_eq!(out, format!("100644 blob {blob}\tx.txt\n"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn usage_and_unknown_commands() {
        let dir = temp("usage");
        assert_eq!(run_in(&dir, &[], b"").0, 129);
        assert_eq!(run_in(&dir, &["frobnicate"], b"").0, 129);
        assert_eq!(run_in(&dir, &["help"], b"").0, 0);
        assert_eq!(run_in(&dir, &["cat-file"], b"").0, 129);
        assert_eq!(run_in(&dir, &["hash-object"], b"").0, 129);
        fs::remove_dir_all(&dir).unwrap();
    }
}
