//! The graph-result model (design §3 and §6, #424): what the engine produces
//! and every front end draws. No GPU, no browser, no SQL and no Arrow.
//!
//! - [`GraphResult`]: nodes, edges, their columns, highlights, encodings and
//!   provenance.
//! - [`GraphDelta`]: a change to a result, which carries its own inverse.
//!   A scene is a base result and the deltas applied to it; undo applies
//!   [`GraphDelta::invert`], with no query.
//! - [`ugr`]: the wire format, `to_bytes` and `from_bytes` on both.
//! - [`Selection`]: the nodes and edges a user picked. A front end's, not
//!   part of a result.
//!
//! # Invariants
//!
//! [`GraphResult::apply`] keeps these, and [`GraphResult::validate`] checks
//! them, as the wire format's decoder does.
//!
//! - **A node's identity is `(label, key)`**, and no two nodes share one. A
//!   label is a vertex table; its keys are all integers or all text, and two
//!   labels may use the same key.
//! - **An edge's identity** is `(label, row)` where the engine knows the
//!   edge's row in its edge table, else its endpoints and its place among
//!   their parallel edges. The model stores it and does not require it to be
//!   unique.
//! - **Indexes are append-only.** A node or an edge is added in a new slot
//!   at the end, and a slot's index never changes. *Removing* one sets a
//!   flag: it keeps its slot and its data, so everything that names it by
//!   index stays valid, and restoring it is the flag again. An edge is shown
//!   when neither it nor either end is removed. Slots only go away from the
//!   end, when the delta that added them is undone.
//! - Edges, seeds and paths name slots that exist. A path with edges has one
//!   between each two of its nodes, joining those two.
//! - A column has one value per slot. The distance of a highlight and each
//!   encoding name a node column that exists (a `u32` one for distance, a
//!   numeric one for size).
//!
//! # Deltas and undo
//!
//! Every [`Op`] carries what its inverse needs (`SetColumn` has the column it
//! replaces, `PopNodes` the nodes it takes off), and every op is strict: it
//! applies only to the state it was made for. So `invert` reads no result,
//! and `apply(delta)` followed by `apply(&delta.invert())` restores the
//! result exactly, bit for bit, including its encoding as bytes.
//!
//! ```
//! use ursa_viz::model::*;
//!
//! // Two labels that share the key 1, and an edge between them.
//! let base = GraphDelta::new(vec![
//!     Op::AddNodeLabel(NodeLabel::new("people", KeyType::Int)),
//!     Op::AddNodeLabel(NodeLabel::new("places", KeyType::Int)),
//!     Op::AddEdgeLabel("lives_in".into()),
//!     Op::AddNodes(NodeBatch::of_label(0, [1i64, 2])),
//!     Op::AddNodes(NodeBatch::of_label(1, [1i64])),
//!     Op::AddEdges(EdgeBatch::of_label(0, vec![0, 1], vec![2, 2])),
//! ]);
//! let mut result = GraphResult::build(Some("town".into()), Provenance::default(), &base)?;
//! assert_eq!(result.node(1, &Key::Int(1)), Some(2));
//!
//! // Over the wire and back.
//! let bytes = result.to_bytes();
//! assert_eq!(GraphResult::from_bytes(&bytes)?, result);
//!
//! // A step, and its undo.
//! let step = GraphDelta::new(vec![Op::RemoveNodes(vec![0])]);
//! result.apply(&step)?;
//! assert!(!result.edge_is_shown(0));
//! result.apply(&step.invert())?;
//! assert_eq!(result.to_bytes(), bytes);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod delta;
mod result;
mod select;
pub mod ugr;

pub use delta::{BatchColumn, BatchValues, EdgeBatch, GraphDelta, NodeBatch, Op};
pub use result::{
    Column, Encodings, GraphResult, Highlights, Key, KeyType, Layout, ModelError, NodeLabel, Path,
    Positions, Provenance, Role, Strings, Target, Values, NO_ROW, NULL, REMOVED,
};
pub use select::Selection;
pub use ugr::UgrError;

#[cfg(test)]
mod tests;
