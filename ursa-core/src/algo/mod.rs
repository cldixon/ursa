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

mod clustering;
mod components;
mod degree;
mod hop;
mod neighbor_agg;
mod pagerank;
mod triangle;

pub use clustering::clustering_coefficient;
pub use components::connected_components_weak;
pub use degree::degree;
pub use hop::k_hop;
pub use neighbor_agg::{neighbor_aggregate, AggKind};
pub use pagerank::{pagerank, PageRankParams};
pub use triangle::triangle_count;

// ---------------------------------------------------------------------------
// Frontier kernels (BFS / k-hop / unweighted shortest path).
// ---------------------------------------------------------------------------
// `k_hop` (above) is the first, backing `ur.hop`. Still to come: a distance-
// returning `bfs` and the direction-optimizing top-down/bottom-up switch
// (Beamer et al.) ported from the GAP suite, which will back `ur.shortest_path`
// (unweighted) and BFS-derived stats (diameter, avg_path_length).
