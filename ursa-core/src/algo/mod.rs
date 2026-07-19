//! Algorithm kernels.
//!
//! Kernels cluster into four computational shapes (spec §Algorithm kernels):
//!
//! | Shape | Algorithms | Technique |
//! |---|---|---|
//! | Fixpoint iteration    | PageRank, label propagation, Louvain | dense per-node vectors; Rayon sweep per iteration |
//! | Frontier expansion    | BFS, k-hop, unweighted shortest path | visited bitmaps + frontier queues; direction-optimizing BFS |
//! | Adjacency intersection| triangle count, clustering coefficient | sorted adjacency lists; parallel merge-intersection |
//! | Priority / disjoint-set| SSSP, connected components, betweenness | per-algorithm |
//!
//! **Do not invent these.** The GAP Benchmark Suite reference implementations are
//! canonical; port from them. The `graph` crate (Junghanns / Neo4j GDS lineage)
//! is worth evaluating as prior art before writing kernels from scratch.
//!
//! Every kernel takes a `&Topology` and returns dense, `u32`-indexed results;
//! translation back to user ids happens at the Arrow boundary in `ursa-plan`.

mod components;
mod degree;
mod pagerank;
mod triangle;

pub use components::connected_components_weak;
pub use degree::degree;
pub use pagerank::{pagerank, PageRankParams};
pub use triangle::triangle_count;

// ---------------------------------------------------------------------------
// Frontier kernels (BFS / k-hop / unweighted shortest path) — SKELETON.
// ---------------------------------------------------------------------------
// These back `ur.hop`, `ur.shortest_path` (unweighted), and BFS-derived stats.
// Port the direction-optimizing BFS (top-down/bottom-up switch, Beamer et al.)
// from the GAP suite. Signature sketch, intentionally not yet implemented:
//
//   pub fn bfs(topo: &Topology, source: u32, dir: Direction) -> Vec<i32> /* dist */
//   pub fn k_hop(topo: &Topology, seeds: &[u32], k: u32, dir: Direction)
//         -> (Vec<u32> /* reached */, Vec<u32> /* via edge_id */)
//
// Left as documented future work so the skeleton compiles and the seam is visible.
