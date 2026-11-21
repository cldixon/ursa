//! The layouts' bits, pinned.
//!
//! `Fa2State` gained a fixed-node mask and a per-node inertia (#442). A state
//! that sets neither must give the positions it always has: layouts are cached
//! to Parquet and compared across versions. These hashes were taken from the
//! code before the masks existed. The linear force law uses nothing but
//! `+ - * /` and `sqrt`, so they hold on every platform and thread count.

use ursa_core::algo::{
    layout_fa2, layout_fa2_multilevel, layout_fr, Fa2Params, LayoutParams, MultilevelParams,
};
use ursa_core::Topology;

fn seeded_graph(n: usize, m: usize) -> Topology {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u32
    };
    let mut src = Vec::with_capacity(m);
    let mut dst = Vec::with_capacity(m);
    for _ in 0..m {
        src.push(next() % n as u32);
        dst.push(next() % n as u32);
    }
    Topology::build(n, src, dst)
}

/// FNV-1a over the positions' bits, x then y.
fn hash(p: &(Vec<f32>, Vec<f32>)) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in p.0.iter().chain(&p.1) {
        for b in v.to_bits().to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

#[test]
fn layouts_without_masks_have_the_bits_they_always_had() {
    let topo = seeded_graph(1500, 3000);
    let fa2 = layout_fa2(
        &topo,
        None,
        Fa2Params {
            iterations: 80,
            seed: Some(11),
            ..Fa2Params::default()
        },
    );
    let strong = layout_fa2(
        &topo,
        None,
        Fa2Params {
            iterations: 40,
            strong_gravity: true,
            theta: 1.2,
            ..Fa2Params::default()
        },
    );
    let multilevel = layout_fa2_multilevel(
        &topo,
        None,
        MultilevelParams {
            max_iterations: 30,
            ..MultilevelParams::default()
        },
    );
    let fr = layout_fr(
        &topo,
        None,
        LayoutParams {
            iterations: 30,
            ..LayoutParams::default()
        },
    );
    let got = [hash(&fa2), hash(&strong), hash(&multilevel), hash(&fr)];
    assert_eq!(got, GOLDEN, "got {got:#x?}");
}

const GOLDEN: [u64; 4] = [
    0x8b37_061d_12a3_5354,
    0xfb28_3fb2_9452_9447,
    0x5bc8_dd0e_8dc4_0c10,
    0xef23_fee1_7b62_24ae,
];
