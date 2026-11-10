//! Git's binary delta format, used by `OFS_DELTA` and `REF_DELTA` pack entries.
//!
//! A delta starts with the base size and the result size (little-endian base-128
//! varints), followed by instructions:
//!
//! * `1xxxxxxx` - copy from the base.  Bits 0-3 say which offset bytes follow,
//!   bits 4-6 which size bytes follow (a size of 0 means 0x10000).
//! * `0nnnnnnn` - insert the next `n` (1..=127) literal bytes.
//! * `00000000` - reserved; rejected.

use crate::error::{Error, Result};

/// Read one size varint (7 bits per byte, least significant group first).
pub fn read_size(data: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *data.get(*pos).ok_or_else(|| Error::corrupt("truncated delta header"))?;
        *pos += 1;
        if shift > 63 {
            return Err(Error::corrupt("delta size varint is too long"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
}

/// The `(base size, result size)` a delta declares.
pub fn sizes(delta: &[u8]) -> Result<(u64, u64)> {
    let mut pos = 0;
    let base = read_size(delta, &mut pos)?;
    let result = read_size(delta, &mut pos)?;
    Ok((base, result))
}

/// Rebuild the target object from `base` and `delta`.
pub fn apply(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0;
    let base_size = read_size(delta, &mut pos)?;
    let result_size = read_size(delta, &mut pos)?;
    if base_size != base.len() as u64 {
        return Err(Error::corrupt(format!(
            "delta expects a {base_size}-byte base, got {} bytes",
            base.len()
        )));
    }
    let result_size = usize::try_from(result_size).map_err(|_| Error::corrupt("delta result is too large"))?;
    let mut out = Vec::with_capacity(result_size);
    while pos < delta.len() {
        let op = delta[pos];
        pos += 1;
        if op & 0x80 != 0 {
            let mut offset = 0usize;
            let mut size = 0usize;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    let b = *delta.get(pos).ok_or_else(|| Error::corrupt("truncated delta copy"))?;
                    pos += 1;
                    offset |= usize::from(b) << (8 * i);
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    let b = *delta.get(pos).ok_or_else(|| Error::corrupt("truncated delta copy"))?;
                    pos += 1;
                    size |= usize::from(b) << (8 * i);
                }
            }
            if size == 0 {
                size = 0x10000;
            }
            let end = offset
                .checked_add(size)
                .filter(|&e| e <= base.len())
                .ok_or_else(|| Error::corrupt("delta copies past the end of its base"))?;
            if out.len() + size > result_size {
                return Err(Error::corrupt("delta produces more data than it declares"));
            }
            out.extend_from_slice(&base[offset..end]);
        } else if op != 0 {
            let n = usize::from(op);
            let lit = delta
                .get(pos..pos + n)
                .ok_or_else(|| Error::corrupt("truncated delta insert"))?;
            if out.len() + n > result_size {
                return Err(Error::corrupt("delta produces more data than it declares"));
            }
            out.extend_from_slice(lit);
            pos += n;
        } else {
            return Err(Error::corrupt("delta uses the reserved opcode 0"));
        }
    }
    if out.len() != result_size {
        return Err(Error::corrupt(format!(
            "delta produced {} bytes but declares {result_size}",
            out.len()
        )));
    }
    Ok(out)
}

fn write_size(mut n: usize, out: &mut Vec<u8>) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Block size used to index the base; matches shorter than this are not
/// worth a copy instruction.
const BLOCK: usize = 16;
/// Largest copy a single instruction carries (git's own encoder limit).
const MAX_COPY: usize = 0x10000;

fn flush_insert(lit: &[u8], out: &mut Vec<u8>) {
    for chunk in lit.chunks(127) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
}

fn push_copy(mut offset: usize, mut len: usize, out: &mut Vec<u8>) {
    while len > 0 {
        let n = len.min(MAX_COPY);
        let at = out.len();
        out.push(0x80);
        let mut op = 0x80u8;
        for i in 0..4 {
            let b = (offset >> (8 * i)) as u8;
            if b != 0 {
                op |= 1 << i;
                out.push(b);
            }
        }
        let size = if n == MAX_COPY { 0 } else { n };
        for i in 0..3 {
            let b = (size >> (8 * i)) as u8;
            if b != 0 {
                op |= 0x10 << i;
                out.push(b);
            }
        }
        out[at] = op;
        offset += n;
        len -= n;
    }
}

/// Encode `target` as a delta against `base`.
///
/// The base is indexed in non-overlapping 16-byte blocks; the target is
/// scanned byte by byte, and every block hit is extended backwards (into the
/// pending literal run) and forwards as far as the bytes agree.  The result
/// always round-trips through [`apply`], though it is not guaranteed to be
/// the smallest possible delta.
pub fn encode(base: &[u8], target: &[u8]) -> Vec<u8> {
    use std::collections::HashMap;
    let mut out = Vec::new();
    write_size(base.len(), &mut out);
    write_size(target.len(), &mut out);
    // Base offsets are limited to 32 bits by the copy instruction.
    let usable = base.len().min(u32::MAX as usize);
    let mut blocks: HashMap<&[u8], usize> = HashMap::new();
    let mut i = 0;
    while i + BLOCK <= usable {
        blocks.entry(&base[i..i + BLOCK]).or_insert(i);
        i += BLOCK;
    }
    let mut lit_start = 0;
    let mut p = 0;
    while p + BLOCK <= target.len() {
        let Some(&b) = blocks.get(&target[p..p + BLOCK]) else {
            p += 1;
            continue;
        };
        // Extend backwards over the literal run, then forwards.
        let (mut bs, mut ts) = (b, p);
        while bs > 0 && ts > lit_start && base[bs - 1] == target[ts - 1] {
            bs -= 1;
            ts -= 1;
        }
        let mut len = p + BLOCK - ts;
        while bs + len < usable && ts + len < target.len() && base[bs + len] == target[ts + len] {
            len += 1;
        }
        flush_insert(&target[lit_start..ts], &mut out);
        push_copy(bs, len, &mut out);
        p = ts + len;
        lit_start = p;
    }
    flush_insert(&target[lit_start..], &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(mut n: usize, out: &mut Vec<u8>) {
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    #[test]
    fn copy_and_insert() {
        let base = b"hello, world";
        let mut d = Vec::new();
        size(base.len(), &mut d);
        size(13, &mut d);
        d.extend_from_slice(&[0x80 | 0x10, 5]); // copy offset 0, size 5
        d.extend_from_slice(&[3, b'!', b'!', b' ']);
        d.extend_from_slice(&[0x80 | 0x01 | 0x10, 7, 5]); // copy offset 7, size 5
        assert_eq!(apply(base, &d).unwrap(), b"hello!! world");
        assert_eq!(sizes(&d).unwrap(), (12, 13));
    }

    #[test]
    fn zero_copy_size_means_64k() {
        let base = vec![7u8; 0x10000];
        let mut d = Vec::new();
        size(base.len(), &mut d);
        size(0x10000, &mut d);
        d.push(0x80);
        assert_eq!(apply(&base, &d).unwrap(), base);
    }

    #[test]
    fn encode_round_trips() {
        let base: Vec<u8> = (0..5000u32).flat_map(|i| format!("line {i}\n").into_bytes()).collect();
        let mut target = base.clone();
        target.splice(1000..1010, b"inserted text here".iter().copied());
        target.truncate(30_000);
        target.extend_from_slice(b"tail");
        let d = encode(&base, &target);
        assert!(d.len() < target.len() / 10, "delta of {} bytes", d.len());
        assert_eq!(apply(&base, &d).unwrap(), target);
        for (b, t) in [(&b""[..], &b""[..]), (b"abc", b""), (b"", b"xyz"), (b"same", b"same")] {
            assert_eq!(apply(b, &encode(b, t)).unwrap(), t);
        }
    }

    #[test]
    fn long_copies_are_split() {
        let base = vec![3u8; 200_000];
        let d = encode(&base, &base);
        assert_eq!(apply(&base, &d).unwrap(), base);
        assert!(d.len() < 40);
    }

    #[test]
    fn malformed_deltas_are_rejected() {
        let base = b"abc";
        // wrong base size
        assert!(apply(base, &[4, 1, 1, b'x']).is_err());
        // reserved opcode
        assert!(apply(base, &[3, 1, 0]).is_err());
        // copy past the end
        assert!(apply(base, &[3, 4, 0x80 | 0x10, 4]).is_err());
        // result too short
        assert!(apply(base, &[3, 5, 1, b'x']).is_err());
        // truncated insert
        assert!(apply(base, &[3, 2, 2, b'x']).is_err());
        // truncated header
        assert!(apply(base, &[0x83]).is_err());
    }
}
