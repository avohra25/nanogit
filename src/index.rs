//! The git index (staging area), file format version 2.
//!
//! Layout: `DIRC`, version, entry count, then one fixed 62-byte record plus
//! NUL-padded path per entry (padded to a multiple of 8), optional extensions,
//! and a SHA-1 trailer over everything before it.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::error::{Error, Result};
use crate::object::{mode, Oid};
use crate::sha1::sha1;

/// One index record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// Creation (inode change) time: seconds and nanoseconds.
    pub ctime: (u32, u32),
    /// Modification time: seconds and nanoseconds.
    pub mtime: (u32, u32),
    /// Device number.
    pub dev: u32,
    /// Inode number.
    pub ino: u32,
    /// Git file mode (`0o100644`, `0o100755`, `0o120000`, ...).
    pub mode: u32,
    /// Owner user id.
    pub uid: u32,
    /// Owner group id.
    pub gid: u32,
    /// File size (truncated to 32 bits like git does).
    pub size: u32,
    /// Blob id.
    pub oid: Oid,
    /// Merge stage: 0 normal, 1 base, 2 ours, 3 theirs.
    pub stage: u8,
    /// Slash-separated path relative to the work tree.
    pub path: String,
}

/// Reduce raw `st_mode` bits to the three file modes git records.
pub fn normalize_mode(st_mode: u32) -> u32 {
    match st_mode & 0o170_000 {
        0o120_000 => mode::SYMLINK,
        0o040_000 => mode::GITLINK,
        _ if st_mode & 0o111 != 0 => mode::EXEC,
        _ => mode::FILE,
    }
}

impl IndexEntry {
    /// An entry with no cached stat data (used for entries built from trees).
    pub fn new(path: &str, mode: u32, oid: Oid) -> IndexEntry {
        IndexEntry {
            ctime: (0, 0),
            mtime: (0, 0),
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            stage: 0,
            path: path.to_string(),
        }
    }

    /// An entry that caches the stat data of `meta`.
    pub fn from_metadata(path: &str, meta: &fs::Metadata, oid: Oid) -> IndexEntry {
        IndexEntry {
            ctime: (meta.ctime() as u32, meta.ctime_nsec() as u32),
            mtime: (meta.mtime() as u32, meta.mtime_nsec() as u32),
            dev: meta.dev() as u32,
            ino: meta.ino() as u32,
            mode: normalize_mode(meta.mode()),
            uid: meta.uid(),
            gid: meta.gid(),
            size: meta.size() as u32,
            oid,
            stage: 0,
            path: path.to_string(),
        }
    }

    /// True if the cached stat data still describes `meta` exactly.
    pub fn stat_matches(&self, meta: &fs::Metadata) -> bool {
        self.size == meta.size() as u32
            && self.mtime == (meta.mtime() as u32, meta.mtime_nsec() as u32)
            && self.mode == normalize_mode(meta.mode())
            && (self.ino == 0 || self.ino == meta.ino() as u32)
    }
}

/// The whole staging area, kept sorted by `(path, stage)`.
#[derive(Clone, Debug, Default)]
pub struct Index {
    /// Entries in git order.
    pub entries: Vec<IndexEntry>,
    /// Modification time of the index file when it was read.
    pub file_mtime: Option<(u32, u32)>,
}

fn be32(data: &[u8], at: usize) -> Result<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| Error::corrupt("index is truncated"))
}

impl Index {
    /// An empty index.
    pub fn new() -> Index {
        Index::default()
    }

