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
//! This is the v0.1 *skeleton*. [`Topology`], [`IdMap`], and the `degree`,
//! `pagerank`, and `connected_components` kernels are real and unit-tested.
//! `triangle_count` and the BFS/frontier kernels are stubbed with `todo!`-shaped
//! documentation and a clear porting target (the GAP Benchmark Suite).

pub mod algo;
pub mod id_map;
pub mod topology;

pub use id_map::IdMap;
pub use topology::{Adjacency, Direction, Topology};
