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
}
