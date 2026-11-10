//! Writing packs and packed refs: `repack`, `pack-refs` and `gc`.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::cli::{usage_error, Ctx};
use crate::error::{Error, Result};
use crate::index::Index;
use crate::object::{mode, Kind, Object, ObjectStore, Oid};
use crate::pack::{self, name_hash, Pack, PackInput, PackOptions};
use crate::repo::Repo;
use crate::sha1::to_hex;

/// Every object reachable from refs, `HEAD`, merge state and the index, plus
/// everything already packed (so repacking never loses an object).
pub fn collect_objects(repo: &Repo, odb: &ObjectStore) -> Result<Vec<PackInput>> {
    let mut tips: Vec<(Oid, String)> = Vec::new();
    for (_, oid) in repo.list_refs("refs/")? {
        tips.push((oid, String::new()));
    }
    if let Some(head) = repo.head_oid()? {
        tips.push((head, String::new()));
    }
    for name in ["ORIG_HEAD", "MERGE_HEAD"] {
        if let Ok(text) = fs::read_to_string(repo.git_dir.join(name)) {
            if let Some(oid) = Oid::from_hex(text.trim()) {
                tips.push((oid, String::new()));
            }
        }
    }
    let index_path = repo.git_dir.join("index");
    if index_path.is_file() {
        for e in Index::read(&index_path)?.entries {
            if e.mode != mode::GITLINK {
                tips.push((e.oid, e.path.clone()));
            }
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut stack = tips;
    while let Some((oid, path)) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        let (kind, body) = odb
            .read_raw(&oid)
            .map_err(|e| Error::fatal(format!("cannot read {oid} (needed by a ref or the index): {e}")))?;
        match Object::parse(kind, &body)? {
            Object::Commit(c) => {
                stack.push((c.tree, String::new()));
                stack.extend(c.parents.iter().map(|p| (*p, String::new())));
            }
            Object::Tree(entries) => {
                for e in entries {
                    if e.mode == mode::GITLINK {
                        continue;
                    }
                    let child = if path.is_empty() { e.name.clone() } else { format!("{path}/{}", e.name) };
                    stack.push((e.oid, child));
                }
            }
            Object::Tag(t) => stack.push((t.object, String::new())),
            Object::Blob(_) => {}
        }
        let hash = if path.is_empty() { 0 } else { name_hash(&path) };
        out.push(PackInput { oid, kind, body, name_hash: hash });
    }
    for p in odb.packs()? {
        for oid in &p.index.oids {
            if seen.insert(*oid) {
                let (kind, body) = odb.read_raw(oid)?;
                out.push(PackInput { oid: *oid, kind, body, name_hash: 0 });
            }
        }
    }
    Ok(out)
}

/// Summary of a [`repack`] run.
#[derive(Clone, Debug, Default)]
pub struct RepackStats {
    /// Objects written to the new pack.
    pub objects: usize,
    /// How many of them are deltas.
    pub deltas: usize,
    /// File name of the new pack (`pack-<sha>.pack`), if any objects existed.
    pub pack_name: Option<String>,
    /// Loose objects removed (`-d`).
    pub removed_loose: usize,
    /// Old packs removed (`-d`).
    pub removed_packs: usize,
}

fn write_read_only(path: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    fs::write(&tmp, data)?;
    let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o444));
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Pack every object into a single new pack.  With `delete`, remove the old
/// packs and the loose objects that the new pack now holds.
pub fn repack(repo: &Repo, opts: PackOptions, delete: bool) -> Result<RepackStats> {
    let mut odb = repo.odb();
    let objects = collect_objects(repo, &odb)?;
    let mut stats = RepackStats::default();
    if objects.is_empty() {
        return Ok(stats);
    }
    let written = pack::write_pack(objects, opts);
    let base = format!("pack-{}", to_hex(&written.checksum));
    let dir = odb.dir().join("pack");
    fs::create_dir_all(&dir)?;
    let pack_path = dir.join(format!("{base}.pack"));
    let idx_path = dir.join(format!("{base}.idx"));
    if !(pack_path.is_file() && idx_path.is_file()) {
        write_read_only(&pack_path, &written.pack)?;
        write_read_only(&idx_path, &written.index)?;
    }
    // Re-read the new pack and check every object before deleting anything.
    let fresh = Pack::open(&idx_path)?;
    let no_external = |oid: &Oid| -> Result<(Kind, Vec<u8>)> {
        Err(Error::corrupt(format!("new pack refers to {oid} outside itself")))
    };
    for (i, oid) in fresh.index.oids.iter().enumerate() {
        let (kind, body) = fresh.read_at(fresh.index.offsets[i], &no_external)?;
        if crate::object::hash_object(kind, &body) != *oid {
            return Err(Error::corrupt(format!("new pack stores {oid} incorrectly")));
        }
    }
    stats.objects = fresh.index.oids.len();
    stats.deltas = written.deltas;
    stats.pack_name = Some(format!("{base}.pack"));
    if delete {
        for p in odb.packs()? {
            let keep = p.path.with_extension("keep");
            if p.path == pack_path || keep.exists() {
                continue;
            }
            fs::remove_file(p.path.with_extension("idx"))?;
            fs::remove_file(&p.path)?;
            let _ = fs::remove_file(p.path.with_extension("rev"));
            stats.removed_packs += 1;
        }
        for oid in odb.loose_oids()? {
            if fresh.contains(&oid) {
                let hex = oid.to_hex();
                fs::remove_file(odb.dir().join(&hex[..2]).join(&hex[2..]))?;
                stats.removed_loose += 1;
            }
        }
        if let Ok(entries) = fs::read_dir(odb.dir()) {
            for e in entries.flatten() {
                if e.file_name().len() == 2 {
                    let _ = fs::remove_dir(e.path());
                }
            }
        }
        odb.reload_packs();
    }
    Ok(stats)
}

