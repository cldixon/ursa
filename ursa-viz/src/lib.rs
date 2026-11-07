//! Ursa's graph view (#280): a graph drawn as its ForceAtlas2 layout
//! converges, with pan, zoom and picking.
//!
//! The crate is a library first. Its input is a graph as a node count and
//! dense `src`/`dst` node indexes, plus optional per-node values for size and
//! colour, so it knows nothing of SQL, DataFusion or Python: the browser shell
//! maps a result's ids to dense indexes, and the DataFrame API already has them
//! in its topology. Both can drive the same view.
//!
//! - [`scene`]: the graph, the live layout (`ursa-core`'s [`Fa2State`],
//!   stepped a few iterations per frame), node styles and the camera. No GPU.
//! - [`render`]: drawing with wgpu: WebGPU or WebGL2 in the browser, Vulkan,
//!   Metal or DX12 natively. Edges are a line list over the node positions,
//!   so a layout step uploads 8 bytes a node and never the edges.
//! - [`view`]: a scene and its renderer, for a platform binding to drive.
//! - `web` (feature `web`): the wasm-bindgen API for an HTML canvas.
//!
//! [`Fa2State`]: ursa_core::algo::Fa2State

pub mod render;
pub mod scene;
pub mod view;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
pub mod web;

pub use scene::{Camera, Graph, LiveLayout, NodeStyle};
pub use view::View;
