//! Reading git packfiles (`objects/pack/pack-*.pack` plus their `.idx`).
//!
//! A pack is `"PACK"`, a version (2 or 3), an object count, the entries, and a
//! SHA-1 of everything before it.  Each entry has a small header (type and
//! inflated size), then either a zlib-compressed object or a delta against a
//! base that is named by offset (`OFS_DELTA`) or by id (`REF_DELTA`).  The
//! `.idx` file (version 2, or the older version 1) maps sorted object ids to
//! entry offsets so lookups are a binary search.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::delta;
use crate::error::{Error, Result};
use crate::object::{Kind, Oid};
use crate::sha1::sha1;
use crate::zlib::zlib_decompress_prefix;

/// Pack entry type codes.
pub mod entry {
    /// A commit.
    pub const COMMIT: u8 = 1;
    /// A tree.
    pub const TREE: u8 = 2;
    /// A blob.
    pub const BLOB: u8 = 3;
    /// An annotated tag.
    pub const TAG: u8 = 4;
    /// A delta whose base is an earlier entry, given as a negative offset.
    pub const OFS_DELTA: u8 = 6;
    /// A delta whose base is given by object id.
    pub const REF_DELTA: u8 = 7;
}

/// Deltas deeper than this are treated as corrupt (git itself caps at 4095).
const MAX_CHAIN: usize = 10_000;

/// Pack type code for an object kind.
pub fn type_code(kind: Kind) -> u8 {
    match kind {
        Kind::Commit => entry::COMMIT,
        Kind::Tree => entry::TREE,
        Kind::Blob => entry::BLOB,
        Kind::Tag => entry::TAG,
    }
}

fn kind_of(code: u8) -> Option<Kind> {
    match code {
        entry::COMMIT => Some(Kind::Commit),
        entry::TREE => Some(Kind::Tree),
        entry::BLOB => Some(Kind::Blob),
        entry::TAG => Some(Kind::Tag),
        _ => None,
    }
}

fn be32(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| Error::corrupt("pack index is truncated"))
}

/// A parsed `.idx` file.
#[derive(Clone, Debug)]
pub struct PackIndex {
    /// Index format version (1 or 2).
    pub version: u32,
    /// Object ids in ascending order.
    pub oids: Vec<Oid>,
    /// Entry offset of `oids[i]` in the pack.
    pub offsets: Vec<u64>,
    /// CRC-32 of each packed entry (version 2 only).
    pub crcs: Vec<u32>,
    /// The pack's trailing checksum, as recorded in the index.
    pub pack_checksum: [u8; 20],
}

impl PackIndex {
    /// Parse an index file and verify its own checksum.
    pub fn parse(data: &[u8]) -> Result<PackIndex> {
        if data.len() < 40 {
            return Err(Error::corrupt("pack index is truncated"));
        }
        let body = &data[..data.len() - 20];
        if sha1(body) != data[data.len() - 20..] {
            return Err(Error::corrupt("pack index checksum mismatch"));
        }
        let mut pack_checksum = [0u8; 20];
        pack_checksum.copy_from_slice(&data[data.len() - 40..data.len() - 20]);
        let (version, fan_at) = if data.starts_with(b"\xfftOc") {
            let v = be32(data, 4)?;
            if v != 2 {
                return Err(Error::corrupt(format!("unsupported pack index version {v}")));
            }
            (2, 8)
        } else {
            (1, 0)
        };
        let mut fanout = [0u32; 256];
        for (i, f) in fanout.iter_mut().enumerate() {
            *f = be32(data, fan_at + 4 * i)?;
        }
        if fanout.windows(2).any(|w| w[0] > w[1]) {
            return Err(Error::corrupt("pack index fan-out table is not monotonic"));
        }
        let n = fanout[255] as usize;
        let mut oids = Vec::with_capacity(n);
        let mut offsets = Vec::with_capacity(n);
        let mut crcs = Vec::new();
        let at = fan_at + 1024;
        if version == 1 {
            let need = at + n * 24 + 40;
            if data.len() != need {
                return Err(Error::corrupt("pack index has the wrong size"));
            }
            for i in 0..n {
                let e = at + i * 24;
                offsets.push(u64::from(be32(data, e)?));
                oids.push(Oid::from_slice(&data[e + 4..e + 24]).expect("20 bytes"));
            }
        } else {
            let sha_at = at;
            let crc_at = sha_at + n * 20;
            let off_at = crc_at + n * 4;
            let large_at = off_at + n * 4;
            if data.len() < large_at + 40 {
                return Err(Error::corrupt("pack index is truncated"));
            }
            let large_count = (data.len() - 40 - large_at) / 8;
            if (data.len() - 40 - large_at) % 8 != 0 {
                return Err(Error::corrupt("pack index has the wrong size"));
            }
            for i in 0..n {
                oids.push(Oid::from_slice(&data[sha_at + i * 20..sha_at + i * 20 + 20]).expect("20 bytes"));
                crcs.push(be32(data, crc_at + i * 4)?);
                let off = be32(data, off_at + i * 4)?;
                if off & 0x8000_0000 != 0 {
                    let k = (off & 0x7fff_ffff) as usize;
                    if k >= large_count {
                        return Err(Error::corrupt("pack index large offset out of range"));
                    }
                    let hi = u64::from(be32(data, large_at + k * 8)?);
                    let lo = u64::from(be32(data, large_at + k * 8 + 4)?);
                    offsets.push(hi << 32 | lo);
                } else {
                    offsets.push(u64::from(off));
                }
            }
        }
        for w in oids.windows(2) {
            if w[0] >= w[1] {
                return Err(Error::corrupt("pack index object ids are not sorted"));
            }
        }
        for (b, &f) in fanout.iter().enumerate() {
            let below = oids.partition_point(|o| usize::from(o.as_bytes()[0]) <= b);
            if below != f as usize {
                return Err(Error::corrupt("pack index fan-out table does not match its ids"));
            }
        }
        Ok(PackIndex { version, oids, offsets, crcs, pack_checksum })
    }