/// Move refs into `packed-refs` (tags only, or every ref with `all`), with
/// annotated tags peeled.  Returns how many refs were packed.
pub fn pack_refs(repo: &Repo, all: bool) -> Result<usize> {
    let odb = repo.odb();
    let refs = repo.list_refs("refs/")?;
    let mut text = String::from("# pack-refs with: peeled fully-peeled sorted \n");
    let mut moved = Vec::new();
    for (name, oid) in &refs {
        let loose = repo.git_dir.join(name).is_file();
        let wanted = all || name.starts_with("refs/tags/");
        if loose && !wanted {
            // Stays loose; any stale packed copy it shadows is dropped.
            continue;
        }
        text.push_str(&format!("{oid} {name}\n"));
        let mut peeled = *oid;
        let mut is_tag = false;
        while let Ok(Object::Tag(t)) = odb.read(&peeled) {
            peeled = t.object;
            is_tag = true;
        }
        if is_tag {
            text.push_str(&format!("^{peeled}\n"));
        }
        if loose {
            moved.push(name.clone());
        }
    }
    let path = repo.git_dir.join("packed-refs");
    let lock = repo.git_dir.join("packed-refs.lock");
    match fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
        Ok(mut f) => f.write_all(text.as_bytes())?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::fatal(format!(
                "unable to create '{}': File exists (another process running?)",
                lock.display()
            )));
        }
        Err(e) => return Err(e.into()),
    }
    fs::rename(&lock, &path)?;
    let stop = repo.git_dir.join("refs");
    for name in &moved {
        let p = repo.git_dir.join(name);
        fs::remove_file(&p)?;
        let mut dir = p.parent();
        while let Some(d) = dir {
            // Keep the standard top-level directories git creates.
            if d == stop || d == stop.join("heads") || d == stop.join("tags") || fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
    Ok(moved.len())
}

fn parse_num(arg: &str, flag: &str, it: &mut std::slice::Iter<'_, String>) -> Result<Option<usize>> {
    let value = if let Some(v) = arg.strip_prefix(&format!("{flag}=")) {
        v.to_string()
    } else if arg == flag {
        it.next().ok_or_else(|| Error::fatal(format!("option '{flag}' needs a value")))?.clone()
    } else {
        return Ok(None);
    };
    value
        .parse()
        .map(Some)
        .map_err(|_| Error::fatal(format!("option '{flag}' expects a number, got '{value}'")))
}

fn report(ctx: &mut Ctx<'_>, stats: &RepackStats) -> Result<()> {
    match &stats.pack_name {
        Some(name) => writeln!(ctx.out, "Total {} (delta {}), {name}", stats.objects, stats.deltas)?,
        None => writeln!(ctx.out, "Nothing new to pack.")?,
    }
    if stats.removed_loose + stats.removed_packs > 0 {
        writeln!(
            ctx.out,
            "Removed {} loose object{} and {} old pack{}",
            stats.removed_loose,
            if stats.removed_loose == 1 { "" } else { "s" },
            stats.removed_packs,
            if stats.removed_packs == 1 { "" } else { "s" }
        )?;
    }
    Ok(())
}

const REPACK_USAGE: &str = "repack [-d] [--window=<n>] [--depth=<n>]";

/// `nanogit repack`.
pub fn cmd_repack(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    let mut opts = PackOptions::default();
    let mut delete = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "-d" {
            delete = true;
        } else if arg == "-a" || arg == "-ad" || arg == "-q" {
            // Always packs everything; accepted for git muscle memory.
            delete |= arg == "-ad";
        } else if let Some(w) = parse_num(arg, "--window", &mut it)? {
            opts.window = w;
        } else if let Some(d) = parse_num(arg, "--depth", &mut it)? {
            opts.depth = d;
        } else {
            return usage_error(ctx, REPACK_USAGE);
        }
    }
    let repo = Repo::discover(ctx.cwd)?;
    let stats = repack(&repo, opts, delete)?;
    report(ctx, &stats)?;
    Ok(0)
}

/// `nanogit pack-refs`.
pub fn cmd_pack_refs(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    let mut all = false;
    for arg in args {
        match arg.as_str() {
            "--all" => all = true,
            _ => return usage_error(ctx, "pack-refs [--all]"),
        }
    }
    let repo = Repo::discover(ctx.cwd)?;
    pack_refs(&repo, all)?;
    Ok(0)
}

/// `nanogit gc`: pack every ref, then repack everything into one pack and
/// delete what it replaces.
pub fn cmd_gc(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    let mut opts = PackOptions::default();
    let mut quiet = false;
    for arg in args {
        match arg.as_str() {
            "--aggressive" => opts = PackOptions { window: 250, depth: 50 },
            "-q" | "--quiet" => quiet = true,
            _ => return usage_error(ctx, "gc [--aggressive] [-q]"),
        }
    }
    let repo = Repo::discover(ctx.cwd)?;
    pack_refs(&repo, true)?;
    let stats = repack(&repo, opts, true)?;
    if !quiet {
        report(ctx, &stats)?;
    }
    Ok(0)
}
