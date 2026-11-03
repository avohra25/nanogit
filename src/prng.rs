//! A tiny seeded pseudo-random number generator (SplitMix64).
//!
//! It exists so randomized tests are reproducible without external crates.
//! It is *not* cryptographically secure.

/// SplitMix64 generator.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Create a generator from a seed; equal seeds give equal streams.
    pub fn new(seed: u64) -> Rng {
        Rng { state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03 }
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform-ish value in `0..n` (`n` must be non-zero).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Uniform-ish value in the inclusive range `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    /// True with probability `percent`/100.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    /// `n` random bytes.
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next_u64() as u8).collect()
    }

    /// `n` bytes drawn from an alphabet of `alphabet` letters (`a`, `b`, ...).
    /// Small alphabets are highly compressible.
    pub fn text(&mut self, n: usize, alphabet: u64) -> Vec<u8> {
        let alphabet = alphabet.clamp(1, 26);
        (0..n).map(|_| b'a' + self.below(alphabet) as u8).collect()
    }

    /// Pick an element of a non-empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_per_seed() {
        let a: Vec<u64> = (0..8).map(|_| 0).collect();
        let mut r1 = Rng::new(5);
        let mut r2 = Rng::new(5);
        let mut r3 = Rng::new(6);
        let s1: Vec<u64> = a.iter().map(|_| r1.next_u64()).collect();
        let s2: Vec<u64> = a.iter().map(|_| r2.next_u64()).collect();
        let s3: Vec<u64> = a.iter().map(|_| r3.next_u64()).collect();
        assert_eq!(s1, s2);
        assert_ne!(s1, s3);
    }

    #[test]
    fn ranges_are_respected() {
        let mut r = Rng::new(1);
        for _ in 0..1000 {
            assert!(r.below(7) < 7);
            let v = r.range(3, 5);
            assert!((3..=5).contains(&v));
        }
        assert_eq!(r.text(10, 1), b"aaaaaaaaaa");
        assert_eq!(*r.pick(&[9]), 9);
    }

    #[test]
    fn roughly_uniform() {
        let mut r = Rng::new(42);
        let mut buckets = [0u32; 8];
        for _ in 0..8000 {
            buckets[r.below(8) as usize] += 1;
        }
        assert!(buckets.iter().all(|&c| (800..1200).contains(&c)), "{buckets:?}");
        let hits = (0..10_000).filter(|_| r.chance(30)).count();
        assert!((2700..3300).contains(&hits), "{hits}");
    }
}