    /// Read the index file at `path`; a missing file is an empty index.
    pub fn read(path: &Path) -> Result<Index> {
        let data = match fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Index::new()),
            Err(e) => return Err(e.into()),
        };
        let mut index = Index::parse(&data)?;
        if let Ok(meta) = fs::metadata(path) {
            index.file_mtime = Some((meta.mtime() as u32, meta.mtime_nsec() as u32));
        }
        Ok(index)
    }

    /// Parse the bytes of an index file.
    pub fn parse(data: &[u8]) -> Result<Index> {
        if data.len() < 32 || &data[..4] != b"DIRC" {
            return Err(Error::corrupt("index has a bad signature"));
        }
        let version = be32(data, 4)?;
        if version != 2 {
            return Err(Error::fatal(format!("index version {version} is not supported (only version 2)")));
        }
        let body_end = data.len() - 20;
        if sha1(&data[..body_end])[..] != data[body_end..] {
            return Err(Error::corrupt("index checksum mismatch"));
        }
        let count = be32(data, 8)? as usize;
        let mut entries = Vec::with_capacity(count);
        let mut pos = 12;
        for _ in 0..count {
            if pos + 62 > body_end {
                return Err(Error::corrupt("index entry is truncated"));
            }
            let flags = u16::from_be_bytes([data[pos + 60], data[pos + 61]]);
            if flags & 0x4000 != 0 {
                return Err(Error::fatal("index entries with extended flags are not supported"));
            }
            let name_len = usize::from(flags & 0x0fff);
            let name_start = pos + 62;
            let name_end = if name_len < 0x0fff {
                name_start + name_len
            } else {
                name_start
                    + data[name_start..body_end]
                        .iter()
                        .position(|&b| b == 0)
                        .ok_or_else(|| Error::corrupt("unterminated index path"))?
            };
            let path = data
                .get(name_start..name_end)
                .and_then(|b| std::str::from_utf8(b).ok())
                .ok_or_else(|| Error::corrupt("index path is not valid UTF-8"))?
                .to_string();
            let oid = Oid::from_slice(&data[pos + 40..pos + 60])
                .ok_or_else(|| Error::corrupt("bad object id in index"))?;
            entries.push(IndexEntry {
                ctime: (be32(data, pos)?, be32(data, pos + 4)?),
                mtime: (be32(data, pos + 8)?, be32(data, pos + 12)?),
                dev: be32(data, pos + 16)?,
                ino: be32(data, pos + 20)?,
                mode: be32(data, pos + 24)?,
                uid: be32(data, pos + 28)?,
                gid: be32(data, pos + 32)?,
                size: be32(data, pos + 36)?,
                oid,
                stage: ((flags >> 12) & 3) as u8,
                path,
            });
            pos += (62 + (name_end - name_start) + 8) & !7;
        }
        // Optional extensions (TREE, REUC, ...): skipped, but must be well formed.
        while pos + 8 <= body_end {
            let sig = &data[pos..pos + 4];
            let size = be32(data, pos + 4)? as usize;
            if !sig[0].is_ascii_uppercase() {
                return Err(Error::fatal("index uses a required extension nanogit does not know"));
            }
            pos += 8 + size;
        }
        if pos != body_end {
            return Err(Error::corrupt("index has trailing garbage"));
        }
        if entries.windows(2).any(|w| (w[0].path.as_bytes(), w[0].stage) >= (w[1].path.as_bytes(), w[1].stage)) {
            return Err(Error::corrupt("index entries are not sorted"));
        }
        Ok(Index { entries, file_mtime: None })
    }

    /// Serialize to the on-disk format (without the cached-tree extension).
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        for e in &self.entries {
            let start = out.len();
            for v in [
                e.ctime.0, e.ctime.1, e.mtime.0, e.mtime.1, e.dev, e.ino, e.mode, e.uid, e.gid, e.size,
            ] {
                out.extend_from_slice(&v.to_be_bytes());
            }
            out.extend_from_slice(e.oid.as_bytes());
            let name_len = e.path.len().min(0x0fff) as u16;
            let flags = (u16::from(e.stage) << 12) | name_len;
            out.extend_from_slice(&flags.to_be_bytes());
            out.extend_from_slice(e.path.as_bytes());
            let padded = (62 + e.path.len() + 8) & !7;
            out.resize(start + padded, 0);
        }
        let digest = sha1(&out);
        out.extend_from_slice(&digest);
        out
    }

    /// Write atomically through `<path>.lock`.
    pub fn write(&self, path: &Path) -> Result<()> {
        let lock = path.with_extension("lock");
        fs::write(&lock, self.serialize())?;
        fs::rename(&lock, path)?;
        Ok(())
    }

    fn position(&self, path: &str, stage: u8) -> std::result::Result<usize, usize> {
        self.entries
            .binary_search_by(|e| (e.path.as_bytes(), e.stage).cmp(&(path.as_bytes(), stage)))
    }

    /// The stage-0 entry for `path`.
    pub fn get(&self, path: &str) -> Option<&IndexEntry> {
        self.position(path, 0).ok().map(|i| &self.entries[i])
    }

    /// Entries (any stage) for exactly `path`.
    pub fn entries_for<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a IndexEntry> + 'a {
        self.entries.iter().filter(move |e| e.path == path)
    }

    /// Insert or replace an entry.  A stage-0 entry also clears the other
    /// stages of the path and any file/directory conflicts, like `git add`.
    pub fn add(&mut self, entry: IndexEntry) {
        if entry.stage == 0 {
            let mut p = entry.path.as_str();
            while let Some(i) = p.rfind('/') {
                p = &p[..i];
                self.remove(p);
            }
            let prefix = format!("{}/", entry.path);
            self.entries.retain(|e| !e.path.starts_with(&prefix));
            self.entries.retain(|e| !(e.path == entry.path && e.stage != 0));
        }
        match self.position(&entry.path, entry.stage) {
            Ok(i) => self.entries[i] = entry,
            Err(i) => self.entries.insert(i, entry),
        }
    }

    /// Remove every stage of `path`; true if anything was removed.
    pub fn remove(&mut self, path: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.path != path);
        self.entries.len() != before
    }

    /// True if any entry is in a conflict stage.
    pub fn has_conflicts(&self) -> bool {
        self.entries.iter().any(|e| e.stage != 0)
    }

    /// Sorted, de-duplicated paths that have conflict entries.
    pub fn conflicted_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> =
            self.entries.iter().filter(|e| e.stage != 0).map(|e| e.path.clone()).collect();
        paths.dedup();
        paths
    }

    /// True if `path` is tracked (any stage).
    pub fn contains(&self, path: &str) -> bool {
        self.entries.iter().any(|e| e.path == path)
    }

    /// True if some tracked path lives under directory `dir`.
    pub fn contains_dir(&self, dir: &str) -> bool {
        let prefix = format!("{dir}/");
        let start = self
            .entries
            .partition_point(|e| e.path.as_bytes() < prefix.as_bytes());
        self.entries.get(start).is_some_and(|e| e.path.starts_with(&prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{hash_object, Kind};

    fn entry(path: &str, content: &str) -> IndexEntry {
        IndexEntry::new(path, mode::FILE, hash_object(Kind::Blob, content.as_bytes()))
    }

    #[test]
    fn stays_sorted_and_replaces() {
        let mut idx = Index::new();
        idx.add(entry("b", "1"));
        idx.add(entry("a", "1"));
        idx.add(entry("c/d", "1"));
        idx.add(entry("b", "2"));
        let paths: Vec<&str> = idx.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a", "b", "c/d"]);
        assert_eq!(idx.get("b").unwrap().oid, hash_object(Kind::Blob, b"2"));
        assert!(idx.contains_dir("c"));
        assert!(!idx.contains_dir("a"));
        assert!(idx.remove("a"));
        assert!(!idx.remove("a"));
    }

    #[test]
    fn file_directory_conflicts_are_replaced() {
        let mut idx = Index::new();
        idx.add(entry("a", "file"));
        idx.add(entry("a/b", "now a dir"));
        assert!(idx.get("a").is_none());
        assert!(idx.get("a/b").is_some());
        idx.add(entry("a", "file again"));
        assert!(idx.get("a/b").is_none());
        assert_eq!(idx.entries.len(), 1);
    }

    #[test]
    fn serialization_round_trip_with_odd_name_lengths() {
        let mut idx = Index::new();
        for len in 1..20 {
            idx.add(entry(&"x".repeat(len), "data"));
        }
        let mut e = entry("exec.sh", "#!/bin/sh");
        e.mode = mode::EXEC;
        e.mtime = (1_700_000_000, 123);
        e.size = 9;
        idx.add(e);
        let bytes = idx.serialize();
        assert_eq!(&bytes[..4], b"DIRC");
        assert_eq!((bytes.len() - 12 - 20) % 8, 0);
        let back = Index::parse(&bytes).unwrap();
        assert_eq!(back.entries, idx.entries);
    }

    #[test]
    fn stages_round_trip() {
        let mut idx = Index::new();
        for stage in 1..=3u8 {
            let mut e = entry("conflict.txt", &stage.to_string());
            e.stage = stage;
            idx.add(e);
        }
        idx.add(entry("ok.txt", "fine"));
        assert!(idx.has_conflicts());
        assert_eq!(idx.conflicted_paths(), ["conflict.txt"]);
        let back = Index::parse(&idx.serialize()).unwrap();
        assert_eq!(back.entries, idx.entries);
        assert_eq!(back.entries_for("conflict.txt").count(), 3);
        // A stage-0 add resolves the conflict.
        let mut resolved = back;
        resolved.add(entry("conflict.txt", "merged"));
        assert!(!resolved.has_conflicts());
    }

    #[test]
    fn rejects_damage() {
        let mut idx = Index::new();
        idx.add(entry("a", "1"));
        let good = idx.serialize();
        let mut bad = good.clone();
        bad[20] ^= 1;
        assert!(Index::parse(&bad).is_err(), "checksum must catch edits");
        assert!(Index::parse(&good[..good.len() - 3]).is_err());
        assert!(Index::parse(b"nope").is_err());
        let mut v3 = good.clone();
        v3[7] = 3;
        assert!(Index::parse(&v3).is_err());
    }

    #[test]
    fn skips_known_extensions() {
        let mut idx = Index::new();
        idx.add(entry("a", "1"));
        let mut body = idx.serialize();
        body.truncate(body.len() - 20);
        body.extend_from_slice(b"TREE\0\0\0\x03abc");
        let digest = sha1(&body);
        body.extend_from_slice(&digest);
        let back = Index::parse(&body).unwrap();
        assert_eq!(back.entries.len(), 1);
    }

    #[test]
    fn normalizes_modes() {
        assert_eq!(normalize_mode(0o100_664), mode::FILE);
        assert_eq!(normalize_mode(0o100_775), mode::EXEC);
        assert_eq!(normalize_mode(0o120_777), mode::SYMLINK);
    }
}
