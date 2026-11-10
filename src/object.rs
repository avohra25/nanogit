//! Git objects (blob, tree, commit, tag) and the loose object store.
//!
//! Every object is stored as `zlib("<kind> <size>\0<body>")` in
//! `objects/xx/yyyy...`, named by the SHA-1 of the uncompressed bytes.  That is
//! exactly git's loose format, so the two tools can share a repository.
//! Objects that git has moved into packfiles (`objects/pack/`, e.g. after
//! `git gc`) are found there when no loose copy exists.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::error::{Error, Result};
use crate::pack::{self, Pack};
use crate::sha1::{from_hex, sha1, to_hex};
use crate::zlib::{zlib_compress, zlib_decompress};

/// A 20-byte object id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid([u8; 20]);

impl Oid {
    /// The all-zero id git uses for "no object".
    pub const ZERO: Oid = Oid([0; 20]);

    /// Wrap raw digest bytes.
    pub fn from_bytes(bytes: [u8; 20]) -> Oid {
        Oid(bytes)
    }

    /// Wrap a slice that must be exactly 20 bytes long.
    pub fn from_slice(bytes: &[u8]) -> Option<Oid> {
        <[u8; 20]>::try_from(bytes).ok().map(Oid)
    }

    /// Parse 40 hexadecimal characters.
    pub fn from_hex(s: &str) -> Option<Oid> {
        if s.len() != 40 {
            return None;
        }
        Oid::from_slice(&from_hex(s)?)
    }

    /// The raw 20 bytes.
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// 40-character lower-case hex form.
    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }

    /// The first `n` hex characters (clamped to 40).
    pub fn short(&self, n: usize) -> String {
        let mut h = self.to_hex();
        h.truncate(n.min(40));
        h
    }

    /// True for [`Oid::ZERO`].
    pub fn is_zero(&self) -> bool {
        self.0 == [0; 20]
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.to_hex())
    }
}

/// The four git object types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// File contents.
    Blob,
    /// A directory listing.
    Tree,
    /// A snapshot with parents and a message.
    Commit,
    /// An annotated tag.
    Tag,
}

impl Kind {
    /// The name git uses in object headers.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Blob => "blob",
            Kind::Tree => "tree",
            Kind::Commit => "commit",
            Kind::Tag => "tag",
        }
    }

    /// Parse a type name.
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "blob" => Some(Kind::Blob),
            "tree" => Some(Kind::Tree),
            "commit" => Some(Kind::Commit),
            "tag" => Some(Kind::Tag),
            _ => None,
        }
    }
}

/// File mode constants as stored in trees and the index.
pub mod mode {
    /// Regular file.
    pub const FILE: u32 = 0o100_644;
    /// Executable file.
    pub const EXEC: u32 = 0o100_755;
    /// Symbolic link.
    pub const SYMLINK: u32 = 0o120_000;
    /// Directory (tree).
    pub const TREE: u32 = 0o040_000;
    /// Submodule commit.
    pub const GITLINK: u32 = 0o160_000;

    /// True for tree entries that are directories.
    pub fn is_tree(m: u32) -> bool {
        m & 0o170_000 == TREE
    }
}

/// One line of a tree object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// Git file mode, e.g. `0o100644`.
    pub mode: u32,
    /// File or directory name (one path component).
    pub name: String,
    /// Id of the blob or subtree.
    pub oid: Oid,
}

impl TreeEntry {
    /// The object type this entry points at.
    pub fn kind(&self) -> Kind {
        match self.mode & 0o170_000 {
            mode::TREE => Kind::Tree,
            mode::GITLINK => Kind::Commit,
            _ => Kind::Blob,
        }
    }

    fn sort_key(&self) -> Vec<u8> {
        let mut key = self.name.clone().into_bytes();
        if mode::is_tree(self.mode) {
            key.push(b'/');
        }
        key
    }
}

/// Author, committer or tagger line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// Display name.
    pub name: String,
    /// E-mail address (without angle brackets).
    pub email: String,
    /// Seconds since the Unix epoch.
    pub time: i64,
    /// Time zone offset such as `+0000` or `-0800`.
    pub tz: String,
}

