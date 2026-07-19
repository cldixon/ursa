//! The dialect seam: lowering Ursa's Polars-shaped expressions to DataFusion.
//!
//! This is the module that contains the cost of the DataFusion decision. Ursa's
//! public expression dialect (`ur.col`, `ur.lit`, arithmetic, comparisons,
//! `.str`/`.dt`/`.list`, aggregations, and the graph verbs) is *not* DataFusion's
//! expression surface. Everything that translates one into the other lives here
//! and nowhere else, so the "we own a Polars-shaped frontend" tax is a single
//! bounded surface we can reason about, test, and evolve.
//!
//! `pl.Expr` objects are **not** accepted and no Polars-expression translator is
//! built or maintained (spec §The expression dialect / §Architecture). Interop is
//! Arrow, and Arrow interop is zero-copy regardless of engine.
//!
//! ## Shape
//!
//! The Python layer builds an [`UrsaExpr`] tree (mirrored across the FFI from
//! `ursa-py`); [`lower`] walks it to a `datafusion::logical_expr::Expr`, emitting
//! the graph verbs as the custom logical nodes in [`crate::logical`] instead.

use datafusion::logical_expr::Expr as DfExpr;

/// A node in Ursa's expression dialect. This is the stable, engine-independent
/// representation the Python bindings construct; `ursa-plan` owns its translation.
#[derive(Debug, Clone)]
pub enum UrsaExpr {
    /// `ur.col("name")`
    Column(String),
    /// `ur.lit(value)` — scalar literal (variant set widened during impl).
    LitI64(i64),
    LitF64(f64),
    LitStr(String),
    /// role references: `ur.src()`, `ur.dst()`, `ur.id()`
    Src,
    Dst,
    Id,
    /// binary ops (arithmetic / comparison / boolean) — op kept as a string in
    /// the skeleton; becomes an enum during implementation.
    Binary {
        op: String,
        left: Box<UrsaExpr>,
        right: Box<UrsaExpr>,
    },
    // graph verbs (`ur.degree`, `ur.pagerank`, `ur.neighbors(..).agg(..)`, ...)
    // lower to custom logical nodes rather than DfExpr — added during impl.
}

/// Lower an [`UrsaExpr`] to a DataFusion expression. Skeleton: covers the trivial
/// leaf cases so the seam compiles and is exercisable; the rest is the first
/// implementation task for this module.
pub fn lower(expr: &UrsaExpr) -> DfExpr {
    use datafusion::logical_expr::{col, lit};
    match expr {
        UrsaExpr::Column(name) => col(name),
        UrsaExpr::LitI64(v) => lit(*v),
        UrsaExpr::LitF64(v) => lit(*v),
        UrsaExpr::LitStr(v) => lit(v.clone()),
        // TODO(v0.1): Src/Dst/Id resolve against the frame's role mapping;
        // Binary maps to df binary_expr; graph verbs -> custom logical nodes.
        other => todo!("lower {other:?} — see module docs"),
    }
}
