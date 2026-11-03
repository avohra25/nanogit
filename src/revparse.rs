//! Revision syntax: `HEAD`, branches, tags, hex prefixes, `rev~N`, `rev^N`,
//! `rev^{commit}`, `rev^{tree}` and `rev:path`.

use crate::error::{Error, Result};
use crate::history::{peel_to_commit, read_commit};
use crate::object::{Object, Oid};
use crate::repo::Repo;
use crate::tree::lookup;

impl Repo {
    /// Resolve a revision expression to an object id.
    pub fn rev_parse(&self, spec: &str) -> Result<Oid> {
        if let Some((rev, path)) = spec.split_once(':') {
            if !rev.is_empty() {
                let tree = self.tree_of(&self.rev_parse(rev)?)?;
                return match lookup(&self.odb(), &tree, path)? {
                    Some((_, oid)) => Ok(oid),
                    None => Err(Error::fatal(format!("path '{path}' does not exist in '{rev}'"))),
                };
            }
        }
        let cut = spec.find(['~', '^']).unwrap_or(spec.len());
        let (base, mut rest) = spec.split_at(cut);
        let mut oid = self.resolve_name(base)?;
        let odb = self.odb();
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix("^{") {
                let close = after
                    .find('}')
                    .ok_or_else(|| Error::fatal(format!("bad revision '{spec}'")))?;
                oid = match &after[..close] {
                    "commit" => peel_to_commit(&odb, &oid)?,
                    "tree" => self.tree_of(&oid)?,
                    "" => oid,
                    other => return Err(Error::fatal(format!("unsupported peel type '{other}' in '{spec}'"))),
                };
                rest = &after[close + 1..];
            } else if let Some(after) = rest.strip_prefix('~') {
                let digits = after.bytes().take_while(u8::is_ascii_digit).count();
                let n: usize = if digits == 0 { 1 } else { after[..digits].parse().unwrap_or(usize::MAX) };
                for _ in 0..n {
                    let commit = read_commit(&odb, &oid)?;
                    oid = *commit
                        .parents
                        .first()
                        .ok_or_else(|| Error::fatal(format!("revision '{spec}' goes past the root commit")))?;
                }
                rest = &after[digits..];
            } else if let Some(after) = rest.strip_prefix('^') {
                let digits = after.bytes().take_while(u8::is_ascii_digit).count();
                let n: usize = if digits == 0 { 1 } else { after[..digits].parse().unwrap_or(usize::MAX) };
                if n == 0 {
                    oid = peel_to_commit(&odb, &oid)?;
                } else {
                    let commit = read_commit(&odb, &oid)?;
                    oid = *commit
                        .parents
                        .get(n - 1)
                        .ok_or_else(|| Error::fatal(format!("'{spec}' has no parent number {n}")))?;
                }
                rest = &after[digits..];
            } else {
                return Err(Error::fatal(format!("bad revision '{spec}'")));
            }
        }
        Ok(oid)
    }

    /// Resolve a bare name: `HEAD`, a ref, or a hex id/prefix.
    pub fn resolve_name(&self, name: &str) -> Result<Oid> {
        if name == "HEAD" || name == "@" {
            return self
                .head_oid()?
                .ok_or_else(|| Error::fatal("ambiguous argument 'HEAD': unknown revision (no commits yet)"));
        }
        for candidate in [
            name.to_string(),
            format!("refs/{name}"),
            format!("refs/tags/{name}"),
            format!("refs/heads/{name}"),
        ] {
            if candidate.starts_with("refs/") && crate::refs::valid_ref_name(&candidate) {
                if let Some(oid) = self.read_ref(&candidate)? {
                    return Ok(oid);
                }
            }
        }
        if name.len() >= 4 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
            return self.odb().resolve_prefix(name);
        }
        Err(Error::fatal(format!("ambiguous argument '{name}': unknown revision or path")))
    }

    /// The tree a commit, tag or tree object refers to.
    pub fn tree_of(&self, oid: &Oid) -> Result<Oid> {
        match self.odb().read(oid)? {
            Object::Tree(_) => Ok(*oid),
            Object::Commit(c) => Ok(c.tree),
            Object::Tag(t) => self.tree_of(&t.object),
            Object::Blob(_) => Err(Error::fatal(format!("{oid} is a blob, not a tree-ish"))),
        }
    }
}