impl Signature {
    /// Parse `Name <email> 1700000000 +0000`.
    pub fn parse(s: &str) -> Result<Signature> {
        let lt = s.find('<').ok_or_else(|| Error::corrupt("signature without '<'"))?;
        let gt = s.rfind('>').ok_or_else(|| Error::corrupt("signature without '>'"))?;
        if gt < lt {
            return Err(Error::corrupt("malformed signature"));
        }
        let mut rest = s[gt + 1..].split_whitespace();
        let time = rest
            .next()
            .and_then(|t| t.parse::<i64>().ok())
            .ok_or_else(|| Error::corrupt("signature without timestamp"))?;
        let tz = rest.next().unwrap_or("+0000").to_string();
        Ok(Signature {
            name: s[..lt].trim_end().to_string(),
            email: s[lt + 1..gt].to_string(),
            time,
            tz,
        })
    }

    /// Seconds east of UTC encoded by [`Signature::tz`].
    pub fn tz_offset_seconds(&self) -> i64 {
        let digits = self.tz.trim_start_matches(['+', '-']);
        if digits.len() != 4 {
            return 0;
        }
        let h: i64 = digits[..2].parse().unwrap_or(0);
        let m: i64 = digits[2..].parse().unwrap_or(0);
        let secs = h * 3600 + m * 60;
        if self.tz.starts_with('-') {
            -secs
        } else {
            secs
        }
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} <{}> {} {}", self.name, self.email, self.time, self.tz)
    }
}

/// A parsed commit object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    /// Root tree of the snapshot.
    pub tree: Oid,
    /// Parent commits (none for a root commit, two or more for merges).
    pub parents: Vec<Oid>,
    /// Who wrote the change.
    pub author: Signature,
    /// Who recorded it.
    pub committer: Signature,
    /// Unknown headers (`gpgsig`, `encoding`, ...), preserved verbatim.
    pub extra: Vec<(String, String)>,
    /// Commit message, including its trailing newline.
    pub message: String,
}

impl Commit {
    /// First line of the message.
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

/// A parsed annotated tag object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    /// Tagged object.
    pub object: Oid,
    /// Type of the tagged object.
    pub kind: Kind,
    /// Tag name.
    pub name: String,
    /// Optional tagger line.
    pub tagger: Option<Signature>,
    /// Tag message.
    pub message: String,
}

/// Any parsed git object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Object {
    /// Raw file contents.
    Blob(Vec<u8>),
    /// Directory entries.
    Tree(Vec<TreeEntry>),
    /// A commit.
    Commit(Commit),
    /// An annotated tag.
    Tag(Tag),
}

fn utf8(body: &[u8], what: &str) -> Result<String> {
    String::from_utf8(body.to_vec()).map_err(|_| Error::corrupt(format!("{what} is not valid UTF-8")))
}

fn parse_tree(body: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        let sp = rest
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| Error::corrupt("tree entry without mode"))?;
        let mode = std::str::from_utf8(&rest[..sp])
            .ok()
            .and_then(|m| u32::from_str_radix(m, 8).ok())
            .ok_or_else(|| Error::corrupt("tree entry with bad mode"))?;
        rest = &rest[sp + 1..];
        let nul = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::corrupt("tree entry without name terminator"))?;
        let name = utf8(&rest[..nul], "tree entry name")?;
        rest = &rest[nul + 1..];
        let oid = rest
            .get(..20)
            .and_then(Oid::from_slice)
            .ok_or_else(|| Error::corrupt("truncated tree entry"))?;
        rest = &rest[20..];
        entries.push(TreeEntry { mode, name, oid });
    }
    Ok(entries)
}

fn serialize_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut sorted: Vec<&TreeEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| e.sort_key());
    let mut out = Vec::new();
    for e in sorted {
        out.extend_from_slice(format!("{:o} ", e.mode).as_bytes());
        out.extend_from_slice(e.name.as_bytes());
        out.push(0);
        out.extend_from_slice(e.oid.as_bytes());
    }
    out
}

/// Split an object body into header lines and the message after the blank line.
fn split_headers(text: &str) -> (Vec<(String, String)>, String) {
    let (head, message) = match text.find("\n\n") {
        Some(p) => (&text[..p], &text[p + 2..]),
        None => (text.trim_end_matches('\n'), ""),
    };
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in head.split('\n') {
        if let Some(cont) = line.strip_prefix(' ') {
            if let Some(last) = headers.last_mut() {
                last.1.push('\n');
                last.1.push_str(cont);
            }
        } else if let Some((k, v)) = line.split_once(' ') {
            headers.push((k.to_string(), v.to_string()));
        } else if !line.is_empty() {
            headers.push((line.to_string(), String::new()));
        }
    }
    (headers, message.to_string())
}

fn parse_oid(hex: &str, what: &str) -> Result<Oid> {
    Oid::from_hex(hex).ok_or_else(|| Error::corrupt(format!("bad {what} id {hex:?}")))
}

