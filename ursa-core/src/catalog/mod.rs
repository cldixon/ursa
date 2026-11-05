//! The graph-function catalog: one description and one dispatch for every
//! node-valued algorithm, layout, neighbour aggregation and whole-graph
//! statistic, shared by every front end (the Python DataFrame library through
//! `ursa-plan`, and the SQL engine).
//!
//! Three parts, used in this order:
//!
//! 1. **Specs** ([`FunctionSpec`], [`FUNCTIONS`], [`lookup`]): each function's
//!    name, parameters (type, default, accepted values), outputs (names and
//!    Arrow types), the NetworkX call it is pinned to, and a one-line summary.
//!    A front end lists them, generates its signatures from them and checks its
//!    own against them.
//! 2. **Resolution** ([`FunctionSpec::resolve`]): a call's arguments, as
//!    front-end-neutral [`Arg`]s, become a validated [`Call`] plus its output
//!    [`OutputDtype`]. A bad argument is a structured [`ParamError`]; the
//!    wording, quoting and spelling of the message are the front end's.
//! 3. **Dispatch** ([`Evaluator`]): a [`Call`] over a [`Topology`], with
//!    optional per-edge weights, an optional per-node attribute and an optional
//!    [`EdgeMask`], becomes its output columns as Arrow arrays. One evaluator
//!    shares work between the calls made through it: a repeated call, the
//!    triangle pass behind `triangle_count` and `clustering_coefficient`, the
//!    symmetric views behind the `direction='both'` forms, and the two columns
//!    of a layout.
//!
//! Weights and attributes are *expressions* in every front end, so the catalog
//! never evaluates them: [`ParamType::Weight`] and [`ParamType::Attribute`]
//! parameters are listed in the specs but given to the [`Evaluator`] as
//! [`Inputs`], already evaluated, rather than to `resolve`.
//!
//! ```
//! use arrow::array::Array;
//! use ursa_core::catalog::{self, Arg, Evaluator, Inputs};
//! use ursa_core::Topology;
//!
//! let topo = Topology::build(3, vec![0, 1, 2], vec![1, 2, 0]);
//! let spec = catalog::lookup("pagerank").unwrap();
//! let resolved = spec
//!     .resolve([("direction", Arg::Str("both".into()))])
//!     .unwrap();
//! let mut eval = Evaluator::new(&topo, None);
//! let columns = eval.evaluate(&resolved.call, Inputs::default()).unwrap();
//! assert_eq!(columns.len(), 1);
//! assert_eq!(columns[0].len(), 3);
//! ```

mod call;
mod eval;
mod spec;
pub mod stats;
#[cfg(test)]
mod tests;

pub use call::{Arg, Call, Expected, ParamError, ParamErrorKind, Resolved};
pub use eval::{DispatchError, Evaluator, Inputs, OutputDtype};
pub use spec::{
    direction_name, lookup, names, parse_direction, ColumnType, Family, FunctionSpec, OutputSpec,
    ParamSpec, ParamType, Shape, Value, ALL_DIRECTIONS, FUNCTIONS, OUT_OR_BOTH,
};
