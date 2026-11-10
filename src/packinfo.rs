//! Inspecting packs and the object store: `verify-pack` and `count-objects`.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::cli::{resolve_path, usage_error, Ctx};
use crate::error::{Error, Result};
use crate::object::{hash_object, Kind, Oid};
use crate::pack::{crc32, Base, Pack};
use crate::repo::Repo;
use crate::sha1::sha1;

/// One entry of a verified pack, in pack order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryInfo {
    /// Object id.
    pub oid: Oid,
    /// Type of the (resolved) object.
    pub kind: Kind,
    /// Size from the entry header: the object size, or the delta size.
    pub size: u64,
    /// Bytes the entry occupies in the pack.
    pub packed_size: u64,
    /// Offset of the entry.
    pub offset: u64,
    /// Delta chain length (0 for whole objects) and the direct base.
    pub delta: Option<(usize, Oid)>,
}

/// Check a pack completely: its checksum, that the entries tile the file
/// exactly, every CRC the index records, and that every object resolves
/// (without bases from outside the pack) to the id the index gives it.
pub fn verify(pack: &Pack) -> Result<Vec<EntryInfo>> {
    let data = pack.bytes();
    let end = data.len() - 20;
    if sha1(&data[..end]) != data[end..] {
        return Err(Error::corrupt("pack checksum mismatch"));
    }
    let idx = &pack.index;
    let mut order: Vec<usize> = (0..idx.oids.len()).collect();
    order.sort_by_key(|&i| idx.offsets[i]);
    let by_offset: HashMap<u64, usize> = order.iter().map(|&i| (idx.offsets[i], i)).collect();
    if by_offset.len() != order.len() {
        return Err(Error::corrupt("two index entries share one pack offset"));
    }
    let no_external = |oid: &Oid| -> Result<(Kind, Vec<u8>)> {
        Err(Error::corrupt(format!("delta base {oid} is not in this pack")))
    };
    let mut expect_at = 12u64;
    let mut bases: Vec<Option<usize>> = vec![None; idx.oids.len()];
    let mut infos: Vec<EntryInfo> = Vec::with_capacity(order.len());
    for (n, &i) in order.iter().enumerate() {
        let offset = idx.offsets[i];
        if offset != expect_at {
            return Err(Error::corrupt(format!("pack has unindexed data at offset {expect_at}")));
        }
        let h = pack.header_at(offset)?;
        let (_, stop) = pack.inflate_at(&h)?;
        let next = order.get(n + 1).map_or(end as u64, |&j| idx.offsets[j]);
        if stop as u64 != next {
            return Err(Error::corrupt(format!("entry at offset {offset} does not end where the next begins")));
        }
        if !idx.crcs.is_empty() && crc32(&data[offset as usize..next as usize]) != idx.crcs[i] {
            return Err(Error::corrupt(format!("CRC mismatch for object {}", idx.oids[i])));
        }
        let (kind, body) = pack.read_at(offset, &no_external)?;
        if hash_object(kind, &body) != idx.oids[i] {
            return Err(Error::corrupt(format!("object at offset {offset} does not hash to {}", idx.oids[i])));
        }
        bases[i] = match h.base {
            Base::None => None,
            Base::Offset(b) => Some(
                *by_offset.get(&b).ok_or_else(|| Error::corrupt(format!("delta at {offset} has no base entry")))?,
            ),
            Base::Ref(oid) => Some(idx.position(&oid).ok_or_else(|| Error::corrupt("thin pack: base missing"))?),
        };
        infos.push(EntryInfo { oid: idx.oids[i], kind, size: h.size, packed_size: next - offset, offset, delta: None });
        expect_at = next;
    }
    if expect_at != end as u64 {
        return Err(Error::corrupt("pack has trailing data after its last entry"));
    }
    for (n, &i) in order.iter().enumerate() {
        if let Some(b) = bases[i] {
            let mut depth = 1;
            let mut cur = b;
            while let Some(next) = bases[cur] {
                depth += 1;
                cur = next;
                if depth > order.len() {
                    return Err(Error::corrupt("delta chain is cyclic"));
                }
            }
            infos[n].delta = Some((depth, idx.oids[b]));
        }
    }
    Ok(infos)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Print git's `verify-pack -v` statistics block.
fn write_stats(out: &mut dyn Write, infos: &[EntryInfo]) -> Result<()> {
    let non_delta = infos.iter().filter(|e| e.delta.is_none()).count();
    let mut hist: Vec<usize> = Vec::new();
    for (depth, _) in infos.iter().filter_map(|e| e.delta) {
        if hist.len() < depth {
            hist.resize(depth, 0);
        }
        hist[depth - 1] += 1;
    }
    if non_delta > 0 {
        writeln!(out, "non delta: {non_delta} object{}", plural(non_delta))?;
    }
    for (i, &count) in hist.iter().enumerate() {
        if count > 0 {
            writeln!(out, "chain length = {}: {count} object{}", i + 1, plural(count))?;
        }
    }
    Ok(())
}

fn idx_path_for(arg: &Path) -> PathBuf {
    match arg.extension().and_then(|e| e.to_str()) {
        Some("idx") => arg.to_path_buf(),
        Some("pack") => arg.with_extension("idx"),
        _ => {
            let mut s = arg.as_os_str().to_os_string();
            s.push(".idx");
            PathBuf::from(s)
        }
    }
}

/// `nanogit verify-pack [-v | -s] <pack>...`, output-compatible with git.
pub fn cmd_verify_pack(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    const USAGE: &str = "verify-pack [-v | -s] <pack>.idx...";
    let (mut verbose, mut stat_only) = (false, false);
    let mut packs = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "-s" | "--stat-only" => stat_only = true,
            s if s.starts_with('-') => return usage_error(ctx, USAGE),
            s => packs.push(s.to_string()),
        }
    }
    if packs.is_empty() {
        return usage_error(ctx, USAGE);
    }
    let mut code = 0;
    for arg in &packs {
        let idx = idx_path_for(Path::new(arg));
        let shown = {
            let a = Path::new(arg);
            match a.extension().and_then(|e| e.to_str()) {
                Some("idx") | Some("pack") => a.with_extension("pack"),
                _ => idx.with_extension("pack"),
            }
        };
        let result = Pack::open(&resolve_path(ctx.cwd, &idx.to_string_lossy())).and_then(|p| verify(&p));
        match result {
            Ok(infos) => {
                if verbose && !stat_only {
                    for e in &infos {
                        write!(ctx.out, "{} {:<6} {} {} {}", e.oid, e.kind.as_str(), e.size, e.packed_size, e.offset)?;
                        if let Some((depth, base)) = e.delta {
                            write!(ctx.out, " {depth} {base}")?;
                        }
                        writeln!(ctx.out)?;
                    }
                }
                if verbose || stat_only {
                    write_stats(ctx.out, &infos)?;
                }
                if verbose && !stat_only {
                    writeln!(ctx.out, "{}: ok", shown.display())?;
                }
            }
            Err(e) => {
                writeln!(ctx.err, "error: {e}")?;
                if verbose {
                    writeln!(ctx.out, "{}: bad", shown.display())?;
                }
                code = 1;
            }
        }
    }
    Ok(code)
}