fn parse_commit(body: &[u8]) -> Result<Commit> {
    let text = utf8(body, "commit")?;
    let (headers, message) = split_headers(&text);
    let (mut tree, mut author, mut committer) = (None, None, None);
    let mut parents = Vec::new();
    let mut extra = Vec::new();
    for (key, value) in headers {
        match key.as_str() {
            "tree" => tree = Some(parse_oid(&value, "tree")?),
            "parent" => parents.push(parse_oid(&value, "parent")?),
            "author" => author = Some(Signature::parse(&value)?),
            "committer" => committer = Some(Signature::parse(&value)?),
            _ => extra.push((key, value)),
        }
    }
    Ok(Commit {
        tree: tree.ok_or_else(|| Error::corrupt("commit without tree"))?,
        parents,
        author: author.ok_or_else(|| Error::corrupt("commit without author"))?,
        committer: committer.ok_or_else(|| Error::corrupt("commit without committer"))?,
        extra,
        message,
    })
}

fn serialize_commit(c: &Commit) -> Vec<u8> {
    let mut s = format!("tree {}\n", c.tree);
    for p in &c.parents {
        s.push_str(&format!("parent {p}\n"));
    }
    s.push_str(&format!("author {}\ncommitter {}\n", c.author, c.committer));
    for (k, v) in &c.extra {
        s.push_str(&format!("{k} {}\n", v.replace('\n', "\n ")));
    }
    s.push('\n');
    s.push_str(&c.message);
    s.into_bytes()
}

fn parse_tag(body: &[u8]) -> Result<Tag> {
    let text = utf8(body, "tag")?;
    let (headers, message) = split_headers(&text);
    let (mut object, mut kind, mut name, mut tagger) = (None, None, None, None);
    for (key, value) in headers {
        match key.as_str() {
            "object" => object = Some(parse_oid(&value, "tag target")?),
            "type" => kind = Kind::parse(&value),
            "tag" => name = Some(value),
            "tagger" => tagger = Some(Signature::parse(&value)?),
            _ => {}
        }
    }
    Ok(Tag {
        object: object.ok_or_else(|| Error::corrupt("tag without object"))?,
        kind: kind.ok_or_else(|| Error::corrupt("tag without type"))?,
        name: name.ok_or_else(|| Error::corrupt("tag without name"))?,
        tagger,
        message,
    })
}

fn serialize_tag(t: &Tag) -> Vec<u8> {
    let mut s = format!("object {}\ntype {}\ntag {}\n", t.object, t.kind.as_str(), t.name);
    if let Some(tagger) = &t.tagger {
        s.push_str(&format!("tagger {tagger}\n"));
    }
    s.push('\n');
    s.push_str(&t.message);
    s.into_bytes()
}

impl Object {
    /// The object's type.
    pub fn kind(&self) -> Kind {
        match self {
            Object::Blob(_) => Kind::Blob,
            Object::Tree(_) => Kind::Tree,
            Object::Commit(_) => Kind::Commit,
            Object::Tag(_) => Kind::Tag,
        }
    }

    /// Parse a body of the given kind.
    pub fn parse(kind: Kind, body: &[u8]) -> Result<Object> {
        Ok(match kind {
            Kind::Blob => Object::Blob(body.to_vec()),
            Kind::Tree => Object::Tree(parse_tree(body)?),
            Kind::Commit => Object::Commit(parse_commit(body)?),
            Kind::Tag => Object::Tag(parse_tag(body)?),
        })
    }

    /// The canonical body bytes (trees are sorted in git order).
    pub fn serialize(&self) -> Vec<u8> {
        match self {
            Object::Blob(b) => b.clone(),
            Object::Tree(entries) => serialize_tree(entries),
            Object::Commit(c) => serialize_commit(c),
            Object::Tag(t) => serialize_tag(t),
        }
    }

    /// The id this object would be stored under.
    pub fn id(&self) -> Oid {
        hash_object(self.kind(), &self.serialize())
    }
}

/// Hash `body` the way `git hash-object -t <kind>` does.
pub fn hash_object(kind: Kind, body: &[u8]) -> Oid {
    Oid(sha1(&frame(kind, body)))
}

fn frame(kind: Kind, body: &[u8]) -> Vec<u8> {
    let mut data = format!("{} {}\0", kind.as_str(), body.len()).into_bytes();
    data.extend_from_slice(body);
    data
}

