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
