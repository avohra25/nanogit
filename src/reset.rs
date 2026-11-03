//! `nanogit reset [--soft | --mixed | --hard] [<commit>] [-- <path>...]`

use std::collections::BTreeSet;

use crate::cli::{usage_error, Ctx};
use crate::error::{Error, Result};
use crate::history::{peel_to_commit, read_commit};
use crate::index::{Index, IndexEntry};
use crate::merge::conflict_strays;
use crate::object::Oid;
use crate::repo::Repo;
use crate::tree::Flat;
use crate::workdir::pathspec_matches;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Soft,
    Mixed,
    Hard,
}

/// Replace the index by `target`, keeping cached stat data of unchanged entries.
fn index_from_flat(old: &Index, target: &Flat) -> Index {
    let mut index = Index::new();
    for (path, e) in target {
        match old.get(path) {
            Some(prev) if prev.stage == 0 && prev.oid == e.oid && prev.mode == e.mode => index.add(prev.clone()),
            _ => index.add(IndexEntry::new(path, e.mode, e.oid)),
        }
    }
    index
}

/// `reset [--soft | --mixed | --hard] [<commit>] [--] [<path>...]`
pub fn reset(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "reset [--soft | --mixed | --hard] [<commit>] [--] [<path>...]";
    let mut mode = None;
    let mut operands: Vec<&String> = Vec::new();
    let mut paths_only = false;
    let mut explicit_paths: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            _ if paths_only => explicit_paths.push(a),
            "--" => paths_only = true,
            "--soft" => mode = Some(Mode::Soft),
            "--mixed" => mode = Some(Mode::Mixed),
            "--hard" => mode = Some(Mode::Hard),
            "-q" | "--quiet" => {}
            s if s.starts_with('-') => return usage_error(ctx, USAGE),
            _ => operands.push(a),
        }
    }
    let repo = Repo::discover(ctx.cwd)?;
    let odb = repo.odb();
    // The first operand is a commit when it resolves to one (or when `--` follows).
    let mut rev: Option<Oid> = None;
    let mut rest: Vec<&String> = operands;
    if let Some(first) = rest.first() {
        match repo.rev_parse(first).and_then(|o| peel_to_commit(&odb, &o)) {
            Ok(oid) => {
                rev = Some(oid);
                rest.remove(0);
            }
            Err(_) if paths_only => {
                return Err(Error::fatal(format!("ambiguous argument '{first}': unknown revision")));
            }
            Err(_) => {}
        }
    }
    let mut specs: Vec<String> = Vec::new();
    for p in rest.into_iter().chain(explicit_paths) {
        specs.push(repo.rel_path(ctx.cwd, p)?);
    }
    let head = repo.head_oid()?;
    let mut index = repo.load_index()?;
    if !specs.is_empty() {
        if mode.is_some_and(|m| m != Mode::Mixed) {
            return Err(Error::fatal("Cannot do a soft or hard reset with paths."));
        }
        let target = match rev.or(head) {
            Some(c) => repo.commit_flat(&c)?,
            None => Flat::new(),
        };
        let current: BTreeSet<String> = index.entries.iter().map(|e| e.path.clone()).collect();
        let paths: BTreeSet<&String> = current.iter().chain(target.keys()).collect();
        for path in paths {
            if !specs.iter().any(|s| pathspec_matches(s, path)) {
                continue;
            }
            let prev = index.get(path).filter(|e| e.stage == 0).cloned();
            index.remove(path);
            if let Some(e) = target.get(path) {
                match prev {
                    Some(p) if p.oid == e.oid && p.mode == e.mode => index.add(p),
                    _ => index.add(IndexEntry::new(path, e.mode, e.oid)),
                }
            }
        }
        repo.save_index(&index)?;
        return Ok(0);
    }
    let mode = mode.unwrap_or(Mode::Mixed);
    let target = match rev.or(head) {
        Some(c) => c,
        None if mode == Mode::Mixed => {
            repo.save_index(&Index::new())?;
            return Ok(0);
        }
        None => return Err(Error::fatal("Failed to resolve 'HEAD' as a valid ref.")),
    };
    if mode == Mode::Soft && repo.merge_head()?.is_some() {
        return Err(Error::fatal("Cannot do a soft reset in the middle of a merge."));
    }
    let flat = repo.commit_flat(&target)?;
    match mode {
        Mode::Soft => {}
        Mode::Mixed => repo.save_index(&index_from_flat(&index, &flat))?,
        Mode::Hard => {
            let stray = conflict_strays(&index, &flat);
            repo.switch_tree(&mut index, &flat, &flat, true)?;
            repo.remove_strays(&stray)?;
            repo.save_index(&index)?;
        }
    }
    if let Some(old) = head {
        std::fs::write(repo.git_dir.join("ORIG_HEAD"), format!("{old}\n"))?;
    }
    repo.update_head(&target)?;
    repo.clear_merge_state()?;
    if mode == Mode::Hard {
        let subject = read_commit(&odb, &target)?.subject().to_string();
        writeln!(ctx.out, "HEAD is now at {} {subject}", target.short(7))?;
    }
    Ok(0)
}
