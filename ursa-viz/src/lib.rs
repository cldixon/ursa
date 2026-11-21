//! Ursa's graph view (#280): a graph drawn as its ForceAtlas2 layout
//! converges, with pan, zoom and picking.
//!
//! The crate is a library first. Its input is a graph as a node count and
//! dense `src`/`dst` node indexes, plus optional per-node values for size and
//! colour, so it knows nothing of SQL, DataFusion or Python: the browser shell
//! maps a result's ids to dense indexes, and the DataFrame API already has them
//! in its topology. Both can drive the same view.
//!
//! - [`model`]: the graph-result model, deltas and the UGR wire format: what
//!   the engine produces and every front end reads.
//! - [`live`]: a result as the view draws it, kept in step with its deltas:
//!   which slots a delta changes, in time proportional to the delta. No GPU.
//! - [`interact`]: the selection set and the hovered node: what a click
//!   selects. No GPU.
//! - [`scene`]: the graph, the live layout (`ursa-core`'s [`Fa2State`],
//!   stepped a few iterations per frame, and grown when the graph gains
//!   nodes), node styles and the camera. No GPU.
//! - `render` (feature `render`, on by default): drawing with wgpu: WebGPU
//!   or WebGL2 in the browser, Vulkan, Metal or DX12 natively. Edges are a
//!   line list over the node positions, so a layout step uploads 8 bytes a
//!   node and never the edges.
//! - `view` (feature `render`): a scene and its renderer, for a platform
//!   binding to drive.
//! - `web` (feature `web`): the wasm-bindgen API for an HTML canvas.
//!
//! With `default-features = false` the crate is [`model`], [`scene`],
//! [`highlight`], [`interact`] and [`live`] alone: no GPU and no browser, which is how the engine depends on it.
//!
//! [`Fa2State`]: ursa_core::algo::Fa2State

pub mod highlight;
pub mod interact;
pub mod live;
pub mod model;
#[cfg(feature = "render")]
pub mod render;
pub mod scene;
#[cfg(feature = "render")]
pub mod view;
#[cfg(all(feature = "web", target_arch = "wasm32"))]
pub mod web;

pub use highlight::HighlightState;
pub use interact::{Interact, Selection};
pub use live::{Changes, LiveResult};
pub use model::{GraphDelta, GraphResult};
pub use scene::{BadGrowth, Camera, Graph, Growth, LiveLayout, NodeStyle};
#[cfg(feature = "render")]
pub use view::View;