/// The `objects/` directory of a repository: loose objects plus packs.
#[derive(Clone, Debug)]
pub struct ObjectStore {
    dir: PathBuf,
    packs: Arc<OnceLock<std::result::Result<Vec<Pack>, String>>>,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl ObjectStore {
    /// Open the store rooted at `objects_dir`.
    pub fn new(objects_dir: impl Into<PathBuf>) -> ObjectStore {
        ObjectStore { dir: objects_dir.into(), packs: Arc::new(OnceLock::new()) }
    }

    /// The packs under `objects/pack/`, opened on first use.
    pub fn packs(&self) -> Result<&[Pack]> {
        match self.packs.get_or_init(|| pack::open_all(&self.dir).map_err(|e| e.to_string())) {
            Ok(p) => Ok(p),
            Err(msg) => Err(Error::corrupt(msg.clone())),
        }
    }

    /// Forget the opened packs so the next lookup rescans `objects/pack/`.
    pub fn reload_packs(&mut self) {
        self.packs = Arc::new(OnceLock::new());
    }

    /// Whether the object exists as a loose file.
    pub fn contains_loose(&self, oid: &Oid) -> bool {
        self.path_of(oid).is_file()
    }

    /// Every loose object id, sorted.
    pub fn loose_oids(&self) -> Result<Vec<Oid>> {
        let mut out = Vec::new();
        let Ok(top) = fs::read_dir(&self.dir) else { return Ok(out) };
        for d in top.flatten() {
            let dname = d.file_name().to_string_lossy().into_owned();
            if dname.len() != 2 || !d.path().is_dir() {
                continue;
            }
            for f in fs::read_dir(d.path())?.flatten() {
                let fname = f.file_name().to_string_lossy().into_owned();
                if fname.len() == 38 {
                    if let Some(oid) = Oid::from_hex(&format!("{dname}{fname}")) {
                        out.push(oid);
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Read an object from the packs only (used for delta bases too).
    fn read_packed(&self, oid: &Oid, depth: usize) -> Result<Option<(Kind, Vec<u8>)>> {
        if depth > 64 {
            return Err(Error::corrupt(format!("delta bases of {oid} cross packs too deeply")));
        }
        let external = |base: &Oid| -> Result<(Kind, Vec<u8>)> {
            if self.contains_loose(base) {
                return self.read_loose(base);
            }
            self.read_packed(base, depth + 1)?
                .ok_or_else(|| Error::corrupt(format!("delta base {base} of {oid} is missing")))
        };
        for p in self.packs()? {
            if let Some(found) = p.read(oid, &external) {
                let (kind, body) = found?;
                if hash_object(kind, &body) != *oid {
                    return Err(Error::corrupt(format!(
                        "object {oid} in {} has the wrong hash",
                        p.path.display()
                    )));
                }
                return Ok(Some((kind, body)));
            }
        }
        Ok(None)
    }

    fn path_of(&self, oid: &Oid) -> PathBuf {
        let hex = oid.to_hex();
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    /// Whether the object exists, loose or packed.
    pub fn contains(&self, oid: &Oid) -> bool {
        self.contains_loose(oid)
            || self.packs().map(|ps| ps.iter().any(|p| p.contains(oid))).unwrap_or(false)
    }

    /// Store an object body and return its id (a no-op if already present).
    pub fn write(&self, kind: Kind, body: &[u8]) -> Result<Oid> {
        let data = frame(kind, body);
        let oid = Oid(sha1(&data));
        let path = self.path_of(&oid);
        if path.is_file() || self.packs().map(|ps| ps.iter().any(|p| p.contains(&oid))).unwrap_or(false) {
            return Ok(oid);
        }
        let parent = path.parent().unwrap_or(&self.dir);
        fs::create_dir_all(parent)?;
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!("tmp_obj_{}_{n}", std::process::id()));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&zlib_compress(&data))?;
        }
        set_read_only(&tmp);
        fs::rename(&tmp, &path)?;
        Ok(oid)
    }

    /// Store a parsed object.
    pub fn write_object(&self, obj: &Object) -> Result<Oid> {
        self.write(obj.kind(), &obj.serialize())
    }

    /// Read an object's type and body (loose first, then packs).
    pub fn read_raw(&self, oid: &Oid) -> Result<(Kind, Vec<u8>)> {
        if self.contains_loose(oid) {
            return self.read_loose(oid);
        }
        match self.read_packed(oid, 0)? {
            Some(found) => Ok(found),
            None => Err(Error::fatal(format!("object {oid} not found"))),
        }
    }

    fn read_loose(&self, oid: &Oid) -> Result<(Kind, Vec<u8>)> {
        let path = self.path_of(oid);
        let packed = fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::fatal(format!("object {oid} not found"))
            } else {
                Error::Io(e)
            }
        })?;
        let data = zlib_decompress(&packed)?;
        let nul = data
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::corrupt(format!("object {oid} has no header")))?;
        let header = std::str::from_utf8(&data[..nul])
            .map_err(|_| Error::corrupt(format!("object {oid} has a bad header")))?;
        let (kind, size) = header
            .split_once(' ')
            .and_then(|(k, s)| Some((Kind::parse(k)?, s.parse::<usize>().ok()?)))
            .ok_or_else(|| Error::corrupt(format!("object {oid} has a bad header")))?;
        let body = data[nul + 1..].to_vec();
        if body.len() != size {
            return Err(Error::corrupt(format!("object {oid} has the wrong size")));
        }
        Ok((kind, body))
    }

