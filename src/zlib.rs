//! zlib (RFC 1950) and DEFLATE (RFC 1951), implemented from scratch.
//!
//! * [`inflate`] / [`zlib_decompress`] decode every kind of DEFLATE block:
//!   stored, fixed Huffman and dynamic Huffman.
//! * [`deflate`] encodes with a hash-chain LZ77 matcher and the fixed Huffman
//!   code; [`deflate_stored`] emits stored blocks only.  [`zlib_compress`]
//!   picks whichever of the two is smaller.

use std::fmt;

/// Reasons a zlib or DEFLATE stream is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZlibError {
    /// The input ended in the middle of the stream.
    UnexpectedEof,
    /// The two-byte zlib header is invalid.
    BadHeader,
    /// The stream asks for a preset dictionary, which is not supported.
    UnsupportedDictionary,
    /// A block used the reserved type `11`.
    BadBlockType,
    /// A stored block's length and its complement disagree.
    BadStoredLength,
    /// A dynamic block described an invalid Huffman code.
    BadHuffmanTable,
    /// A Huffman symbol could not be decoded or is out of range.
    BadSymbol,
    /// A back-reference points before the start of the output.
    BadDistance,
    /// The Adler-32 trailer does not match the decoded data.
    ChecksumMismatch,
}

impl fmt::Display for ZlibError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            ZlibError::UnexpectedEof => "unexpected end of compressed data",
            ZlibError::BadHeader => "invalid zlib header",
            ZlibError::UnsupportedDictionary => "preset dictionaries are not supported",
            ZlibError::BadBlockType => "invalid deflate block type",
            ZlibError::BadStoredLength => "stored block length check failed",
            ZlibError::BadHuffmanTable => "invalid Huffman table",
            ZlibError::BadSymbol => "invalid Huffman symbol",
            ZlibError::BadDistance => "back-reference distance too far back",
            ZlibError::ChecksumMismatch => "adler32 checksum mismatch",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for ZlibError {}

type Result<T> = std::result::Result<T, ZlibError>;

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Adler-32 checksum (RFC 1950 section 8.2).
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

// ---------------------------------------------------------------- inflate

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u32,
    count: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0, buf: 0, count: 0 }
    }

    /// Read `need` (at most 16) bits, least significant first.
    fn bits(&mut self, need: u32) -> Result<u32> {
        while self.count < need {
            let byte = *self.data.get(self.pos).ok_or(ZlibError::UnexpectedEof)?;
            self.pos += 1;
            self.buf |= u32::from(byte) << self.count;
            self.count += 8;
        }
        let value = self.buf & ((1u32 << need) - 1);
        self.buf >>= need;
        self.count -= need;
        Ok(value)
    }

    /// Drop the partial byte so the next read starts on a byte boundary.
    fn align(&mut self) {
        self.buf = 0;
        self.count = 0;
    }
}

/// Canonical Huffman decoding table (count of codes per length plus the
/// symbols ordered by code), decoded one bit at a time.
struct Huffman {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman> {
        let mut count = [0u16; 16];
        for &len in lengths {
            count[usize::from(len)] += 1;
        }
        let mut left: i32 = 1;
        for &c in &count[1..] {
            left = (left << 1) - i32::from(c);
            if left < 0 {
                return Err(ZlibError::BadHuffmanTable);
            }
        }
        let mut offsets = [0usize; 16];
        for i in 1..15 {
            offsets[i + 1] = offsets[i] + usize::from(count[i]);
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let slot = &mut offsets[usize::from(len)];
                symbol[*slot] = sym as u16;
                *slot += 1;
            }
        }
        Ok(Huffman { count, symbol })
    }

    fn decode(&self, br: &mut BitReader<'_>) -> Result<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for &count in &self.count[1..] {
            code |= br.bits(1)? as i32;
            let count = i32::from(count);
            if code - count < first {
                return self
                    .symbol
                    .get((index + (code - first)) as usize)
                    .copied()
                    .ok_or(ZlibError::BadSymbol);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(ZlibError::BadSymbol)
    }
}

fn fixed_tables() -> Result<(Huffman, Huffman)> {
    let mut lit = [8u8; 288];
    lit[144..256].fill(9);
    lit[256..280].fill(7);
    Ok((Huffman::new(&lit)?, Huffman::new(&[5u8; 30])?))
}

