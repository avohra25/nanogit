//! SHA-1 (FIPS 180-1), implemented from scratch.
//!
//! Git still names every object by the SHA-1 of its framed contents, so this
//! is the one hash function nanogit needs.

/// Streaming SHA-1 hasher.
#[derive(Clone)]
pub struct Sha1 {
    h: [u32; 5],
    block: [u8; 64],
    filled: usize,
    len: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha1 {
    /// Create a hasher in its initial state.
    pub fn new() -> Sha1 {
        Sha1 {
            h: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0],
            block: [0; 64],
            filled: 0,
            len: 0,
        }
    }

    /// Feed more bytes into the hash.
    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        if self.filled > 0 {
            let take = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == 64 {
                let block = self.block;
                compress(&mut self.h, &block);
                self.filled = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            compress(&mut self.h, &block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.filled = data.len();
        }
    }

    /// Finish hashing and return the 20-byte digest.
    pub fn finalize(mut self) -> [u8; 20] {
        let bit_len = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.filled != 56 {
            self.update(&[0]);
        }
        self.update(&bit_len.to_be_bytes());
        debug_assert_eq!(self.filled, 0);
        let mut out = [0u8; 20];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.h.iter()) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

fn compress(h: &mut [u32; 5], block: &[u8; 64]) {
    let mut w = [0u32; 80];
    for (slot, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
        *slot = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *h;
    for (i, &wi) in w.iter().enumerate() {
        let (f, k) = match i / 20 {
            0 => ((b & c) | (!b & d), 0x5A82_7999u32),
            1 => (b ^ c ^ d, 0x6ED9_EBA1),
            2 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
            _ => (b ^ c ^ d, 0xCA62_C1D6),
        };
        let temp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(wi);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = temp;
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
}

/// One-shot SHA-1 of a byte slice.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(data);
    h.finalize()
}

/// Lower-case hexadecimal encoding.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(DIGITS[usize::from(b >> 4)] as char);
        s.push(DIGITS[usize::from(b & 15)] as char);
    }
    s
}

/// Decode a hexadecimal string (either case). Returns `None` on bad input.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    bytes
        .chunks_exact(2)
        .map(|p| Some(nibble(p[0])? << 4 | nibble(p[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_of(data: &[u8]) -> String {
        to_hex(&sha1(data))
    }

    #[test]
    fn nist_vectors() {
        assert_eq!(hex_of(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex_of(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex_of(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            hex_of(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"),
            "a49b2446a02c645bf419f995b67091253a04a259"
        );
    }

    #[test]
    fn million_a() {
        let data = vec![b'a'; 1_000_000];
        assert_eq!(hex_of(&data), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    }

    #[test]
    fn git_blob_framing() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(
            hex_of(b"blob 6\0hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        // The well-known empty blob.
        assert_eq!(hex_of(b"blob 0\0"), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    }

    #[test]
    fn streaming_equals_one_shot_for_every_split() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 7 + 3) as u8).collect();
        let expect = sha1(&data);
        for split in 0..data.len() {
            let mut h = Sha1::new();
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(h.finalize(), expect, "split at {split}");
        }
    }

    #[test]
    fn padding_boundaries() {
        // Lengths around the 55/56/63/64 byte padding edge cases, checked
        // against digests produced by a reference implementation.
        let known = [
            (55usize, "c1c8bbdc22796e28c0e15163d20899b65621d65a"),
            (56, "c2db330f6083854c99d4b5bfb6e8f29f201be699"),
            (63, "03f09f5b158a7a8cdad920bddc29b81c18a551f5"),
            (64, "0098ba824b5c16427bd7a1122a5a442a25ec644d"),
        ];
        for (n, want) in known {
            assert_eq!(hex_of(&vec![b'a'; n]), want, "len {n}");
        }
    }

    #[test]
    fn hex_round_trip() {
        let d = sha1(b"x");
        let s = to_hex(&d);
        assert_eq!(from_hex(&s).unwrap(), d.to_vec());
        assert_eq!(from_hex("ABCDEF").unwrap(), vec![0xab, 0xcd, 0xef]);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }
}