    /// Read and parse an object.
    pub fn read(&self, oid: &Oid) -> Result<Object> {
        let (kind, body) = self.read_raw(oid)?;
        Object::parse(kind, &body)
    }

    /// Expand an abbreviated hex id (at least 4 characters) to a full id.
    pub fn resolve_prefix(&self, prefix: &str) -> Result<Oid> {
        let valid = prefix.len() >= 4
            && prefix.len() <= 40
            && prefix.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !valid {
            return Err(Error::fatal(format!("bad object name '{prefix}'")));
        }
        if prefix.len() == 40 {
            return Oid::from_hex(prefix).ok_or_else(|| Error::fatal("bad object name"));
        }
        let dir = self.dir.join(&prefix[..2]);
        let mut found = Vec::new();
        if let Ok(read_dir) = fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.len() == 38 && name.starts_with(&prefix[2..]) {
                    if let Some(oid) = Oid::from_hex(&format!("{}{}", &prefix[..2], name)) {
                        found.push(oid);
                    }
                }
            }
        }
        for p in self.packs()? {
            found.extend(p.index.with_prefix(prefix));
        }
        found.sort();
        found.dedup();
        match found.len() {
            0 => Err(Error::fatal(format!("no object matches '{prefix}'"))),
            1 => Ok(found[0]),
            _ => Err(Error::fatal(format!("short object id '{prefix}' is ambiguous"))),
        }
    }

    /// The objects directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(unix)]
fn set_read_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o444));
}