    /// Position of `oid` in the sorted id table.
    pub fn position(&self, oid: &Oid) -> Option<usize> {
        self.oids.binary_search(oid).ok()
    }

    /// Ids starting with the hex `prefix` (at least one character).
    pub fn with_prefix(&self, prefix: &str) -> Vec<Oid> {
        let lo_hex = format!("{prefix:0<40}");
        let Some(lo) = Oid::from_hex(&lo_hex) else { return Vec::new() };
        let start = self.oids.partition_point(|o| *o < lo);
        self.oids[start..]
            .iter()
            .take_while(|o| o.to_hex().starts_with(prefix))
            .copied()
            .collect()
    }
}

/// One raw entry header inside a pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    /// A whole object.
    None,
    /// Delta against the entry at this absolute offset.
    Offset(u64),
    /// Delta against this object id.
    Ref(Oid),
}

/// A decoded entry header.
#[derive(Clone, Copy, Debug)]
pub struct EntryHeader {
    /// Pack type code (1-4, 6 or 7).
    pub code: u8,
    /// Inflated size of the object or delta.
    pub size: u64,
    /// The delta base, if any.
    pub base: Base,
    /// Offset of the zlib stream.
    pub data_at: usize,
}

/// An open pack: the whole `.pack` file in memory plus its index.
pub struct Pack {
    /// Path of the `.pack` file.
    pub path: PathBuf,
    data: Vec<u8>,
    /// The parsed index.
    pub index: PackIndex,
    cache: Mutex<HashMap<u64, (Kind, Arc<Vec<u8>>)>>,
}

impl fmt::Debug for Pack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pack")
            .field("path", &self.path)
            .field("objects", &self.index.oids.len())
            .finish()
    }
}

/// How many resolved delta bases each pack keeps around.
const CACHE_ENTRIES: usize = 256;

impl Pack {
    /// Open `pack-*.idx` and the matching `.pack`.
    pub fn open(idx_path: &Path) -> Result<Pack> {
        let index = PackIndex::parse(&fs::read(idx_path)?)?;
        let path = idx_path.with_extension("pack");
        let data = fs::read(&path)?;
        Pack::from_parts(path, data, index)
    }

    /// Build a pack from bytes already in memory.
    pub fn from_parts(path: PathBuf, data: Vec<u8>, index: PackIndex) -> Result<Pack> {
        if data.len() < 32 || &data[..4] != b"PACK" {
            return Err(Error::corrupt(format!("{} is not a packfile", path.display())));
        }
        let version = be32(&data, 4)?;
        if version != 2 && version != 3 {
            return Err(Error::corrupt(format!("unsupported pack version {version}")));
        }
        let count = be32(&data, 8)? as usize;
        if count != index.oids.len() {
            return Err(Error::corrupt(format!(
                "pack has {count} objects but its index lists {}",
                index.oids.len()
            )));
        }
        if data[data.len() - 20..] != index.pack_checksum {
            return Err(Error::corrupt("pack checksum does not match its index"));
        }
        let end = (data.len() - 20) as u64;
        if index.offsets.iter().any(|&o| o < 12 || o >= end) {
            return Err(Error::corrupt("pack index offset points outside the pack"));
        }
        Ok(Pack { path, data, index, cache: Mutex::new(HashMap::new()) })
    }

    /// Raw pack bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Whether the pack holds `oid`.
    pub fn contains(&self, oid: &Oid) -> bool {
        self.index.position(oid).is_some()
    }

