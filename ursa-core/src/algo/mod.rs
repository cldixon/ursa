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
pub(crate) mod frontier;
mod hop;
mod induced;
mod label_prop;
mod layout;
mod levels;
mod linked;
mod louvain;
mod multilevel;
mod neighbor_agg;
mod pagerank;
/// Public because it is independently useful — a deterministic spatial index over
/// 2-D positions — and because `examples/layout_scaling.rs` times the build
/// separately from the traversal, which is how we know which half to optimize.
pub mod quadtree;
mod random_walk;
mod rng;
mod subrounds;
pub use rng::sample_indices;
mod triangle;
mod undirected;
mod warm;

pub use betweenness::{betweenness, betweenness_weighted};
pub use bfs::{
    bfs_distances, dijkstra_distances, shortest_path, shortest_path_masked, shortest_path_weighted,
    shortest_path_weighted_with_cost, shortest_path_weighted_with_cost_masked,
};
pub use closeness::{
    closeness, closeness_directed, closeness_weighted, closeness_weighted_directed,
};
pub use clustering::{clustering_coefficient, clustering_from_triangles};
pub use components::{connected_components_strong, connected_components_weak};
pub use degree::degree;
pub use hop::{
    k_hop, k_hop_distances_masked, k_hop_masked, k_hop_reached_masked, k_hop_reached_set,
};
pub use induced::{induced_edges, InducedEdges};
pub use label_prop::label_propagation;
pub use layout::{
    fa2_seed_positions, layout_circle, layout_fa2, layout_fr, layout_random, Fa2Params, Fa2State,
    LayoutParams, Positions,
};
pub use levels::{layout_layers, layout_rings, LayersParams, RingsParams, NO_LEVEL};
pub use louvain::{louvain, louvain_weighted};
pub use multilevel::{layout_fa2_multilevel, Fa2Multilevel, MultilevelParams};
pub use neighbor_agg::{neighbor_aggregate, AggKind};
pub use pagerank::{pagerank, pagerank_weighted, PageRankParams};
pub use random_walk::{random_walk, random_walk_masked, Walks};
pub use triangle::{per_node_triangles, triangle_count, undirected_view, UndirectedView};
pub use undirected::{symmetric_view, symmetric_view_weighted};
pub use warm::{fa2_grow, place_new_nodes, warm_iterations, WarmParams};

// ---------------------------------------------------------------------------
// Frontier kernels (BFS / k-hop / unweighted shortest path).
// ---------------------------------------------------------------------------
// `k_hop` (hop.rs) backs `ur.hop`; `bfs_distances` / `shortest_path` (bfs.rs)
// back `ur.shortest_path` and the BFS-derived stats (diameter, avg_path_length).
// Still to come: the direction-optimizing top-down/bottom-up BFS switch (Beamer
// et al.) from the GAP suite, and weighted SSSP (delta-stepping).