#[cfg(not(unix))]
fn set_read_only(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(name: &str, time: i64) -> Signature {
        Signature {
            name: name.into(),
            email: format!("{name}@example.com"),
            time,
            tz: "-0800".into(),
        }
    }

    fn temp_store(tag: &str) -> ObjectStore {
        let dir = std::env::temp_dir().join(format!("nanogit-obj-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        ObjectStore::new(dir)
    }

    #[test]
    fn well_known_ids() {
        assert_eq!(
            hash_object(Kind::Blob, b"").to_hex(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            hash_object(Kind::Tree, b"").to_hex(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
        assert_eq!(
            hash_object(Kind::Blob, b"hello\n").to_hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn oid_helpers() {
        let oid = Oid::from_hex("ce013625030ba8dba906f756967f9e9ca394464a").unwrap();
        assert_eq!(oid.short(7), "ce01362");
        assert_eq!(oid.to_string().len(), 40);
        assert!(Oid::from_hex("nope").is_none());
        assert!(Oid::ZERO.is_zero());
        assert!(!oid.is_zero());
        assert!(Oid::from_slice(&[0; 3]).is_none());
    }

    #[test]
    fn tree_round_trip_and_git_ordering() {
        let a = hash_object(Kind::Blob, b"a");
        let entries = vec![
            TreeEntry { mode: mode::TREE, name: "foo".into(), oid: a },
            TreeEntry { mode: mode::FILE, name: "foo.c".into(), oid: a },
            TreeEntry { mode: mode::FILE, name: "foo-bar".into(), oid: a },
            TreeEntry { mode: mode::EXEC, name: "zed".into(), oid: a },
        ];
        let body = Object::Tree(entries).serialize();
        let Object::Tree(parsed) = Object::parse(Kind::Tree, &body).unwrap() else {
            panic!("not a tree");
        };
        let names: Vec<&str> = parsed.iter().map(|e| e.name.as_str()).collect();
        // git sorts directories as if their names ended with '/'.
        assert_eq!(names, ["foo-bar", "foo.c", "foo", "zed"]);
        assert_eq!(parsed[2].kind(), Kind::Tree);
        assert_eq!(parsed[3].mode, mode::EXEC);
        assert!(body.starts_with(b"100644 foo-bar\0"));
        assert!(body.windows(10).any(|w| w == b"40000 foo\0"));
    }

    #[test]
    fn commit_round_trip() {
        let tree = hash_object(Kind::Tree, b"");
        let commit = Commit {
            tree,
            parents: vec![hash_object(Kind::Blob, b"p1"), hash_object(Kind::Blob, b"p2")],
            author: sig("alice", 1_700_000_000),
            committer: sig("bob", 1_700_000_100),
            extra: vec![("gpgsig".into(), "-----BEGIN-----\nline\n-----END-----".into())],
            message: "subject\n\nbody line\n".into(),
        };
        let obj = Object::Commit(commit.clone());
        let body = obj.serialize();
        let text = String::from_utf8(body.clone()).unwrap();
        assert!(text.starts_with("tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nparent "));
        assert!(text.contains("\ngpgsig -----BEGIN-----\n line\n -----END-----\n\nsubject"));
        assert_eq!(Object::parse(Kind::Commit, &body).unwrap(), obj);
        assert_eq!(commit.subject(), "subject");
    }

    #[test]
    fn tag_round_trip() {
        let tag = Tag {
            object: hash_object(Kind::Blob, b"x"),
            kind: Kind::Blob,
            name: "v1.0".into(),
            tagger: Some(sig("carol", 5)),
            message: "release\n".into(),
        };
        let obj = Object::Tag(tag);
        assert_eq!(Object::parse(Kind::Tag, &obj.serialize()).unwrap(), obj);
    }

    #[test]
    fn signature_parsing() {
        let s = Signature::parse("A B <a@b.c> 1700000000 -0830").unwrap();
        assert_eq!((s.name.as_str(), s.email.as_str(), s.time), ("A B", "a@b.c", 1_700_000_000));
        assert_eq!(s.tz_offset_seconds(), -(8 * 3600 + 30 * 60));
        assert_eq!(s.to_string(), "A B <a@b.c> 1700000000 -0830");
        assert!(Signature::parse("nobody").is_err());
        assert!(Signature::parse("x <y> notanumber +0000").is_err());
    }

    #[test]
    fn malformed_objects_are_rejected() {
        assert!(Object::parse(Kind::Tree, b"100644 name\0short").is_err());
        assert!(Object::parse(Kind::Tree, b"zzz name\0").is_err());
        assert!(Object::parse(Kind::Commit, b"author x <y> 1 +0000\n\nmsg").is_err());
        assert!(Object::parse(Kind::Tag, b"object nothex\n\n").is_err());
    }

    #[test]
    fn store_round_trip_and_idempotence() {
        let store = temp_store("rt");
        let oid = store.write(Kind::Blob, b"some contents\n").unwrap();
        assert!(store.contains(&oid));
        assert_eq!(store.write(Kind::Blob, b"some contents\n").unwrap(), oid);
        let (kind, body) = store.read_raw(&oid).unwrap();
        assert_eq!((kind, body.as_slice()), (Kind::Blob, &b"some contents\n"[..]));
        assert_eq!(store.read(&oid).unwrap(), Object::Blob(b"some contents\n".to_vec()));
        let missing = hash_object(Kind::Blob, b"missing");
        assert!(store.read_raw(&missing).is_err());
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn prefix_resolution() {
        let store = temp_store("prefix");
        let oid = store.write(Kind::Blob, b"abc").unwrap();
        let hex = oid.to_hex();
        assert_eq!(store.resolve_prefix(&hex[..8]).unwrap(), oid);
        assert_eq!(store.resolve_prefix(&hex).unwrap(), oid);
        assert!(store.resolve_prefix(&hex[..3]).is_err());
        assert!(store.resolve_prefix("zzzzzz").is_err());
        assert!(store.resolve_prefix("0000000").is_err());
        fs::remove_dir_all(store.dir()).unwrap();
    }

    #[test]
    fn corrupted_object_is_detected() {
        let store = temp_store("corrupt");
        let oid = store.write(Kind::Blob, b"important").unwrap();
        let path = store.path_of(&oid);
        let mut bytes = fs::read(&path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        fs::remove_file(&path).unwrap();
        fs::write(&path, bytes).unwrap();
        assert!(store.read_raw(&oid).is_err());
        fs::remove_dir_all(store.dir()).unwrap();
    }
}