fn dynamic_tables(br: &mut BitReader<'_>) -> Result<(Huffman, Huffman)> {
    let nlen = br.bits(5)? as usize + 257;
    let ndist = br.bits(5)? as usize + 1;
    let ncode = br.bits(4)? as usize + 4;
    if nlen > 286 || ndist > 30 {
        return Err(ZlibError::BadHuffmanTable);
    }
    let mut clens = [0u8; 19];
    for &slot in CLEN_ORDER.iter().take(ncode) {
        clens[slot] = br.bits(3)? as u8;
    }
    let clen_code = Huffman::new(&clens)?;
    let total = nlen + ndist;
    let mut lengths = vec![0u8; total];
    let mut idx = 0;
    while idx < total {
        let sym = clen_code.decode(br)?;
        let (value, repeat) = match sym {
            0..=15 => (sym as u8, 1),
            16 => {
                if idx == 0 {
                    return Err(ZlibError::BadHuffmanTable);
                }
                (lengths[idx - 1], 3 + br.bits(2)? as usize)
            }
            17 => (0, 3 + br.bits(3)? as usize),
            _ => (0, 11 + br.bits(7)? as usize),
        };
        if idx + repeat > total {
            return Err(ZlibError::BadHuffmanTable);
        }
        lengths[idx..idx + repeat].fill(value);
        idx += repeat;
    }
    if lengths[256] == 0 {
        return Err(ZlibError::BadHuffmanTable);
    }
    Ok((Huffman::new(&lengths[..nlen])?, Huffman::new(&lengths[nlen..])?))
}

fn inflate_codes(
    br: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<()> {
    loop {
        let sym = usize::from(lit.decode(br)?);
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()),
            257..=285 => {
                let i = sym - 257;
                let len = usize::from(LEN_BASE[i]) + br.bits(u32::from(LEN_EXTRA[i]))? as usize;
                let ds = usize::from(dist.decode(br)?);
                if ds >= 30 {
                    return Err(ZlibError::BadSymbol);
                }
                let d = usize::from(DIST_BASE[ds]) + br.bits(u32::from(DIST_EXTRA[ds]))? as usize;
                if d > out.len() {
                    return Err(ZlibError::BadDistance);
                }
                let start = out.len() - d;
                for k in 0..len {
                    let byte = out[start + k];
                    out.push(byte);
                }
            }
            _ => return Err(ZlibError::BadSymbol),
        }
    }
}

/// Decode a raw DEFLATE stream; also returns how many input bytes it used.
fn inflate_stream(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut br = BitReader::new(data);
    let mut out = Vec::with_capacity(data.len().saturating_mul(3));
    loop {
        let last = br.bits(1)? == 1;
        match br.bits(2)? {
            0 => {
                br.align();
                let header = data
                    .get(br.pos..br.pos + 4)
                    .ok_or(ZlibError::UnexpectedEof)?;
                let len = usize::from(u16::from_le_bytes([header[0], header[1]]));
                let nlen = u16::from_le_bytes([header[2], header[3]]);
                if nlen != !(len as u16) {
                    return Err(ZlibError::BadStoredLength);
                }
                br.pos += 4;
                let body = data
                    .get(br.pos..br.pos + len)
                    .ok_or(ZlibError::UnexpectedEof)?;
                out.extend_from_slice(body);
                br.pos += len;
            }
            1 => {
                let (lit, dist) = fixed_tables()?;
                inflate_codes(&mut br, &mut out, &lit, &dist)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut br)?;
                inflate_codes(&mut br, &mut out, &lit, &dist)?;
            }
            _ => return Err(ZlibError::BadBlockType),
        }
        if last {
            break;
        }
    }
    Ok((out, br.pos))
}

/// Decode a raw DEFLATE stream (RFC 1951) with no zlib wrapper.
pub fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    inflate_stream(data).map(|(out, _)| out)
}

/// Decode a zlib stream (RFC 1950), verifying the Adler-32 trailer.
pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>> {
    zlib_decompress_prefix(data).map(|(out, _)| out)
}

