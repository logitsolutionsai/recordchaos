//! Seeded random numbers. ChaCha8 gives the same stream on every platform.
//! Only `next_u64` is used, with our own range and shuffle helpers, so the
//! case stream does not depend on `rand` distribution code.

use rand_chacha::ChaCha8Rng;
use rand_chacha::rand_core::{RngCore, SeedableRng};

/// Deterministic random source for generation and operators.
pub struct CaseRng {
    inner: ChaCha8Rng,
}

impl CaseRng {
    pub fn new(seed: u64) -> Self {
        Self {
            inner: ChaCha8Rng::seed_from_u64(seed),
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.inner.next_u64()
    }

    /// Uniform integer in `0..n` (returns 0 when `n` is 0). Uses rejection
    /// sampling, so there is no modulo bias.
    pub fn below(&mut self, n: u64) -> u64 {
        if n <= 1 {
            return 0;
        }
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let x = self.next_u64();
            if x < zone {
                return x % n;
            }
        }
    }

    /// Uniform index into a slice of length `len`.
    pub fn index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }

    /// Pick an item by weight. Returns `None` when all weights are zero.
    pub fn weighted(&mut self, weights: &[u32]) -> Option<usize> {
        let total: u64 = weights.iter().map(|w| u64::from(*w)).sum();
        if total == 0 {
            return None;
        }
        let mut x = self.below(total);
        for (i, w) in weights.iter().enumerate() {
            let w = u64::from(*w);
            if x < w {
                return Some(i);
            }
            x -= w;
        }
        None
    }

    /// Fisher-Yates permutation of `0..len`.
    pub fn permutation(&mut self, len: usize) -> Vec<usize> {
        let mut p: Vec<usize> = (0..len).collect();
        for i in (1..len).rev() {
            let j = self.index(i + 1);
            p.swap(i, j);
        }
        p
    }

    /// A version 4 style UUID built from the stream (deterministic, not random).
    pub fn uuid(&mut self) -> String {
        let a = self.next_u64();
        let b = self.next_u64();
        let hi = (a & 0xffff_ffff_ffff_0fff) | 0x0000_0000_0000_4000;
        let lo = (b & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000;
        format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            hi >> 32,
            (hi >> 16) & 0xffff,
            hi & 0xffff,
            lo >> 48,
            lo & 0x0000_ffff_ffff_ffff
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = CaseRng::new(49302);
        let mut b = CaseRng::new(49302);
        for _ in 0..16 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn below_stays_in_range() {
        let mut r = CaseRng::new(1);
        for n in 1..50u64 {
            assert!(r.below(n) < n);
        }
    }

    #[test]
    fn permutation_is_a_permutation() {
        let mut r = CaseRng::new(7);
        let mut p = r.permutation(12);
        p.sort_unstable();
        assert_eq!(p, (0..12).collect::<Vec<_>>());
    }

    #[test]
    fn uuid_has_v4_shape() {
        let mut r = CaseRng::new(3);
        let u = r.uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
    }
}
