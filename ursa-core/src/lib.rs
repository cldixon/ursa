//! # ursa-core
//!
//! The pure-Rust heart of Ursa: a compressed-sparse-row (CSR) **topology index**
//! and the **algorithm kernels** that run over it. This crate has *no* DataFusion
//! dependency — it takes Arrow columns (or plain dense slices) plus a shared
//! [`Topology`] in, and hands plain Rust vectors out. `ursa-plan` is responsible
//! for wrapping these kernels as DataFusion `ExecutionPlan`s; `ursa-py` exposes
//! them to Python.
//!
//! The governing rule (from the design spec):
//!
//! > **Arrow at the boundaries, index in the middle.** Topology lives in the
//! > index; edge *properties* stay in their original Arrow columns, reached
//! > through [`Topology`]'s `edge_ids` permutation — nothing is copied twice.
//!
//! ## Status
//!
//! [`Topology`], [`IdMap`], and the full kernel set are implemented and
//! unit-tested: `degree`, `pagerank` (+ weighted), `connected_components`,
//! `triangle_count`, `clustering_coefficient`, `closeness` (+ weighted),
//! `betweenness` (+ weighted Dijkstra-Brandes), `label_propagation`, `louvain`
//! (+ weighted), the BFS/frontier kernels (`bfs_distances`, `shortest_path` +
//! weighted, `k_hop`), neighbour aggregation, and `random_walk`.

pub mod algo;
pub mod id_map;
pub(crate) mod parallel;
pub mod topology;

pub use id_map::{IdError, IdMap};
pub use topology::{Adjacency, Direction, EdgeMask, Topology};