/// Decode a zlib stream that may be followed by unrelated bytes (as inside a
/// packfile).  Returns the decoded data and how many input bytes the stream
/// used, trailer included.
pub fn zlib_decompress_prefix(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    if data.len() < 2 {
        return Err(ZlibError::UnexpectedEof);
    }
    let (cmf, flg) = (data[0], data[1]);
    if cmf & 0x0f != 8 || cmf >> 4 > 7 || (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
        return Err(ZlibError::BadHeader);
    }
    if flg & 0x20 != 0 {
        return Err(ZlibError::UnsupportedDictionary);
    }
    let (out, used) = inflate_stream(&data[2..])?;
    let at = 2 + used;
    let trailer = data.get(at..at + 4).ok_or(ZlibError::UnexpectedEof)?;
    let want = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    if want != adler32(&out) {
        return Err(ZlibError::ChecksumMismatch);
    }
    Ok((out, at + 4))
}

// ---------------------------------------------------------------- deflate

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter { out: Vec::new(), acc: 0, n: 0 }
    }

    /// Append `nbits` bits of `value`, least significant bit first.
    fn put(&mut self, value: u32, nbits: u32) {
        self.acc |= value << self.n;
        self.n += nbits;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// Append a Huffman code, which DEFLATE packs most significant bit first.
    fn put_code(&mut self, code: u32, len: u32) {
        self.put(code.reverse_bits() >> (32 - len), len);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// The fixed literal/length code from RFC 1951 section 3.2.6.
fn put_fixed_symbol(w: &mut BitWriter, sym: u32) {
    match sym {
        0..=143 => w.put_code(0x30 + sym, 8),
        144..=255 => w.put_code(0x190 + (sym - 144), 9),
        256..=279 => w.put_code(sym - 256, 7),
        _ => w.put_code(0xC0 + (sym - 280), 8),
    }
}

const HASH_BITS: u32 = 15;
const WINDOW: usize = 32_768;
const MAX_MATCH: usize = 258;
const MAX_CHAIN: usize = 48;
const NIL: usize = usize::MAX;

fn hash3(d: &[u8], i: usize) -> usize {
    let h = (u32::from(d[i]) << 10) ^ (u32::from(d[i + 1]) << 5) ^ u32::from(d[i + 2]);
    (h as usize) & ((1 << HASH_BITS) - 1)
}

struct Matcher<'a> {
    data: &'a [u8],
    head: Vec<usize>,
    prev: Vec<usize>,
}

impl<'a> Matcher<'a> {
    fn new(data: &'a [u8]) -> Self {
        Matcher {
            data,
            head: vec![NIL; 1 << HASH_BITS],
            prev: vec![NIL; data.len()],
        }
    }

    fn insert(&mut self, i: usize) {
        if i + 2 < self.data.len() {
            let h = hash3(self.data, i);
            self.prev[i] = self.head[h];
            self.head[h] = i;
        }
    }

    /// Longest earlier match for position `i` as `(length, distance)`.
    fn find(&self, i: usize) -> (usize, usize) {
        let data = self.data;
        if i + 2 >= data.len() {
            return (0, 0);
        }
        let limit = MAX_MATCH.min(data.len() - i);
        let mut best = (0, 0);
        let mut cand = self.head[hash3(data, i)];
        let mut chain = 0;
        while cand != NIL && i - cand <= WINDOW && chain < MAX_CHAIN {
            let len = data[cand..]
                .iter()
                .zip(&data[i..i + limit])
                .take_while(|(a, b)| a == b)
                .count();
            if len > best.0 {
                best = (len, i - cand);
                if len == limit {
                    break;
                }
            }
            cand = self.prev[cand];
            chain += 1;
        }
        best
    }
}

/// Compress with LZ77 and the fixed Huffman code (one block).
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.put(1, 1); // BFINAL
    w.put(1, 2); // BTYPE = fixed Huffman
    let mut matcher = Matcher::new(data);
    let mut i = 0;
    while i < data.len() {
        let (len, dist) = matcher.find(i);
        if len >= 3 {
            let li = LEN_BASE.iter().rposition(|&b| usize::from(b) <= len).unwrap_or(0);
            put_fixed_symbol(&mut w, 257 + li as u32);
            w.put((len - usize::from(LEN_BASE[li])) as u32, u32::from(LEN_EXTRA[li]));
            let di = DIST_BASE.iter().rposition(|&b| usize::from(b) <= dist).unwrap_or(0);
            w.put_code(di as u32, 5);
            w.put((dist - usize::from(DIST_BASE[di])) as u32, u32::from(DIST_EXTRA[di]));
            for p in i..i + len {
                matcher.insert(p);
            }
            i += len;
        } else {
            put_fixed_symbol(&mut w, u32::from(data[i]));
            matcher.insert(i);
            i += 1;
        }
    }
    put_fixed_symbol(&mut w, 256);
    w.finish()
}

