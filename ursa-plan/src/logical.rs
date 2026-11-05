//! Shared plan-layer types.
//!
//! The custom logical plan *nodes* themselves (with their full
//! `UserDefinedLogicalNodeCore` implementations) live in [`crate::node`]. What a
//! graph column computes is a [`Call`] from the catalog in `ursa-core`
//! ([`ursa_core::catalog`]), which holds every graph function's parameters,
//! defaults and outputs, and the dispatch that runs them; this crate turns the
//! Python API's arguments into calls ([`crate::query`]) and words their errors.
//!
//! The **v0.1 planner ambition** (decided): the nodes execute with *naive*
//! placement — in written order, correctness over cleverness — with the optimizer
//! rules landing incrementally in v0.1.x/v0.2. The public API is identical either
//! way.

pub use ursa_core::catalog::Call;
pub use ursa_core::Direction;