/// Loose and packed object counts, as `git count-objects -v` reports them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectCounts {
    /// Loose objects.
    pub count: usize,
    /// Disk usage of loose objects, in bytes (allocated blocks).
    pub size: u64,
    /// Objects in packs.
    pub in_pack: usize,
    /// Number of packs.
    pub packs: usize,
    /// Bytes used by `.pack` and `.idx` files.
    pub size_pack: u64,
    /// Loose objects that are also in a pack.
    pub prune_packable: usize,
    /// Files in the object store that are not objects or packs.
    pub garbage: usize,
    /// Bytes used by those files.
    pub size_garbage: u64,
}

/// Count what `objects/` holds.
pub fn count_objects(repo: &Repo) -> Result<ObjectCounts> {
    let odb = repo.odb();
    let mut c = ObjectCounts::default();
    let packs = odb.packs()?;
    if let Ok(top) = fs::read_dir(odb.dir()) {
        for d in top.flatten() {
            let name = d.file_name().to_string_lossy().into_owned();
            let is_fanout = name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit());
            if !is_fanout || !d.path().is_dir() {
                continue;
            }
            for f in fs::read_dir(d.path())?.flatten() {
                let fname = f.file_name().to_string_lossy().into_owned();
                let meta = f.metadata()?;
                match Oid::from_hex(&format!("{name}{fname}")).filter(|_| fname.len() == 38) {
                    Some(oid) => {
                        c.count += 1;
                        c.size += meta.blocks() * 512;
                        if packs.iter().any(|p| p.contains(&oid)) {
                            c.prune_packable += 1;
                        }
                    }
                    None if fname.starts_with("tmp_obj_") => {}
                    None => {
                        c.garbage += 1;
                        c.size_garbage += meta.len();
                    }
                }
            }
        }
    }
    for p in packs {
        c.packs += 1;
        c.in_pack += p.index.oids.len();
        c.size_pack += fs::metadata(&p.path)?.len() + fs::metadata(p.path.with_extension("idx"))?.len();
    }
    if let Ok(entries) = fs::read_dir(odb.dir().join("pack")) {
        for e in entries.flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            let paired = match ext {
                "pack" => p.with_extension("idx").is_file(),
                "idx" => p.with_extension("pack").is_file(),
                "keep" | "rev" | "promisor" | "mtimes" | "bitmap" => p.with_extension("pack").is_file(),
                _ => false,
            };
            if !paired {
                c.garbage += 1;
                c.size_garbage += e.metadata()?.len();
            }
        }
    }
    Ok(c)
}

/// `nanogit count-objects [-v]`.
pub fn cmd_count_objects(args: &[String], ctx: &mut Ctx<'_>) -> Result<i32> {
    let mut verbose = false;
    for arg in args {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            _ => return usage_error(ctx, "count-objects [-v]"),
        }
    }
    let repo = Repo::discover(ctx.cwd)?;
    let c = count_objects(&repo)?;
    if verbose {
        writeln!(ctx.out, "count: {}", c.count)?;
        writeln!(ctx.out, "size: {}", c.size / 1024)?;
        writeln!(ctx.out, "in-pack: {}", c.in_pack)?;
        writeln!(ctx.out, "packs: {}", c.packs)?;
        writeln!(ctx.out, "size-pack: {}", c.size_pack / 1024)?;
        writeln!(ctx.out, "prune-packable: {}", c.prune_packable)?;
        writeln!(ctx.out, "garbage: {}", c.garbage)?;
        writeln!(ctx.out, "size-garbage: {}", c.size_garbage / 1024)?;
    } else {
        writeln!(ctx.out, "{} objects, {} kilobytes", c.count, c.size / 1024)?;
    }
    Ok(0)
}