/// Emit `data` as a sequence of stored (uncompressed) blocks.
pub fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 5 * (data.len() / 65_535 + 1));
    let mut chunks = data.chunks(65_535).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(chunk) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out
}

/// Compress into a zlib stream, choosing fixed-Huffman or stored blocks,
/// whichever is smaller.
pub fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut body = deflate(data);
    if body.len() > data.len() + 5 {
        body = deflate_stored(data);
    }
    let mut out = Vec::with_capacity(body.len() + 6);
    out.extend_from_slice(&[0x78, 0x9c]);
    out.extend_from_slice(&body);
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prng::Rng;
    use crate::sha1::from_hex;

    const DYNAMIC_TEXT: &str = "gamma beta delta zeta alpha alpha epsilon alpha gamma epsilon alpha epsilon beta alpha alpha delta delta alpha beta alpha epsilon delta alpha epsilon alpha beta zeta zeta epsilon alpha epsilon epsilon delta alpha beta alpha epsilon beta gamma delta beta epsilon alpha epsilon gamma epsilon zeta beta alpha epsilon epsilon zeta beta gamma alpha epsilon zeta alpha epsilon alpha epsilon beta delta zeta epsilon delta gamma delta epsilon delta gamma gamma beta beta zeta beta alpha epsilon gamma epsilon delta gamma zeta delta gamma epsilon alpha alpha epsilon delta beta gamma beta delta delta alpha zeta alpha epsilon epsilon gamma gamma zeta gamma epsilon delta epsilon delta alpha alpha gamma delta zeta zeta alpha alpha zeta zeta gamma zeta";
    const DYNAMIC_ZLIB: &str = "78da7552d10e843008fb157f6d8b646732ef4cbc27be5e1db7a368f7421c94b68025ad6b9ab27cd3344b3da35e9fa96eaf1e65db97fa79ff5ea5e163aebff2bdd5182d5a0620bd0bcb91b781f51fb828a3212a2d65e60d99c79471c6a64d189f75eb8b2895c170c118ac3ece838e59a5f8f97c5bc46c1c0919d4f5d96dd9ad6058708f07d0f1bad0b7ca43958d8a4e7021fe6b20c2b32e720062f4103b";

    #[test]
    fn adler_known_values() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(&vec![0xffu8; 100_000]), {
            let (mut a, mut b) = (1u64, 0u64);
            for _ in 0..100_000 {
                a = (a + 255) % 65_521;
                b = (b + a) % 65_521;
            }
            ((b << 16) | a) as u32
        });
    }

    #[test]
    fn decodes_real_zlib_streams() {
        // Produced by CPython's zlib at level 9.
        let hello = from_hex("78dacb48cdc9c9d75128cf2fca49e1020021e70493").unwrap();
        assert_eq!(zlib_decompress(&hello).unwrap(), b"hello, world\n");
        let empty = from_hex("78da030000000001").unwrap();
        assert_eq!(zlib_decompress(&empty).unwrap(), b"");
    }

    #[test]
    fn decodes_stored_block() {
        let stored = from_hex("7801011100eeff73746f72656420626c6f636b20646174613bcd0677").unwrap();
        assert_eq!(zlib_decompress(&stored).unwrap(), b"stored block data");
    }

    #[test]
    fn decodes_fixed_block() {
        let fixed = from_hex("78014b4c4a4e8421858aca2a180600718c093a").unwrap();
        assert_eq!(zlib_decompress(&fixed).unwrap(), b"abcabcabcabc xyz xyz xyz");
    }

    #[test]
    fn decodes_dynamic_huffman_block() {
        let stream = from_hex(DYNAMIC_ZLIB).unwrap();
        assert_eq!((stream[2] >> 1) & 3, 2, "test vector must be a dynamic block");
        assert_eq!(zlib_decompress(&stream).unwrap(), DYNAMIC_TEXT.as_bytes());
    }

    #[test]
    fn rejects_corruption() {
        let mut stream = from_hex(DYNAMIC_ZLIB).unwrap();
        assert_eq!(zlib_decompress(&stream[..stream.len() - 1]), Err(ZlibError::UnexpectedEof));
        let last = stream.len() - 1;
        stream[last] ^= 1;
        assert_eq!(zlib_decompress(&stream), Err(ZlibError::ChecksumMismatch));
        assert_eq!(zlib_decompress(&[0x79, 0x9c, 0, 0, 0, 0]), Err(ZlibError::BadHeader));
        assert_eq!(zlib_decompress(&[0x78, 0x9d]), Err(ZlibError::BadHeader));
        assert_eq!(zlib_decompress(&[0x78]), Err(ZlibError::UnexpectedEof));
        // Reserved block type 3.
        assert_eq!(inflate(&[0x07]), Err(ZlibError::BadBlockType));
        // Stored block with a wrong complement.
        assert_eq!(inflate(&[1, 1, 0, 0, 0, 0]), Err(ZlibError::BadStoredLength));
        // Fixed block whose first symbol is a match with nothing to copy.
        assert_eq!(inflate(&[0x03, 0x02, 0x00]), Err(ZlibError::BadDistance));
    }

    #[test]
    fn dictionary_flag_is_reported() {
        // CMF=0x78, FLG with FDICT set and a valid check value.
        let mut flg = 0x20u8;
        while (0x78u16 * 256 + u16::from(flg)) % 31 != 0 {
            flg += 1;
        }
        assert_eq!(
            zlib_decompress(&[0x78, flg, 0, 0, 0, 0, 0, 0]),
            Err(ZlibError::UnsupportedDictionary)
        );
    }

    #[test]
    fn deflate_round_trips_edge_cases() {
        let long_run = vec![7u8; 70_000];
        let cases: Vec<&[u8]> = vec![b"", b"a", b"ab", b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", &long_run];
        for data in cases {
            assert_eq!(inflate(&deflate(data)).unwrap(), data);
            assert_eq!(inflate(&deflate_stored(data)).unwrap(), data);
            assert_eq!(zlib_decompress(&zlib_compress(data)).unwrap(), data);
        }
    }

    #[test]
    fn deflate_actually_compresses() {
        let data = DYNAMIC_TEXT.repeat(20);
        let packed = zlib_compress(data.as_bytes());
        assert!(packed.len() * 8 < data.len(), "{} vs {}", packed.len(), data.len());
        assert_eq!(zlib_decompress(&packed).unwrap(), data.as_bytes());
    }

    #[test]
    fn incompressible_data_falls_back_to_stored() {
        let mut rng = Rng::new(99);
        let data = rng.bytes(5000);
        let packed = zlib_compress(&data);
        assert!(packed.len() <= data.len() + 11);
        assert_eq!(zlib_decompress(&packed).unwrap(), data);
    }

    #[test]
    fn stored_handles_multiple_blocks() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(inflate(&deflate_stored(&data)).unwrap(), data);
    }

    #[test]
    fn randomized_round_trips() {
        for seed in 0..300u64 {
            let mut rng = Rng::new(seed);
            let len = rng.below(4000) as usize;
            let data = match seed % 4 {
                0 => rng.bytes(len),
                1 => rng.text(len, 6),
                2 => rng.text(len, 40),
                _ => {
                    // Long repeats mixed with noise exercise far matches.
                    let n = 50 + rng.below(300) as usize;
                    let chunk = rng.bytes(n);
                    let mut v = Vec::new();
                    while v.len() < len {
                        v.extend_from_slice(&chunk);
                        let m = rng.below(20) as usize;
                        v.extend(rng.bytes(m));
                    }
                    v
                }
            };
            let packed = deflate(&data);
            assert_eq!(inflate(&packed).unwrap(), data, "seed {seed}");
            let z = zlib_compress(&data);
            assert_eq!(zlib_decompress(&z).unwrap(), data, "seed {seed}");
        }
    }
}
