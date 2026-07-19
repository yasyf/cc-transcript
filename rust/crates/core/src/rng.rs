use sha2::{Digest, Sha256};

/// The splitmix64 PRNG (Vigna's reference variant): a 64-bit state advanced by the
/// golden-ratio increment, its output finalized by the standard avalanche mix.
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
}

/// Seeds a `SplitMix64` from the first 8 bytes, little-endian, of the SHA-256 of
/// `material`, so a string key maps to a stable, well-distributed 64-bit state.
pub fn seeded(material: &str) -> SplitMix64 {
    let digest = Sha256::digest(material.as_bytes());
    SplitMix64 {
        state: u64::from_le_bytes(digest[..8].try_into().unwrap()),
    }
}

/// Draws uniformly below `bound` by rejection: draws below `2^64 mod bound` fold
/// unevenly under a bare modulo, so they re-draw (probability `bound / 2^64`).
fn uniform_below(rng: &mut SplitMix64, bound: u64) -> u64 {
    let threshold = bound.wrapping_neg() % bound;
    loop {
        let draw = rng.next_u64();
        if draw >= threshold {
            return draw % bound;
        }
    }
}

/// Draws `k` distinct indexes from `0..len` in draw order — a partial Fisher-Yates
/// shuffle (uniform, without replacement) over a `0..len` pool. Requires `k <= len`.
pub fn sample_indexes(rng: &mut SplitMix64, len: usize, k: usize) -> Vec<usize> {
    let mut pool: Vec<usize> = (0..len).collect();
    let mut out = Vec::with_capacity(k);
    for i in 0..k {
        let j = i + uniform_below(rng, (len - i) as u64) as usize;
        pool.swap(i, j);
        out.push(pool[i]);
    }
    out
}

/// Rounds `x` to the nearest integer with ties going to the even neighbor, matching
/// Python's built-in `round()` (banker's rounding).
pub fn round_half_even(x: f64) -> i64 {
    let lower = x.floor();
    let lower_int = lower as i64;
    let frac = x - lower;
    if frac < 0.5 {
        lower_int
    } else if frac > 0.5 || lower_int % 2 != 0 {
        lower_int + 1
    } else {
        lower_int
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_matches_reference_vectors() {
        // Vigna's canonical splitmix64 outputs for a zero seed.
        let mut rng = SplitMix64 { state: 0 };
        assert_eq!(
            [
                rng.next_u64(),
                rng.next_u64(),
                rng.next_u64(),
                rng.next_u64()
            ],
            [
                0xe220a8397b1dcdaf,
                0x6e789e6aa1b965f4,
                0x06c45d188009454f,
                0xf88bb8a8724c81ec,
            ]
        );
    }

    #[test]
    fn seeded_pins_sha_derived_state() {
        let mut rng = seeded("cc-transcript");
        assert_eq!(
            [rng.next_u64(), rng.next_u64(), rng.next_u64()],
            [0x07c7a5a7348971ba, 0x3739bf88bcd1cff5, 0x8952cc30dcb7b010]
        );
    }

    #[test]
    fn uniform_below_covers_the_full_range_without_bias_zone() {
        let mut rng = SplitMix64 { state: 0 };
        assert_eq!(uniform_below(&mut rng, 1), 0);
        let draws: Vec<u64> = (0..64).map(|_| uniform_below(&mut rng, 3)).collect();
        assert!(draws.iter().all(|&d| d < 3));
        assert!((0..3).all(|v| draws.contains(&v)));
    }

    #[test]
    fn sample_indexes_pins_seeded_draw() {
        let mut rng = seeded("7:33333333-3333-3333-3333-333333333333");
        assert_eq!(sample_indexes(&mut rng, 15, 5), [5, 14, 0, 10, 11]);
    }

    #[test]
    fn sample_indexes_permutes_when_k_equals_len() {
        let mut rng = SplitMix64 { state: 0 };
        let draw = sample_indexes(&mut rng, 10, 10);
        assert_eq!(draw, [5, 1, 9, 7, 0, 4, 3, 2, 6, 8]);
        let mut sorted = draw.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn sample_indexes_partial_draw_is_pinned() {
        let mut rng = SplitMix64 { state: 0 };
        assert_eq!(sample_indexes(&mut rng, 8, 3), [7, 2, 3]);
    }

    #[test]
    fn round_half_even_ties_to_even() {
        assert_eq!(round_half_even(0.5), 0);
        assert_eq!(round_half_even(1.5), 2);
        assert_eq!(round_half_even(2.5), 2);
        assert_eq!(round_half_even(-0.5), 0);
        assert_eq!(round_half_even(-1.5), -2);
        assert_eq!(round_half_even(2.3), 2);
    }
}
