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

mod betweenness;
mod bfs;
mod closeness;
mod clustering;
mod components;
mod degree;
mod hop;
mod label_prop;
mod layout;
mod louvain;
mod neighbor_agg;
mod pagerank;
/// Public because it is independently useful — a deterministic spatial index over
/// 2-D positions — and because `examples/layout_scaling.rs` times the build
/// separately from the traversal, which is how we know which half to optimize.
pub mod quadtree;
mod random_walk;
mod rng;
pub use rng::sample_indices;
mod triangle;

pub use betweenness::{betweenness, betweenness_weighted};
pub use bfs::{
    bfs_distances, dijkstra_distances, shortest_path, shortest_path_weighted,
    shortest_path_weighted_with_cost,
};
pub use closeness::{closeness, closeness_weighted};
pub use clustering::{clustering_coefficient, clustering_from_triangles};
pub use components::{connected_components_strong, connected_components_weak};
pub use degree::degree;
pub use hop::{k_hop, k_hop_reached_set};
pub use label_prop::label_propagation;
pub use layout::{
    layout_circle, layout_fa2, layout_fr, layout_random, Fa2Params, LayoutParams, Positions,
};
pub use louvain::{louvain, louvain_weighted};
pub use neighbor_agg::{neighbor_aggregate, AggKind};
pub use pagerank::{pagerank, pagerank_weighted, PageRankParams};
pub use random_walk::{random_walk, Walks};
pub use triangle::{per_node_triangles, triangle_count, undirected_view, UndirectedView};

// ---------------------------------------------------------------------------
// Frontier kernels (BFS / k-hop / unweighted shortest path).
// ---------------------------------------------------------------------------
// `k_hop` (hop.rs) backs `ur.hop`; `bfs_distances` / `shortest_path` (bfs.rs)
// back `ur.shortest_path` and the BFS-derived stats (diameter, avg_path_length).
// Still to come: the direction-optimizing top-down/bottom-up BFS switch (Beamer
// et al.) from the GAP suite, and weighted SSSP (delta-stepping).
