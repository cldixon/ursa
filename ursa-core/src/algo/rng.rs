//! A tiny deterministic PRNG (`splitmix64`) for the seeded, dependency-free
//! kernels — the community-detection visit orders whose result must be
//! reproducible given a `seed` (spec §Determinism) without pulling `rand` into
//! `ursa-core`. (The frame-valued `random_walk` verb, which needs a
//! general-purpose stream, uses the `rand` crate instead.)

/// Default seed for the `seed=None` case, so an unseeded run is still fully
/// deterministic (deterministic-by-default, per the spec).
pub(crate) const DEFAULT_SEED: u64 = 0x5EED_5EED_5EED_5EED;

/// `splitmix64` — a well-distributed 64-bit generator with a tiny state.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, bound)`. `bound` must be non-zero. The modulo bias is
    /// negligible for the vertex-count bounds these kernels use.
    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }
}

/// Deterministically choose `k` distinct indices from `0..r` **without
/// replacement**, returned sorted ascending. A partial Fisher–Yates: reproducible
/// from `seed` (and portable/thread-count-independent, like [`shuffled_order`]);
/// `k` is clamped to `r`. `seed = None` uses [`DEFAULT_SEED`], so an unseeded
/// `sample` is still fully reproducible (deterministic-by-default, per the spec).
///
/// The caller is responsible for imposing a content-canonical row order *before*
/// mapping these indices onto rows, so the sample is independent of how the engine
/// partitioned the input (see `ursa_plan::query::sample_rows`).
pub fn sample_indices(r: usize, k: usize, seed: Option<u64>) -> Vec<usize> {
    let k = k.min(r);
    let mut pool: Vec<usize> = (0..r).collect();
    // A distinct fold constant from `shuffled_order` so the two never share a stream
    // for the same seed.
    let mut rng = SplitMix64::new(seed.unwrap_or(DEFAULT_SEED) ^ 0x2545_F491_4F6C_DD1D);
    // Partial Fisher–Yates: pick position i uniformly from the unpicked suffix.
    for i in 0..k {
        let j = i + rng.below(r - i);
        pool.swap(i, j);
    }
    let mut chosen = pool[..k].to_vec();
    chosen.sort_unstable();
    chosen
}

/// A deterministic, seed-derived permutation of `0..n` (Fisher–Yates). Community
/// kernels sweep nodes in this order so the `seed` knob reproducibly perturbs the
/// outcome.
pub(crate) fn shuffled_order(n: usize, seed: u64) -> Vec<u32> {
    let mut order: Vec<u32> = (0..n as u32).collect();
    // Fold in a constant so seed 0 isn't a degenerate all-zero state.
    let mut rng = SplitMix64::new(seed ^ 0xD1B5_4A32_D192_ED03);
    for i in (1..n).rev() {
        let j = rng.below(i + 1);
        order.swap(i, j);
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffle_is_a_permutation() {
        let order = shuffled_order(100, 42);
        let mut seen = order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..100u32).collect::<Vec<_>>());
    }

    #[test]
    fn shuffle_is_seed_deterministic() {
        assert_eq!(shuffled_order(50, 7), shuffled_order(50, 7));
        assert_ne!(shuffled_order(50, 7), shuffled_order(50, 8));
    }

    #[test]
    fn sample_is_a_sorted_distinct_subset() {
        let s = sample_indices(100, 10, Some(7));
        assert_eq!(s.len(), 10);
        assert!(s.iter().all(|&i| i < 100));
        assert!(s.windows(2).all(|w| w[0] < w[1])); // sorted, no duplicates
    }

    #[test]
    fn sample_is_seed_deterministic() {
        assert_eq!(
            sample_indices(100, 10, Some(7)),
            sample_indices(100, 10, Some(7))
        );
        assert_ne!(
            sample_indices(100, 10, Some(7)),
            sample_indices(100, 10, Some(8))
        );
        // seed=None is reproducible (uses DEFAULT_SEED).
        assert_eq!(sample_indices(100, 10, None), sample_indices(100, 10, None));
    }

    #[test]
    fn sample_clamps_when_k_exceeds_r() {
        assert_eq!(sample_indices(4, 1000, Some(1)), vec![0, 1, 2, 3]);
        assert_eq!(sample_indices(0, 5, Some(1)), Vec::<usize>::new());
    }
}