    /// Decode the entry header at `offset`.
    pub fn header_at(&self, offset: u64) -> Result<EntryHeader> {
        let mut pos = usize::try_from(offset).map_err(|_| Error::corrupt("pack offset too large"))?;
        let limit = self.data.len() - 20;
        let next = |pos: &mut usize| -> Result<u8> {
            if *pos >= limit {
                return Err(Error::corrupt("truncated pack entry"));
            }
            let b = self.data[*pos];
            *pos += 1;
            Ok(b)
        };
        let mut c = next(&mut pos)?;
        let code = (c >> 4) & 7;
        let mut size = u64::from(c & 0x0f);
        let mut shift = 4;
        while c & 0x80 != 0 {
            c = next(&mut pos)?;
            if shift > 57 {
                return Err(Error::corrupt("pack entry size is too long"));
            }
            size |= u64::from(c & 0x7f) << shift;
            shift += 7;
        }
        let base = match code {
            entry::OFS_DELTA => {
                let mut c = next(&mut pos)?;
                let mut rel = u64::from(c & 0x7f);
                while c & 0x80 != 0 {
                    c = next(&mut pos)?;
                    rel = rel
                        .checked_add(1)
                        .and_then(|r| r.checked_mul(128))
                        .ok_or_else(|| Error::corrupt("delta base offset overflows"))?
                        | u64::from(c & 0x7f);
                }
                if rel == 0 || rel > offset {
                    return Err(Error::corrupt("delta base offset points outside the pack"));
                }
                Base::Offset(offset - rel)
            }
            entry::REF_DELTA => {
                let raw = self
                    .data
                    .get(pos..pos + 20)
                    .filter(|_| pos + 20 <= limit)
                    .ok_or_else(|| Error::corrupt("truncated pack entry"))?;
                pos += 20;
                Base::Ref(Oid::from_slice(raw).expect("20 bytes"))
            }
            1..=4 => Base::None,
            other => return Err(Error::corrupt(format!("unknown pack entry type {other}"))),
        };
        Ok(EntryHeader { code, size, base, data_at: pos })
    }

    /// Inflate an entry's zlib stream; also returns the entry's end offset.
    pub fn inflate_at(&self, h: &EntryHeader) -> Result<(Vec<u8>, usize)> {
        let limit = self.data.len() - 20;
        let (out, used) = zlib_decompress_prefix(&self.data[h.data_at..limit])?;
        if out.len() as u64 != h.size {
            return Err(Error::corrupt("pack entry has the wrong inflated size"));
        }
        Ok((out, h.data_at + used))
    }

    /// Read the object stored at `offset`, resolving any delta chain.
    /// `external` resolves `REF_DELTA` bases that live outside this pack.
    pub fn read_at(
        &self,
        offset: u64,
        external: &dyn Fn(&Oid) -> Result<(Kind, Vec<u8>)>,
    ) -> Result<(Kind, Vec<u8>)> {
        // Walk down to a whole object (or a cached base), then apply the
        // deltas back up.  Iterative, so long chains cannot overflow the stack.
        let mut deltas: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut at = offset;
        let (kind, mut data) = loop {
            if deltas.len() > MAX_CHAIN {
                return Err(Error::corrupt("delta chain is too long (or cyclic)"));
            }
            let hit = self.cache.lock().expect("cache").get(&at).cloned();
            if let Some((k, d)) = hit {
                break (k, d.as_ref().clone());
            }
            let h = self.header_at(at)?;
            let (body, _) = self.inflate_at(&h)?;
            match h.base {
                Base::None => {
                    let k = kind_of(h.code).expect("whole object type");
                    if !deltas.is_empty() {
                        self.remember(at, k, &body);
                    }
                    break (k, body);
                }
                Base::Offset(b) => {
                    deltas.push((at, body));
                    at = b;
                }
                Base::Ref(oid) => {
                    deltas.push((at, body));
                    match self.index.position(&oid) {
                        Some(i) => at = self.index.offsets[i],
                        None => break external(&oid)?,
                    }
                }
            }
        };
        while let Some((off, d)) = deltas.pop() {
            data = delta::apply(&data, &d)?;
            if !deltas.is_empty() {
                self.remember(off, kind, &data);
            }
        }
        Ok((kind, data))
    }

    fn remember(&self, offset: u64, kind: Kind, data: &[u8]) {
        let mut cache = self.cache.lock().expect("cache");
        if cache.len() >= CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(offset, (kind, Arc::new(data.to_vec())));
    }

    /// Read `oid` from this pack, if present.
    pub fn read(
        &self,
        oid: &Oid,
        external: &dyn Fn(&Oid) -> Result<(Kind, Vec<u8>)>,
    ) -> Option<Result<(Kind, Vec<u8>)>> {
        let i = self.index.position(oid)?;
        Some(self.read_at(self.index.offsets[i], external))
    }
}

/// Open every pack under `objects/pack/` that has both files, newest first.
pub fn open_all(objects_dir: &Path) -> Result<Vec<Pack>> {
    let dir = objects_dir.join("pack");
    let Ok(entries) = fs::read_dir(&dir) else { return Ok(Vec::new()) };
    let mut idxs = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("pack-") && name.ends_with(".idx") && p.with_extension("pack").is_file() {
            let mtime = entry.metadata().and_then(|m| m.modified()).ok();
            idxs.push((mtime, p));
        }
    }
    idxs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    idxs.into_iter().map(|(_, p)| Pack::open(&p)).collect()
}
