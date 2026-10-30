//! The plan IR and its one entry point, [`execute_plan`].
//!
//! A [`Plan`] is a whole query as a tree: one variant per frame step, each holding
//! its input as a child plan, so a `join` has two children and a graph verb holds
//! the edge plan it runs over. A frame's tree of [`crate::lazy`] nodes is lowered
//! to this IR for each run (it also parses from JSON, `deny_unknown_fields` on
//! every variant, which the conformance fixtures use) and handed to
//! [`execute_plan`]. The same walk checks a frame's shape as it is built
//! ([`check_shape`]) and computes its schema over empty inputs ([`dry_run`]).
//!
//! ## What runs where (#189)
//!
//! [`execute_plan`] owns the *dispatch* (#190): it walks the tree, rejects the
//! shapes that aren't supported (the [`PlanError::NotImplemented`] guards, with
//! the messages the Python layer used to raise), and builds each frame's leaf
//! from [`crate::query`]: a source's rows, a graph or traversal node.
//!
//! Every relational step is then lowered onto that leaf, in the order written,
//! as stock DataFusion operators ([`crate::lower`], #191): `filter`, `select`,
//! `with_columns`, `rename`, `sort`, `head`, `distinct`, `sample` and
//! `group_by().agg()`, with the order guarantees implemented once there. A step
//! means the same wherever it runs; its semantics and errors are the ones the
//! plain frame always had. A `join` builds its right side the same way, as a
//! frame of its own, and joins the two in one plan ([`crate::lower::join`]).
//!
//! Since #192 it also owns the leaf work around them, with one expression
//! evaluator ([`crate::expr`] lowered to DataFusion, [`crate::eval`] outside a
//! plan):
//!
//! - plain sources: a scan reads the columns the steps need and takes its
//!   leading filters into the read (Parquet row-group pruning); every `filter`
//!   step runs here, wherever it sits;
//! - subgraph-view masks, built in dense id space and cached on the
//!   [`GraphIndex`] ([`MaskCache`]);
//! - the edge columns a weight reads, and the index a weighted scan builds;
//! - a scanned node table, through the frame's [`NodeTableCache`].
//!
//! What belongs to the frames is reached through [`Inputs`] by slot
//! ([`crate::lazy::TreeInputs`] reads it off the frame tree): each frame's
//! source (its in-memory table, or a scan's storage options), its cached index
//! and node-table cache, seed id sets, and the rows a `sample` keeps (drawn with
//! the host's seeded generator).
//!
//! Inputs are resolved lazily, in the order the executor needs them, so a plan
//! that fails validation never builds an index or reads a file.
//!
//! ## The edge input of a graph node
//!
//! Every graph node (`with_columns` of a verb, `hop`, `shortest_path`,
//! `random_walk`, `describe`) names its edges as an [`EdgeInput`]: an edge
//! subplan plus the host frame it came from. [`EdgeInput::view`] splits it into
//! the interface #191 and #192 build on, an [`EdgeView`]:
//!
//! - `topology`: the subplan whose rows define the CSR: a source (under any
//!   `reverse`), whose index the host frame keeps, or the topmost step that
//!   reshapes the edge rows (`distinct`, `sample`, `join`, `group_by().agg()`,
//!   `sort`, `head`; #191), whose index is built from that subplan's output, as
//!   a fresh frame over those rows would build it, and cached by a fingerprint
//!   of the subplan ([`crate::reshape`]).
//! - the steps above the topology that keep its rows: `filter`, and the column
//!   steps `select`, `rename` and `with_columns`, which a weight or a later
//!   filter reads through. A step that leaves no `src` / `dst` column is an
//!   error naming it.
//! - a mask, derived in dense id space over the topology's edge rows: the
//!   `filter` predicates written above the topology (a subgraph view, #114),
//!   ANDed, and for a traversal result the subgraph its reached node set induces
//!   (#116). The view is recursive: a traversal's input is a view of its own, so
//!   the traversal follows only that view's edges and the induced subgraph stays
//!   within it, and each filter is applied exactly once.

use std::any::Any;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use arrow::array::{Array, ArrayRef, RecordBatch};
use datafusion::error::DataFusionError;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use ursa_core::{EdgeMask, IdMap, Topology};

use arrow::datatypes::DataType;
use datafusion::logical_expr::Expr as DfExpr;

use crate::expr::GraphVerb;
use crate::lower::{col_named, Frame, How};
use crate::query::{
    attach_columns, graph_leaf, hop_leaf, node_columns, parse_direction, path_leaf, resolve_dense,
    walk_leaf, NodeColumn,
};
use crate::scan::{scan_edges_batch, scan_edges_rows, NodeTableCache};
use crate::stats::{describe_masked, parse_stat_direction};
use crate::weight::{edge_weights, WeightCache};

/// A numbered input the host resolves: a frame (its table, scan, index, mask) or
/// a seed set.
pub type Slot = usize;

/// A query plan: one node per frame step, each holding its input.
///
/// Relational nodes carry `step`, the host's handle for the frame step they came
/// from: a `sample` draws its rows through it ([`Inputs::sample`]). A step whose
/// arguments the host could not parse is sent with `invalid` set and its
/// arguments left out: the executor raises the host's error
/// ([`Inputs::step_error`]) where it would have read the step, so a guard that
/// fires earlier still wins.
///
/// Integer arguments are [`IntArg`]s, typed only where the engine reads them.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Plan {
    /// `ur.scan_edges(path, src=, dst=)`.
    ScanEdges {
        frame: Slot,
        /// The path(s), when representable (anything else is not a single path).
        /// Storage options stay with the host ([`Inputs::storage_options`]).
        #[serde(default)]
        path: Option<ScanPath>,
        src: String,
        dst: String,
        on_null: String,
    },
    /// `ur.scan_nodes(path, id=)`.
    ScanNodes {
        frame: Slot,
        #[serde(default)]
        path: Option<ScanPath>,
        id: String,
    },
    /// An in-memory Arrow table: an edge frame (`src`/`dst`) or a node frame (`id`).
    FromArrow {
        frame: Slot,
        #[serde(default)]
        src: Option<String>,
        #[serde(default)]
        dst: Option<String>,
        #[serde(default)]
        id: Option<String>,
    },
    Filter {
        input: Box<Plan>,
        #[serde(default)]
        predicate: Expr,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    Select {
        input: Box<Plan>,
        #[serde(default)]
        columns: Vec<SelectItem>,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    WithColumns {
        input: Box<Plan>,
        columns: Vec<NamedExpr>,
    },
    Rename {
        input: Box<Plan>,
        /// `(old, new)` pairs, in the order given.
        #[serde(default)]
        mapping: Vec<(String, String)>,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    Sort {
        input: Box<Plan>,
        #[serde(default)]
        by: Vec<SortKey>,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    Head {
        input: Box<Plan>,
        #[serde(default)]
        n: IntArg,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    Sample {
        input: Box<Plan>,
        #[serde(default)]
        n: IntArg,
        #[serde(default)]
        seed: Option<IntArg>,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    Distinct {
        input: Box<Plan>,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    Join {
        input: Box<Plan>,
        right: Box<Plan>,
        #[serde(default)]
        on: Vec<String>,
        how: String,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    GroupByAgg {
        input: Box<Plan>,
        #[serde(default)]
        keys: Vec<String>,
        #[serde(default)]
        aggs: Vec<AggSpec>,
        #[serde(default)]
        step: Option<Slot>,
        #[serde(default)]
        invalid: bool,
    },
    /// `edges.nodes()`: the node set of an edge plan.
    Nodes { input: Box<Plan> },
    /// `edges.reverse()`: swap the src/dst roles.
    Reverse { input: Box<Plan> },
    /// `ur.hop(edges, n, direction).from_(seeds)`.
    Hop {
        edges: EdgeInput,
        n: IntArg,
        direction: String,
        seeds: Slot,
    },
    /// `ur.shortest_path(edges, source, target, weight=, direction=)`. `source`
    /// and `target` are seed slots holding one id each.
    ShortestPath {
        edges: EdgeInput,
        source: Slot,
        target: Slot,
        #[serde(default)]
        weight: Option<Value>,
        direction: String,
    },
    /// `ur.random_walk(edges, start, steps, walks_per_node, seed)`.
    RandomWalk {
        edges: EdgeInput,
        start: Slot,
        steps: IntArg,
        walks_per_node: IntArg,
        #[serde(default)]
        seed: Option<IntArg>,
    },
    /// `ur.describe(edges, full, direction)`.
    Describe {
        edges: EdgeInput,
        full: bool,
        direction: String,
    },
    /// A frame the host could not serialize (a join's right side). Executing it
    /// raises the host's error for `step`, at the point the frame would have run.
    Invalid { step: Slot },
}

/// A scan path: one path (a glob included) or a list of them.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum ScanPath {
    One(String),
    Many(Vec<String>),
}

/// An integer argument, as given. The engine converts it where it reads it, with
/// the errors a Python integer conversion gives (`OverflowError`, `TypeError`).
/// A host value with no JSON form is sent as `{"type": <type name>}`.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct IntArg(pub Value);

impl IntArg {
    /// The value as an integer, or the `TypeError` for a non-integer.
    fn int(&self, name: &str) -> PlanResult<Option<i128>> {
        let ty = match &self.0 {
            Value::Bool(b) => return Ok(Some(*b as i128)),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    return Ok(Some(i as i128));
                }
                if let Some(u) = n.as_u64() {
                    return Ok(Some(u as i128));
                }
                // An integer past 64 bits arrives as an integral float.
                let f = n.as_f64().unwrap_or(0.0);
                if f.fract() == 0.0 && f.abs() >= 9.2e18 {
                    return Ok(None);
                }
                "float"
            }
            Value::String(_) => "str",
            Value::Null => "NoneType",
            Value::Array(_) => "list",
            Value::Object(o) => o.get("type").and_then(Value::as_str).unwrap_or("object"),
        };
        Err(PlanError::Type(format!(
            "argument '{name}': '{ty}' object cannot be interpreted as an integer"
        )))
    }

    /// As a `u64`/`usize` argument.
    pub fn u64(&self, name: &str) -> PlanResult<u64> {
        match self.int(name)? {
            Some(i) if i < 0 => Err(PlanError::Overflow(
                "can't convert negative int to unsigned".into(),
            )),
            Some(i) if i <= u64::MAX as i128 => Ok(i as u64),
            _ => Err(PlanError::Overflow("int too big to convert".into())),
        }
    }

    pub fn usize(&self, name: &str) -> PlanResult<usize> {
        self.u64(name).map(|v| v as usize)
    }

    /// As a `u32` argument.
    pub fn u32(&self, name: &str) -> PlanResult<u32> {
        match self.int(name)? {
            Some(i) if (0..=u32::MAX as i128).contains(&i) => Ok(i as u32),
            _ => Err(PlanError::Overflow(
                "out of range integral type conversion attempted".into(),
            )),
        }
    }
}

/// An optional seed argument.
fn seed_arg(seed: &Option<IntArg>, name: &str) -> PlanResult<Option<u64>> {
    match seed {
        Some(IntArg(Value::Null)) | None => Ok(None),
        Some(s) => s.u64(name).map(Some),
    }
}

/// One `with_columns` output.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamedExpr {
    pub name: String,
    pub expr: Expr,
}

/// One `select` item: a column name, or an expression (its JSON).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum SelectItem {
    Name(String),
    Expr(Value),
}

/// One sort key: a column, or an expression (its JSON).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SortKey {
    #[serde(default)]
    pub column: Option<String>,
    #[serde(default)]
    pub expr: Option<Value>,
    #[serde(default)]
    pub descending: bool,
}

/// One `group_by().agg()` aggregation: `fn` over `column`, named `output`.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct AggSpec {
    pub output: String,
    #[serde(rename = "fn")]
    pub func: String,
    pub column: String,
}

/// An expression in the grammar of [`crate::expr`]: a graph verb, typed, or any
/// other node, kept as its JSON and parsed where it is used (a filter predicate
/// after its role references resolve, a weight by the weight evaluator).
///
/// A graph node with no `verb` (the stub a graph verb serializes to where it
/// isn't a column, e.g. used as a filter predicate) stays a scalar, so the
/// context it is used in rejects it with its own error.
#[derive(Debug, Clone)]
pub enum Expr {
    Graph(Box<GraphVerb>),
    Scalar(Value),
}

impl Default for Expr {
    fn default() -> Self {
        Expr::Scalar(Value::Null)
    }
}

impl<'de> Deserialize<'de> for Expr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        Expr::from_value(v).map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for Expr {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Expr::Scalar(v) => v.serialize(s),
            Expr::Graph(verb) => {
                let mut v = serde_json::to_value(verb).map_err(serde::ser::Error::custom)?;
                if let Value::Object(m) = &mut v {
                    m.insert("kind".into(), Value::from("graph"));
                }
                v.serialize(s)
            }
        }
    }
}

impl Plan {
    /// The plan as JSON (what [`Plan::from_json`] parses).
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a plan serializes")
    }
}

impl Expr {
    /// An expression from its JSON. Only the verb-less graph stub stays scalar:
    /// a malformed graph verb (an unknown parameter, say) is an error.
    pub fn from_value(v: Value) -> datafusion::error::Result<Expr> {
        if kind(&v) == "graph" && v.get("verb").is_some() {
            return GraphVerb::from_value(&v).map(|g| Expr::Graph(Box::new(g)));
        }
        Ok(Expr::Scalar(v))
    }
}

/// The `kind` tag of a JSON expression node (`""` when absent).
pub(crate) fn kind(v: &Value) -> &str {
    v.get("kind").and_then(Value::as_str).unwrap_or("")
}

/// The edges a graph node runs over: the edge plan as written, and the host frame
/// it came from (whose cached index and mask the host holds). See the module docs
/// and [`EdgeInput::view`].
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeInput {
    pub plan: Box<Plan>,
    pub frame: Slot,
}

impl EdgeInput {
    /// Parse an edge input from its JSON (as [`Plan::from_json`]).
    pub fn from_json(json: &str) -> PlanResult<EdgeInput> {
        let mut de = serde_json::Deserializer::from_str(json);
        de.disable_recursion_limit();
        let edges =
            EdgeInput::deserialize(&mut de).map_err(|e| PlanError::Invalid(e.to_string()))?;
        de.end().map_err(|e| PlanError::Invalid(e.to_string()))?;
        Ok(edges)
    }
}

/// An [`EdgeInput`] split into the topology it runs over and the mask that
/// restricts it. See the module docs.
///
/// The view is recursive: a traversal result's mask is the subgraph its reached
/// node set induces *within the view the traversal ran over*, so
/// `ur.hop(e.filter(a), 2)` follows only the edges `a` keeps, and a graph op over
/// that result keeps them too (#192). Each view takes only the filters written
/// above its topology; the filters of a traversal's input belong to that input's
/// view and are applied once, through it. When the topology is a reshaped subplan
/// (`filter(a).distinct().filter(b)`), `a` stays with the topology's own rows and
/// the mask takes `b` alone.
#[derive(Debug)]
pub struct EdgeView<'a> {
    /// The host slot of the frame whose rows and index the view masks: the frame
    /// itself, or a traversal result's input frame.
    pub frame: Slot,
    /// Whether that frame has rows of its own (a source under property-only
    /// steps), not a traversal result's synthetic pairs.
    pub sourced: bool,
    /// The plan whose rows define the CSR: a source, under any `reverse`, or the
    /// topmost step that reshapes the edge rows ([`RESHAPING_OPS`]).
    pub topology: &'a Plan,
    /// Whether `topology` is a reshaped subplan: its index is built from its rows
    /// and cached by a fingerprint of it ([`crate::reshape`]), not the frame's.
    pub reshaped: bool,
    /// The `src` / `dst` columns of the topology's rows, as the frame names them.
    pub roles: (String, String),
    /// The steps written above the topology that keep its rows: `filter` (the
    /// mask) and the column steps `select`, `rename` and `with_columns`, in order.
    /// Each filter reads the columns as the column steps before it leave them;
    /// a weight reads them as all of them leave them.
    pub steps: Vec<&'a Plan>,
    /// `filter` predicates over the topology's edge rows, ANDed (#114).
    pub predicates: Vec<&'a Expr>,
    /// A `hop` / `shortest_path` whose reached node set induces the subgraph
    /// (#116), and the view it ran over.
    pub induced_by: Option<Induced<'a>>,
}

/// The traversal behind a traversal-result view.
#[derive(Debug)]
pub struct Induced<'a> {
    /// The `hop` / `shortest_path` node.
    pub traversal: &'a Plan,
    /// The edges it ran over.
    pub edges: &'a EdgeInput,
    /// Their view: its mask restricts the traversal and the induced subgraph.
    pub input: Box<EdgeView<'a>>,
}

impl EdgeView<'_> {
    /// Whether the view restricts its topology at all.
    pub fn has_mask(&self) -> bool {
        !self.predicates.is_empty() || self.induced_by.is_some()
    }

    /// Whether a column step (`select`, `rename`, `with_columns`) is written
    /// above the topology, so its rows' columns differ from the topology's.
    fn has_column_steps(&self) -> bool {
        self.steps.iter().any(|s| !matches!(s, Plan::Filter { .. }))
    }
}

/// The steps that reshape an edge plan's rows: a graph op over one runs over the
/// topology of its output rows (#191).
pub(crate) const RESHAPING_OPS: [&str; 6] =
    ["distinct", "sample", "join", "group_by_agg", "sort", "head"];

/// The steps that keep an edge plan's rows but change its columns.
pub(crate) const COLUMN_OPS: [&str; 3] = ["select", "rename", "with_columns"];

/// The message for a `with_columns` of a graph verb on an edge frame.
pub(crate) const EDGE_WITH_COLUMNS: &str =
    "collect() does not yet support the 'with_columns' step on an \
     edge frame.";

impl EdgeInput {
    /// Split the edge plan into its [`EdgeView`], rejecting the edge plans a graph
    /// op can't run over: a relational step after a traversal, a `with_columns`
    /// of a graph verb, or a step that leaves no `src` / `dst` column.
    pub fn view(&self) -> PlanResult<EdgeView<'_>> {
        let chain = self.plan.chain();
        let ops: BTreeSet<&str> = chain.iter().map(|p| p.op()).collect();
        let traversal = chain
            .iter()
            .copied()
            .find(|p| matches!(p, Plan::Hop { .. } | Plan::ShortestPath { .. }));
        if traversal.is_some() {
            let tail: Vec<&str> = ops
                .iter()
                .copied()
                .filter(|op| !TRAVERSAL_OPS.contains(op))
                .collect();
            if !tail.is_empty() {
                return Err(PlanError::NotImplemented(format!(
                    "a graph op over a traversal result supports a bare hop/shortest_path \
                     for now; a relational step ({}) between the traversal and the graph op \
                     isn't wired yet — collect() the traversal and re-ingest via \
                     ur.from_arrow(...) to run further ops on it.",
                    tail.join(", ")
                )));
            }
        }
        if let Some(t @ (Plan::Hop { edges, .. } | Plan::ShortestPath { edges, .. })) = traversal {
            // The traversal's input is a view of its own (filters included).
            let input = edges.view()?;
            return Ok(EdgeView {
                frame: edges.frame,
                sourced: edges.traversal().is_none(),
                topology: input.topology,
                reshaped: input.reshaped,
                roles: input.roles.clone(),
                steps: vec![],
                predicates: vec![],
                induced_by: Some(Induced {
                    traversal: t,
                    edges,
                    input: Box::new(input),
                }),
            });
        }
        let edge_graph_columns = chain.iter().any(|p| match p {
            Plan::WithColumns { columns, .. } => {
                columns.iter().any(|c| matches!(c.expr, Expr::Graph(_)))
            }
            _ => false,
        });
        if edge_graph_columns {
            return not_implemented(EDGE_WITH_COLUMNS);
        }
        let roles = edge_roles(&chain);
        check_roles(&chain, &roles)?;
        // The topology: the topmost reshaping step (or step that writes an
        // endpoint column), whose output rows a graph op runs over; else the
        // nearest `reverse` or source below the steps that keep the rows. A
        // `reverse` keeps the rows (and their order), so a filter written below
        // it still filters the same rows.
        let (topology, reshaped, above) = match chain
            .iter()
            .rposition(|p| RESHAPING_OPS.contains(&p.op()) || writes_role(p, &roles))
        {
            Some(i) => (chain[i], true, &chain[i + 1..]),
            None => {
                let topology = chain
                    .iter()
                    .rev()
                    .copied()
                    .find(|p| p.input().is_none() || matches!(p, Plan::Reverse { .. }))
                    .unwrap_or(self.plan.as_ref());
                (topology, false, &chain[..])
            }
        };
        let steps: Vec<&Plan> = above
            .iter()
            .copied()
            .filter(|p| p.op() == "filter" || COLUMN_OPS.contains(&p.op()))
            .collect();
        let predicates = steps
            .iter()
            .filter_map(|p| match p {
                Plan::Filter { predicate, .. } => Some(predicate),
                _ => None,
            })
            .collect();
        Ok(EdgeView {
            frame: self.frame,
            sourced: true,
            topology,
            reshaped,
            roles,
            steps,
            predicates,
            induced_by: None,
        })
    }

    /// The traversal this edge plan is the result of, if any.
    fn traversal(&self) -> Option<&Plan> {
        self.plan
            .chain()
            .into_iter()
            .find(|p| matches!(p, Plan::Hop { .. } | Plan::ShortestPath { .. }))
    }
}

/// The output name of a `select` expression, its role references resolved (as
/// `select` names it).
fn select_name(v: &Value, roles: &Roles) -> String {
    expr_name(&resolve_roles(v, roles).unwrap_or_else(|_| v.clone()))
}

/// Whether a column step writes a column named as an endpoint (`src` / `dst`)
/// with other values: a rename onto it, or a `select` / `with_columns`
/// expression computing it. Its rows then hold another graph than the source's,
/// so the step is the topology, as a reshaping step is (a fresh frame over its
/// rows reads the endpoints by name).
pub(crate) fn writes_role(step: &Plan, roles: &(String, String)) -> bool {
    let role_refs = [("src", roles.0.clone()), ("dst", roles.1.clone())];
    let is_role = |n: &str| n == roles.0 || n == roles.1;
    // An expression that is the column `name` itself, by name or by role.
    let itself = |v: &Value, name: &str| {
        resolve_roles(v, &role_refs)
            .is_ok_and(|r| kind(&r) == "col" && r.get("name").and_then(Value::as_str) == Some(name))
    };
    match step {
        Plan::Rename { mapping, .. } => mapping.iter().any(|(old, new)| old != new && is_role(new)),
        Plan::Select { columns, .. } => columns.iter().any(|c| match c {
            SelectItem::Name(_) => false,
            SelectItem::Expr(v) => {
                let name = select_name(v, &role_refs);
                is_role(&name) && !itself(v, &name)
            }
        }),
        Plan::WithColumns { columns, .. } => columns
            .iter()
            .any(|c| is_role(&c.name) && !matches!(&c.expr, Expr::Scalar(v) if itself(v, &c.name))),
        _ => false,
    }
}

/// Check that an edge plan's steps leave both endpoint columns, by name: a graph
/// op over it reads them. A `rename` does not move a role (an edge frame keeps
/// its `src_col` / `dst_col`), so renaming an endpoint column away removes it.
fn check_roles(chain: &[&Plan], roles: &(String, String)) -> PlanResult<()> {
    let role_refs = [("src", roles.0.clone()), ("dst", roles.1.clone())];
    for (role, name) in [("src", &roles.0), ("dst", &roles.1)] {
        let mut removed_by: Option<&Plan> = None;
        for step in chain {
            let present = match step {
                Plan::Select { columns, .. } => Some(columns.iter().any(|c| match c {
                    SelectItem::Name(n) => n == name,
                    SelectItem::Expr(v) => &select_name(v, &role_refs) == name,
                })),
                Plan::Rename { mapping, .. } => {
                    if mapping.iter().any(|(_, new)| new == name) {
                        Some(true)
                    } else if mapping.iter().any(|(old, _)| old == name) {
                        Some(false)
                    } else {
                        None
                    }
                }
                Plan::GroupByAgg { keys, aggs, .. } => {
                    Some(keys.contains(name) || aggs.iter().any(|a| &a.output == name))
                }
                Plan::WithColumns { columns, .. } if columns.iter().any(|c| &c.name == name) => {
                    Some(true)
                }
                _ => None,
            };
            match present {
                Some(false) => removed_by = Some(step),
                Some(true) => removed_by = None,
                None => {}
            }
        }
        let Some(step) = removed_by else { continue };
        let verb = match step.op() {
            "group_by_agg" => "group_by().agg",
            op => op,
        };
        let how = if let Plan::Rename { mapping, .. } = step {
            let new = mapping
                .iter()
                .find(|(old, _)| old == name)
                .map(|(_, new)| new.as_str())
                .unwrap_or_default();
            format!(
                "renamed it to {}; a rename does not move the {role} role, so",
                py_str(new)
            )
        } else {
            "removed it, so".to_string()
        };
        return Err(PlanError::ColumnNotFound(format!(
            "a graph op needs the edge frame's {role} column {}, but {verb}() {how} \
             the edges have no {role} column. Keep the src and dst columns, or collect \
             the edges and build a new frame with ur.from_arrow(..., src=, dst=).",
            py_str(name)
        )));
    }
    Ok(())
}

pub(crate) const TRAVERSAL_OPS: [&str; 2] = ["hop", "shortest_path"];

/// The relational steps [`lower_steps`] lowers.
const TAIL_OPS: [&str; 8] = [
    "filter",
    "sort",
    "head",
    "distinct",
    "sample",
    "rename",
    "select",
    "group_by_agg",
];

impl Plan {
    /// The step name, as the frame API spells it (and as error messages quote it).
    pub fn op(&self) -> &'static str {
        match self {
            Plan::ScanEdges { .. } => "scan_edges",
            Plan::ScanNodes { .. } => "scan_nodes",
            Plan::FromArrow { .. } => "from_arrow",
            Plan::Filter { .. } => "filter",
            Plan::Select { .. } => "select",
            Plan::WithColumns { .. } => "with_columns",
            Plan::Rename { .. } => "rename",
            Plan::Sort { .. } => "sort",
            Plan::Head { .. } => "head",
            Plan::Sample { .. } => "sample",
            Plan::Distinct { .. } => "distinct",
            Plan::Join { .. } => "join",
            Plan::GroupByAgg { .. } => "group_by_agg",
            Plan::Nodes { .. } => "nodes",
            Plan::Reverse { .. } => "reverse",
            Plan::Hop { .. } => "hop",
            Plan::ShortestPath { .. } => "shortest_path",
            Plan::RandomWalk { .. } => "random_walk",
            Plan::Describe { .. } => "describe",
            Plan::Invalid { .. } => "invalid",
        }
    }

    /// The node's primary input, `None` for a leaf (a source, or a graph node
    /// whose edges are an [`EdgeInput`] rather than an input).
    pub fn input(&self) -> Option<&Plan> {
        match self {
            Plan::Filter { input, .. }
            | Plan::Select { input, .. }
            | Plan::WithColumns { input, .. }
            | Plan::Rename { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Head { input, .. }
            | Plan::Sample { input, .. }
            | Plan::Distinct { input, .. }
            | Plan::Join { input, .. }
            | Plan::GroupByAgg { input, .. }
            | Plan::Nodes { input }
            | Plan::Reverse { input } => Some(input),
            _ => None,
        }
    }

    /// This plan's steps from its leaf up to itself, following primary inputs: a
    /// frame's steps in the order they were written.
    pub fn chain(&self) -> Vec<&Plan> {
        let mut chain = vec![self];
        while let Some(input) = chain.last().and_then(|p| p.input()) {
            chain.push(input);
        }
        chain.reverse();
        chain
    }

    /// The host's handle for the step a relational node came from.
    pub fn step(&self) -> Option<Slot> {
        match self {
            Plan::Filter { step, .. }
            | Plan::Select { step, .. }
            | Plan::Rename { step, .. }
            | Plan::Sort { step, .. }
            | Plan::Head { step, .. }
            | Plan::Sample { step, .. }
            | Plan::Distinct { step, .. }
            | Plan::Join { step, .. }
            | Plan::GroupByAgg { step, .. } => *step,
            Plan::Invalid { step } => Some(*step),
            _ => None,
        }
    }

    /// Whether the host could not parse this step's arguments.
    pub fn invalid(&self) -> bool {
        match self {
            Plan::Filter { invalid, .. }
            | Plan::Select { invalid, .. }
            | Plan::Rename { invalid, .. }
            | Plan::Sort { invalid, .. }
            | Plan::Head { invalid, .. }
            | Plan::Sample { invalid, .. }
            | Plan::Distinct { invalid, .. }
            | Plan::Join { invalid, .. }
            | Plan::GroupByAgg { invalid, .. } => *invalid,
            Plan::Invalid { .. } => true,
            _ => false,
        }
    }

    /// Parse a plan from its JSON. Plans nest one level per step, so the parser's
    /// recursion limit is lifted (the serializer's own limit is lower).
    pub fn from_json(json: &str) -> PlanResult<Plan> {
        let mut de = serde_json::Deserializer::from_str(json);
        de.disable_recursion_limit();
        let plan = Plan::deserialize(&mut de).map_err(|e| PlanError::Invalid(e.to_string()))?;
        de.end().map_err(|e| PlanError::Invalid(e.to_string()))?;
        Ok(plan)
    }
}

/// A built graph index: the CSR topology, the id map, and what is derived from
/// its edge rows and cached with it: the weight vectors evaluated over them, and
/// the subgraph-view masks over them. The host keeps one per edge frame, so both
/// caches live exactly as long as the rows they were computed from.
#[derive(Clone)]
pub struct GraphIndex {
    pub topo: Arc<Topology>,
    pub ids: Arc<IdMap>,
    pub weights: Arc<WeightCache>,
    pub masks: Arc<MaskCache>,
}

impl GraphIndex {
    pub fn new(topo: Arc<Topology>, ids: Arc<IdMap>) -> Self {
        GraphIndex {
            topo,
            ids,
            weights: Arc::default(),
            masks: Arc::default(),
        }
    }
}

/// How many subgraph-view masks a [`MaskCache`] keeps (1 bit per edge each:
/// 1.25 MB at 10M edges), least recently used evicted.
const MASK_CACHE_CAPACITY: usize = 16;

/// One cached mask, filled once by whoever computes it first.
type MaskSlot = Arc<Mutex<Option<Arc<EdgeMask>>>>;

/// Subgraph-view masks memoized across queries (#181, moved here by #192), keyed
/// by a fingerprint of the view: its filters with roles resolved, and for a
/// traversal result the traversal, its resolved seeds, and the view it ran over.
/// Held on a [`GraphIndex`], so a mask is only ever served for the index it was
/// computed over.
#[derive(Default)]
pub struct MaskCache {
    /// Most recently used last. Each slot is locked while its mask is computed,
    /// so concurrent queries over one view compute it once.
    slots: Mutex<Vec<(String, MaskSlot)>>,
    builds: AtomicUsize,
}

impl MaskCache {
    /// The mask for `key`, computed by `build` on a miss.
    pub fn get_or_build(
        &self,
        key: &str,
        build: impl FnOnce() -> PlanResult<EdgeMask>,
    ) -> PlanResult<Arc<EdgeMask>> {
        let slot = {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            let slot = match slots.iter().position(|(k, _)| k == key) {
                Some(i) => slots.remove(i).1,
                None => Arc::default(),
            };
            if slots.len() >= MASK_CACHE_CAPACITY {
                slots.remove(0);
            }
            slots.push((key.to_string(), slot.clone()));
            slot
        };
        let mut mask = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(m) = mask.as_ref() {
            return Ok(m.clone());
        }
        let m = Arc::new(build()?);
        self.builds.fetch_add(1, Ordering::Relaxed);
        *mask = Some(m.clone());
        Ok(m)
    }

    /// Masks this cache has computed (test/observability hook).
    pub fn builds(&self) -> usize {
        self.builds.load(Ordering::Relaxed)
    }
}

/// Topology builds so far, wherever they happen (test/observability hook for the
/// index-preservation contract).
static TOPOLOGY_BUILDS: AtomicUsize = AtomicUsize::new(0);

/// Count one topology build.
pub fn count_topology_build() {
    // A dry run's empty indexes are not builds (see `crate::dry`).
    if crate::dry::active() {
        return;
    }
    TOPOLOGY_BUILDS.fetch_add(1, Ordering::Relaxed);
}

/// Topology builds so far.
pub fn topology_builds() -> usize {
    TOPOLOGY_BUILDS.load(Ordering::Relaxed)
}

/// Role name → column name: what `ur.src()` / `ur.dst()` / `ur.id()` resolve to.
pub type Roles = [(&'static str, String)];

/// What [`execute_plan`] reads from its host, by slot. Every method is called at
/// most when the executor needs the value, in the order the executor needs it.
///
/// The host keeps what belongs to its frames: their sources, their built
/// [`GraphIndex`] (and with it the weight and mask caches), a node scan's
/// [`NodeTableCache`], seeds, and the draw of a `sample`. Everything else
/// (scans, masks, weights, every relational step) runs here.
///
/// Batch lists are at least one batch long, so an empty result still carries its
/// schema. Rows the host produces may stay host-side ([`Rows::Host`]) until the
/// engine needs them as batches ([`Inputs::batches`]), so a result the host
/// finishes never crosses into Rust and back.
pub trait Inputs {
    /// The cached graph index of the edge frame `frame` (built on first use).
    fn index(&self, frame: Slot) -> PlanResult<GraphIndex>;

    /// The index of `frame` if it is already built (never builds one).
    fn built_index(&self, frame: Slot) -> PlanResult<Option<GraphIndex>>;

    /// Keep `index` as the index of `frame` unless one was built meanwhile, and
    /// return the frame's index.
    fn put_index(&self, frame: Slot, index: GraphIndex) -> PlanResult<GraphIndex>;

    /// The in-memory table of a frame: an edge frame's whole edge table, or a node
    /// frame's attribute table; `None` for a frame with none.
    fn table(&self, frame: Slot) -> PlanResult<Option<Rows>>;

    /// The storage options of a scan source (the host raises the error for
    /// options it can't pass, such as `store=`).
    fn storage_options(&self, frame: Slot) -> PlanResult<HashMap<String, String>>;

    /// The scanned-table cache of a `scan_nodes` frame.
    fn node_cache(&self, frame: Slot) -> PlanResult<Arc<NodeTableCache>>;

    /// A seed set: user ids (int64 or string).
    fn seeds(&self, slot: Slot) -> PlanResult<ArrayRef>;

    /// The rows a `sample` step keeps out of `rows` rows: their positions,
    /// ascending, or `None` for all of them. The host draws them with its own
    /// seeded generator from the step's `n` and `seed` (behind its `step` handle).
    fn sample(&self, step: Slot, rows: usize) -> PlanResult<Option<Vec<usize>>>;

    /// Host rows as batches.
    fn batches(&self, rows: HostRows) -> PlanResult<Vec<RecordBatch>>;

    /// The error the host met parsing the step behind `step` (see [`Plan`]).
    fn step_error(&self, step: Slot) -> PlanError;

    /// Tell the user `on_null="drop"` removed `n` edge rows (`n > 0`).
    fn warn_dropped(&self, n: usize) -> PlanResult<()>;

    /// A token naming the in-memory table of `frame` for a cache key: equal for
    /// the same table, never reused for another. `None` (the default) keeps a
    /// subplan over the table out of the reshaped-topology cache.
    fn identity(&self, _frame: Slot) -> PlanResult<Option<String>> {
        Ok(None)
    }
}

/// Rows held by the host, opaque to the executor.
pub type HostRows = Box<dyn Any + Send + Sync>;

/// A row set: engine batches, or rows the host holds (see [`Inputs`]).
pub enum Rows {
    Batches(Vec<RecordBatch>),
    Host(HostRows),
}

impl Rows {
    /// The rows as batches, converting host rows through `inputs`.
    pub fn into_batches(self, inputs: &dyn Inputs) -> PlanResult<Vec<RecordBatch>> {
        match self {
            Rows::Batches(b) => Ok(b),
            Rows::Host(h) => inputs.batches(h),
        }
    }
}

/// An error from planning or execution.
#[derive(Debug)]
pub enum PlanError {
    /// An unsupported plan shape (Python's `NotImplementedError`).
    NotImplemented(String),
    /// A column the plan references doesn't exist.
    ColumnNotFound(String),
    /// An invalid argument (Python's `ValueError`).
    Value(String),
    /// An argument of the wrong type (Python's `TypeError`).
    Type(String),
    /// An integer argument out of range (Python's `OverflowError`).
    Overflow(String),
    /// A [`PlanError::NotImplemented`] raised because of `cause`.
    Wrapped {
        message: String,
        cause: Box<PlanError>,
    },
    /// A malformed plan.
    Invalid(String),
    /// An engine error from an executor.
    Engine(DataFusionError),
    /// An error the host raised while resolving an input.
    Host(HostError),
}

/// An error raised by the host, carried through unchanged.
pub struct HostError {
    pub message: String,
    /// Whether the host classes it as "not implemented".
    pub not_implemented: bool,
    /// The host's own error value, for the host to restore.
    pub payload: Box<dyn Any + Send + Sync>,
}

impl fmt::Debug for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostError")
            .field("message", &self.message)
            .field("not_implemented", &self.not_implemented)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::NotImplemented(m)
            | PlanError::ColumnNotFound(m)
            | PlanError::Value(m)
            | PlanError::Type(m)
            | PlanError::Overflow(m)
            | PlanError::Wrapped { message: m, .. } => f.write_str(m),
            PlanError::Invalid(m) => write!(f, "invalid plan: {m}"),
            PlanError::Engine(e) => write!(f, "{e}"),
            PlanError::Host(e) => f.write_str(&e.message),
        }
    }
}

impl std::error::Error for PlanError {}

impl From<DataFusionError> for PlanError {
    fn from(e: DataFusionError) -> Self {
        // A timezone-aware/naive comparison keeps the `ValueError` pyarrow raised.
        if let DataFusionError::External(inner) = e.find_root() {
            if let Some(m) = inner.downcast_ref::<crate::eval::TimezoneMismatch>() {
                return PlanError::Value(m.0.clone());
            }
        }
        PlanError::Engine(e)
    }
}

impl From<arrow::error::ArrowError> for PlanError {
    fn from(e: arrow::error::ArrowError) -> Self {
        PlanError::Engine(DataFusionError::ArrowError(Box::new(e), None))
    }
}

pub type PlanResult<T> = std::result::Result<T, PlanError>;

/// Execute a plan, reading its inputs from `inputs`. Returns at least one batch.
pub fn execute_plan(plan: &Plan, inputs: &dyn Inputs) -> PlanResult<Vec<RecordBatch>> {
    execute(plan, inputs)?.into_batches(inputs)
}

/// [`execute_plan`], leaving a result the host produced with the host.
pub fn execute(plan: &Plan, inputs: &dyn Inputs) -> PlanResult<Rows> {
    match build(plan, inputs)? {
        Built::Rows(rows) => Ok(rows),
        Built::Frame(frame) => Ok(Rows::Batches((*frame).collect()?)),
    }
}

/// The aggregations `group_by().agg()` and `neighbors().agg()` compute, sorted.
pub const AGG_FNS: [&str; 6] = ["count", "max", "mean", "min", "n_unique", "sum"];

/// The error for an aggregation that is not one of [`AGG_FNS`].
pub fn check_agg_fn(func: &str) -> PlanResult<()> {
    if AGG_FNS.contains(&func) {
        return Ok(());
    }
    not_implemented(format!(
        "aggregation {} is not supported (use {}).",
        py_str(func),
        AGG_FNS.join(", ")
    ))
}

/// The `direction=` values a verb or whole-graph stat takes, and whether its
/// error is a stat's (`None` for a verb that checks its direction in the
/// engine).
pub fn verb_directions(verb: &str) -> Option<(&'static [&'static str], bool)> {
    const ALL: &[&str] = &["out", "in", "both"];
    match verb {
        "pagerank" | "betweenness" => Some((&["out", "both"], false)),
        "closeness" => Some((ALL, false)),
        "describe" | "density" | "avg_path_length" | "diameter" => Some((ALL, true)),
        _ => None,
    }
}

/// What an edge source does with a row whose endpoint is null.
pub const ON_NULL_MODES: [&str; 2] = ["error", "drop"];

/// The error collecting `edges.nodes()` with no columns raises: the one shape
/// error a later step can fix (a `with_columns`), so it is never raised before
/// a collect.
pub(crate) const BARE_NODES: &str = "collect() on this NodeFrame needs a source (scan_nodes / \
     from_arrow(id=...)) or a with_columns of graph algorithms; a bare edges.nodes() has no \
     materializable node set yet.";

/// The end of the error for relational steps on a traversal result's edges
/// before `nodes()`. A later `with_columns` over those edges raises the graph
/// op's own guard first, so it is not raised before a collect either.
pub(crate) const STEPS_BEFORE_NODES: &str = " on the edges before nodes() is not supported yet; \
     collect the edge frame and re-ingest it via ur.from_arrow(...) first.";

/// What a host returns where a plan's shape is checked without its inputs
/// ([`check_shape`]): the walk stops at the first input it needs.
#[derive(Debug)]
pub struct Pending;

/// The error an [`Inputs`] returns for an input it does not provide while a
/// plan's shape is checked.
pub fn pending() -> PlanError {
    PlanError::Host(HostError {
        message: "an input is needed".into(),
        not_implemented: false,
        payload: Box::new(Pending),
    })
}

fn is_pending(e: &PlanError) -> bool {
    matches!(e, PlanError::Host(h) if h.payload.is::<Pending>())
}

/// Check a plan's shape the way [`execute_plan`] does before it reads anything:
/// its guards and its steps' arguments, in the order a collect checks them,
/// up to the first input it needs (`inputs` returns [`pending`] for every input
/// but [`Inputs::step_error`]). The first error is returned.
pub fn check_shape(plan: &Plan, inputs: &dyn Inputs) -> PlanResult<()> {
    match build(plan, inputs) {
        Err(e) if !is_pending(&e) => Err(e),
        _ => Ok(()),
    }
}

/// A plan run over empty inputs ([`crate::dry`]): the schema a collect returns,
/// and the DataFusion plans it would execute, in order. `inputs` serves empty
/// tables and indexes.
pub fn dry_run(
    plan: &Plan,
    inputs: &dyn Inputs,
) -> PlanResult<(
    arrow::datatypes::SchemaRef,
    Vec<datafusion::logical_expr::LogicalPlan>,
)> {
    let run = crate::dry::DryRun::enter();
    let rows = execute(plan, inputs)?.into_batches(inputs)?;
    Ok((rows[0].schema(), run.plans()))
}

/// A plan, built: a source's rows as they are (no step to run), or a frame to
/// lower further steps onto and collect.
enum Built {
    Rows(Rows),
    Frame(Box<Frame>),
}

impl Built {
    fn frame(frame: Frame) -> Built {
        Built::Frame(Box::new(frame))
    }
}

/// Build a plan's frame: run its guards, build its leaf, and lower its steps,
/// without collecting it.
fn build(plan: &Plan, inputs: &dyn Inputs) -> PlanResult<Built> {
    if let Plan::Invalid { step } = plan {
        return Err(inputs.step_error(*step));
    }
    let chain = plan.chain();
    if is_node_frame(&chain) {
        node_frame(&chain, inputs)
    } else {
        edge_frame(&chain, inputs)
    }
}

/// A plan as a [`Frame`], for a step that takes a whole frame (a join's right
/// side): built like any frame, its steps lowered, and not collected.
fn frame_of(plan: &Plan, inputs: &dyn Inputs) -> PlanResult<Frame> {
    match build(plan, inputs)? {
        Built::Frame(frame) => Ok(*frame),
        Built::Rows(rows) => Ok(Frame::from_batches(
            crate::lower::session(),
            rows.into_batches(inputs)?,
        )?),
    }
}

/// The graph index an edge input runs over, after the checks a graph op over it
/// needs. For a traversal result that is its input's index (the traversal only
/// masks it), so its input is checked too.
pub fn require_index(edges: &EdgeInput, inputs: &dyn Inputs) -> PlanResult<GraphIndex> {
    // The view checks a traversal's input too (it is a view of its own), and
    // names the frame whose index a traversal result runs over: its input's.
    let view = edges.view()?;
    view_index(&view, inputs, &mut Scratch::default())
}

/// Per-query state shared while an edge input is resolved (its index, weights
/// and mask): the seed sets read so far, and each reshaped topology's
/// fingerprint and rows, so a subplan runs at most once per query.
#[derive(Default)]
pub struct Scratch {
    seeds: HashMap<Slot, ArrayRef>,
    /// By the topology plan's address.
    keys: HashMap<usize, crate::reshape::TopologyKey>,
    rows: HashMap<usize, Vec<RecordBatch>>,
    /// The indexes of topologies that can't be cached across queries, by their
    /// key: built once per query.
    local: HashMap<String, GraphIndex>,
}

impl Scratch {
    /// A seed set, read from the host on first use.
    pub fn seed_set(&mut self, slot: Slot, inputs: &dyn Inputs) -> PlanResult<ArrayRef> {
        if let Some(a) = self.seeds.get(&slot) {
            return Ok(a.clone());
        }
        let a = inputs.seeds(slot)?;
        self.seeds.insert(slot, a.clone());
        Ok(a)
    }
}

/// The index a view runs over: its frame's (built on first use), or for a
/// reshaped topology the cached index of its subplan, built from the subplan's
/// rows on a miss.
fn view_index(
    view: &EdgeView<'_>,
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<GraphIndex> {
    if !view.reshaped {
        return inputs.index(view.frame);
    }
    let key = topology_key(view, inputs, scratch)?;
    if key.cacheable && !crate::dry::active() {
        return crate::reshape::cached_index(&key.key, || {
            let rows = topology_rows(view, None, inputs, scratch)?;
            crate::reshape::build_index(&rows, &view.roles)
        });
    }
    if let Some(index) = scratch.local.get(&key.key) {
        return Ok(index.clone());
    }
    let rows = topology_rows(view, None, inputs, scratch)?;
    let index = crate::reshape::build_index(&rows, &view.roles)?;
    scratch.local.insert(key.key, index.clone());
    Ok(index)
}

/// The index a view runs over if it is already built (never builds one).
fn built_view_index(
    view: &EdgeView<'_>,
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<Option<GraphIndex>> {
    if !view.reshaped {
        return inputs.built_index(view.frame);
    }
    let key = topology_key(view, inputs, scratch)?;
    // A dry run never reads the cache: its topologies are empty, kept per query.
    Ok(if key.cacheable && !crate::dry::active() {
        crate::reshape::lookup(&key.key)
    } else {
        scratch.local.get(&key.key).cloned()
    })
}

/// The fingerprint of a reshaped view's topology (computed once per query).
fn topology_key(
    view: &EdgeView<'_>,
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<crate::reshape::TopologyKey> {
    let at = view.topology as *const Plan as usize;
    if let Some(k) = scratch.keys.get(&at) {
        return Ok(k.clone());
    }
    let key = crate::reshape::fingerprint(view.topology, &view.roles, inputs, scratch)?;
    scratch.keys.insert(at, key.clone());
    Ok(key)
}

/// The rows of a view's topology, in index order: a reshaped subplan's output
/// (run once per query), or the source's rows reading at least `columns`
/// (`None`: all).
fn topology_rows(
    view: &EdgeView<'_>,
    columns: Option<&[String]>,
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<Vec<RecordBatch>> {
    if !view.reshaped {
        return edge_rows(view.topology.chain()[0], view.frame, columns, inputs);
    }
    let at = view.topology as *const Plan as usize;
    if let Some(rows) = scratch.rows.get(&at) {
        return Ok(rows.clone());
    }
    let rows = frame_of(view.topology, inputs)?.collect()?;
    scratch.rows.insert(at, rows.clone());
    Ok(rows)
}

/// `rows` after one column step (`select`, `rename`, `with_columns`).
fn column_step(
    rows: Vec<RecordBatch>,
    step: &Plan,
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<Vec<RecordBatch>> {
    check_step(step, inputs)?;
    let frame = Frame::from_batches(crate::lower::session(), rows)?;
    Ok(lower_node_steps(frame, &[step], roles, None, inputs)?.collect()?)
}

/// The weight-cache scope of a view: the column steps its weights read through
/// (empty without any, so a source's weights keep their plain keys).
fn weight_scope(view: &EdgeView<'_>) -> String {
    if !view.has_column_steps() {
        return String::new();
    }
    let mut scope = String::from("columns [");
    for s in view.steps.iter().filter(|s| s.op() != "filter") {
        scope.push_str(&column_step_key(s));
    }
    scope.push_str("] ");
    scope
}

/// A column step's arguments, for a cache key.
fn column_step_key(step: &Plan) -> String {
    match step {
        Plan::Select { columns, .. } => format!("select {columns:?};"),
        Plan::Rename { mapping, .. } => format!("rename {mapping:?};"),
        Plan::WithColumns { columns, .. } => format!("with_columns {columns:?};"),
        other => format!("{};", other.op()),
    }
}

// --- frame shape -------------------------------------------------------------

pub(crate) fn is_node_frame(chain: &[&Plan]) -> bool {
    chain.iter().any(|p| matches!(p, Plan::Nodes { .. }))
        || matches!(
            chain[0],
            Plan::ScanNodes { .. }
                | Plan::FromArrow { id: Some(_), .. }
                | Plan::RandomWalk { .. }
                | Plan::Describe { .. }
        )
}

/// A node frame's id column.
pub(crate) fn node_id_col(chain: &[&Plan]) -> String {
    if chain.iter().any(|p| matches!(p, Plan::Nodes { .. })) {
        return "id".into();
    }
    match chain[0] {
        Plan::ScanNodes { id, .. } | Plan::FromArrow { id: Some(id), .. } => id.clone(),
        Plan::RandomWalk { .. } => "node".into(),
        _ => "__summary__".into(),
    }
}

/// An edge frame's `(src, dst)` columns.
pub(crate) fn edge_roles(chain: &[&Plan]) -> (String, String) {
    let (mut src, mut dst) = match chain[0] {
        Plan::ScanEdges { src, dst, .. }
        | Plan::FromArrow {
            src: Some(src),
            dst: Some(dst),
            ..
        } => (src.clone(), dst.clone()),
        _ => ("src".to_string(), "dst".to_string()),
    };
    for p in chain {
        if matches!(p, Plan::Reverse { .. }) {
            std::mem::swap(&mut src, &mut dst);
        }
    }
    (src, dst)
}

/// The host slot of a frame's source.
pub(crate) fn source_slot(chain: &[&Plan]) -> Option<Slot> {
    match chain[0] {
        Plan::ScanEdges { frame, .. }
        | Plan::ScanNodes { frame, .. }
        | Plan::FromArrow { frame, .. } => Some(*frame),
        _ => None,
    }
}

pub(crate) fn not_implemented<T>(msg: impl Into<String>) -> PlanResult<T> {
    Err(PlanError::NotImplemented(msg.into()))
}

fn is_tail(p: &Plan) -> bool {
    TAIL_OPS.contains(&p.op())
}

// --- node frames ---------------------------------------------------------------

/// A node frame: `describe` or a random walk with the steps written after it, or
/// a node source (an attribute table, or `edges.nodes()`) and its steps in the
/// order written: graph columns (`with_columns`), joins, and the relational
/// steps, lowered onto one frame.
fn node_frame(chain: &[&Plan], inputs: &dyn Inputs) -> PlanResult<Built> {
    let id_col = node_id_col(chain);
    // describe() is a whole-graph summary, computed eagerly off the topology and
    // wrapped as a one-row frame; steps written after it run on that row.
    if let Some(Plan::Describe {
        edges,
        full,
        direction,
    }) = chain.first()
    {
        let steps = leaf_steps(chain, "describe()", inputs)?;
        // The edges' subgraph view, as every graph op reads it (#207): the
        // topology's node set, and only the edge rows its mask keeps.
        let view = edges.view()?;
        let mut scratch = Scratch::default();
        let index = view_index(&view, inputs, &mut scratch)?;
        let mask = edge_mask(&view, &index, inputs, &mut scratch)?;
        let dir = parse_stat_direction(direction)?;
        let summary = describe_masked(&index.topo, mask.as_deref(), *full, dir)?;
        if steps.is_empty() {
            return Ok(Built::Rows(Rows::Batches(vec![summary])));
        }
        let frame = Frame::from_batches(crate::lower::session(), vec![summary])?;
        return Ok(Built::frame(lower_node_steps(
            frame,
            &steps,
            &[],
            None,
            inputs,
        )?));
    }

    if let Some(walk) = chain.iter().find(|p| matches!(p, Plan::RandomWalk { .. })) {
        return random_walk_frame(chain, walk, inputs);
    }

    // The steps after the `nodes()` boundary, in the order written. A
    // `nodes()`-derived frame inherits the edge plan: its steps before the
    // boundary are edge-plan steps (a `filter` there is a subgraph view, read as
    // the edge mask; a reshaping step makes the node set its rows' nodes), not
    // node-result operators.
    let mut steps: Vec<&Plan> = vec![];
    let mut pre_nodes: Vec<&str> = vec![]; // non-filter edge-plan steps before `nodes`
    let mut past_nodes_boundary = !chain.iter().any(|p| matches!(p, Plan::Nodes { .. }));
    for &step in chain {
        let op = step.op();
        if matches!(
            op,
            "scan_edges"
                | "scan_nodes"
                | "nodes"
                | "from_arrow"
                | "reverse"
                | "hop"
                | "shortest_path"
        ) {
            if op == "nodes" {
                past_nodes_boundary = true;
            }
            continue; // source / metadata steps
        }
        if !past_nodes_boundary && (is_tail(step) || op == "join" || op == "with_columns") {
            if op != "filter" {
                pre_nodes.push(op);
            }
            continue;
        }
        match step {
            Plan::Join { how, .. } => check_join_how(how)?,
            Plan::WithColumns { columns, .. } => {
                for e in columns {
                    if let Expr::Graph(verb) = &e.expr {
                        check_verb(verb)?;
                    }
                }
            }
            _ if !is_tail(step) => {
                return not_implemented(format!(
                    "collect() does not yet support the '{op}' step in a composed pipeline."
                ));
            }
            _ => {}
        }
        check_step(step, inputs)?;
        steps.push(step);
    }
    let first_columns = steps.iter().find_map(|s| match s {
        Plan::WithColumns { columns, .. } => Some(columns.as_slice()),
        _ => None,
    });

    // The edge frame `nodes()` was called on, when steps other than `filter`
    // shape it: its view is checked, and a reshaped one's rows give the node set.
    let nodes_input = chain.iter().find_map(|p| match p {
        Plan::Nodes { input } => Some(input.as_ref()),
        _ => None,
    });
    let after_traversal = nodes_input.is_some_and(|p| {
        p.chain()
            .iter()
            .any(|s| matches!(s, Plan::Hop { .. } | Plan::ShortestPath { .. }))
    });
    // Its slot is not in the plan: only a reshaped topology, which needs none,
    // is resolved through it.
    let shaped_edges = nodes_input
        .filter(|_| !pre_nodes.is_empty() && !after_traversal)
        .map(|p| EdgeInput {
            plan: Box::new(p.clone()),
            frame: Slot::MAX,
        });
    let shaped = shaped_edges.as_ref().map(EdgeInput::view).transpose()?;
    if !pre_nodes.is_empty() && after_traversal {
        // A graph op over that derived edge frame raises the precise guard;
        // otherwise the node set would silently ignore the step.
        if let Some(exprs) = first_columns.filter(|g| !g.is_empty()) {
            if let Some((edges, _)) = edge_groups(graph_columns(exprs))?.first() {
                edges.view()?;
            }
        }
        let mut seen = HashSet::new();
        let ops: Vec<String> = pre_nodes
            .into_iter()
            .filter(|op| seen.insert(*op))
            .map(|op| format!("{op}()"))
            .collect();
        return not_implemented(format!("{}{STEPS_BEFORE_NODES}", ops.join("/")));
    }

    let roles = [("id", id_col.clone())];
    let Some(slot) = node_source_slot(chain) else {
        // `edges.nodes()`: no rows until the first with_columns computes them.
        if first_columns.is_none() {
            return not_implemented(BARE_NODES);
        }
        let first = steps
            .iter()
            .position(|s| matches!(s, Plan::WithColumns { .. }))
            .expect("a with_columns step");
        check_before_node_set(&steps[..first], steps[first])?;
        let Plan::WithColumns { columns, .. } = steps[first] else {
            unreachable!("found above")
        };
        let node_set = shaped.as_ref().filter(|v| v.reshaped);
        let frame = node_set_frame(columns, node_set, inputs)?;
        let frame = lower_node_steps(frame, &steps[..first], &roles, None, inputs)?;
        let frame = lower_node_steps(frame, &steps[first + 1..], &roles, None, inputs)?;
        return Ok(Built::frame(frame));
    };
    // A scan reads the id, what a neighbour aggregation reads, and whatever the
    // steps read before a select or group_by narrows the columns.
    let mut required: BTreeSet<String> = BTreeSet::from([id_col.clone()]);
    for s in &steps {
        if let Plan::WithColumns { columns, .. } = s {
            for e in columns {
                if let Expr::Graph(verb) = &e.expr {
                    if let Some(c) = verb.agg_column() {
                        required.insert(c.to_string());
                    }
                }
            }
        }
    }
    let needed = projection(&steps, required, &roles);
    let table = node_rows(chain, slot, needed.as_deref(), inputs)?;
    if steps.is_empty() {
        return Ok(Built::Rows(table));
    }
    let table = table.into_batches(inputs)?;
    let frame = Frame::from_batches(crate::lower::session(), table.clone())?;
    let frame = lower_node_steps(frame, &steps, &roles, Some(&table), inputs)?;
    Ok(Built::frame(frame))
}

/// The steps written after a leaf (`describe`, a random walk): relational steps
/// and joins.
fn leaf_steps<'a>(
    chain: &[&'a Plan],
    leaf: &str,
    inputs: &dyn Inputs,
) -> PlanResult<Vec<&'a Plan>> {
    let mut steps = vec![];
    for &s in &chain[1..] {
        if matches!(s, Plan::Nodes { .. }) {
            continue;
        }
        match s {
            Plan::Join { how, .. } => check_join_how(how)?,
            _ if is_tail(s) => {}
            _ => {
                return not_implemented(format!(
                    "collect() does not yet support the '{}' step after {leaf}.",
                    s.op()
                ))
            }
        }
        check_step(s, inputs)?;
        steps.push(s);
    }
    Ok(steps)
}

/// `edges.nodes()` has no rows before its first `with_columns` computes them, so
/// only a filter that commutes with it may be written first: one that reads no
/// column it computes.
fn check_before_node_set(before: &[&Plan], with_columns: &Plan) -> PlanResult<()> {
    let Plan::WithColumns { columns, .. } = with_columns else {
        unreachable!("a with_columns step")
    };
    let computed: HashSet<&str> = columns.iter().map(|e| e.name.as_str()).collect();
    for step in before {
        let Plan::Filter { predicate, .. } = step else {
            let verb = match step.op() {
                "group_by_agg" => "group_by().agg",
                op => op,
            };
            return not_implemented(format!(
                "with_columns() after {verb}() is not supported; compute columns first, \
                 then {verb}()."
            ));
        };
        let clash: BTreeSet<String> = expr_columns(predicate, &[("id", "id".to_string())])
            .into_iter()
            .filter(|c| computed.contains(c.as_str()))
            .collect();
        if !clash.is_empty() {
            return not_implemented(format!(
                "filter() before with_columns() references column(s) {} that \
                 with_columns() computes; filter after the with_columns().",
                py_list(clash.iter())
            ));
        }
    }
    Ok(())
}

/// The graph verbs of a `with_columns`.
pub(crate) fn graph_columns(exprs: &[NamedExpr]) -> Vec<(&str, &GraphVerb)> {
    exprs
        .iter()
        .filter_map(|e| match &e.expr {
            Expr::Graph(v) => Some((e.name.as_str(), v.as_ref())),
            Expr::Scalar(_) => None,
        })
        .collect()
}

/// A graph column of a `with_columns`: its name and verb.
pub(crate) type GraphColumn<'a> = (&'a str, &'a GraphVerb);

/// The graph columns that run over one edge input.
pub(crate) type EdgeGroup<'a> = (&'a EdgeInput, Vec<GraphColumn<'a>>);

/// The graph verbs of a `with_columns`, grouped by the edge frame they run
/// over, in the order each frame first appears. A verb with no edges is an error.
pub(crate) fn edge_groups<'a>(verbs: Vec<GraphColumn<'a>>) -> PlanResult<Vec<EdgeGroup<'a>>> {
    let mut groups: Vec<EdgeGroup<'a>> = vec![];
    for (name, verb) in verbs {
        let Some(edges) = &verb.edges else {
            return not_implemented(NON_GRAPH_COLUMN);
        };
        match groups.iter_mut().find(|(e, _)| e.frame == edges.frame) {
            Some((_, g)) => g.push((name, verb)),
            None => groups.push((edges, vec![(name, verb)])),
        }
    }
    Ok(groups)
}

pub(crate) const NON_GRAPH_COLUMN: &str = "with_columns() takes ursa expressions: ur.col(...), \
     ur.lit(...), arithmetic and comparisons over them, or graph verbs; got a value of \
     another kind.";

/// The graph columns of one edge frame in a `with_columns`, computed: the index
/// and mask their edges run over, and the columns, built.
struct GraphColumns {
    index: GraphIndex,
    mask: Option<Arc<EdgeMask>>,
    columns: Vec<crate::result::OutputColumn>,
}

/// The graph columns of a `with_columns`, one [`GraphColumns`] per edge frame
/// the verbs run over, in the order written (#191: each verb has its own edge
/// input). `nodes` is the frame's attribute table (what a neighbour aggregation
/// reads), if it has one.
fn graph_columns_for(
    exprs: &[NamedExpr],
    nodes: Option<(&[RecordBatch], &str)>,
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<Vec<GraphColumns>> {
    let verbs = graph_columns(exprs);
    if verbs.is_empty() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for (edges, verbs) in edge_groups(verbs)? {
        out.push(edge_group_columns(edges, &verbs, nodes, inputs, scratch)?);
    }
    Ok(out)
}

/// The columns of the verbs that run over one edge input.
fn edge_group_columns(
    edges: &EdgeInput,
    verbs: &[(&str, &GraphVerb)],
    nodes: Option<(&[RecordBatch], &str)>,
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<GraphColumns> {
    // A weighted algorithm needs the edge columns its weight expression reads.
    // The read is skipped when the index already caches every one of these
    // weights (and, for a scan, builds the index too).
    let mut weight_cols: BTreeSet<String> = BTreeSet::new();
    let mut weight_keys: Vec<String> = vec![];
    for (_, verb) in verbs {
        if let Some(w) = &verb.weight {
            check_literals(w, "weight")?;
            weight_cols.extend(value_columns(w, &[]));
            weight_keys.push(w.to_string());
        }
    }
    let view = edges.view()?;
    let edge_attr = if weight_cols.is_empty() {
        None
    } else {
        prepare_weighted(&view, &weight_cols, &weight_keys, inputs, scratch)?
    };
    let index = view_index(&view, inputs, scratch)?;
    let mask = edge_mask(&view, &index, inputs, scratch)?;
    let specs: Vec<NodeColumn> = verbs
        .iter()
        .map(|(name, verb)| (name.to_string(), (*verb).clone()))
        .collect();
    let scope = weight_scope(&view);
    let columns = node_columns(
        &index.topo,
        &index.ids,
        &specs,
        nodes.map(|(b, _)| b),
        nodes.map(|(_, id)| id),
        edge_attr.as_deref(),
        Some((index.weights.as_ref(), scope.as_str())),
    )?;
    Ok(GraphColumns {
        index,
        mask,
        columns,
    })
}

/// The frame of `edges.nodes().with_columns(...)`: the node set, in node order,
/// with the graph columns, then the `with_columns`' other expressions.
///
/// The node set is `node_set`'s (the view of a reshaped edge frame `nodes()` was
/// called on) or else the first graph column's edge frame's, as it always was.
/// The columns of every other edge frame attach to those nodes by id, as a later
/// `with_columns` would attach them: a node that frame's graph lacks gets null.
fn node_set_frame(
    exprs: &[NamedExpr],
    node_set: Option<&EdgeView<'_>>,
    inputs: &dyn Inputs,
) -> PlanResult<Frame> {
    // One scratch for the columns and the node set: a topology they share is
    // built once, even one that can't be cached across queries.
    let mut scratch = Scratch::default();
    let mut groups = graph_columns_for(exprs, None, inputs, &mut scratch)?.into_iter();
    let Some(first) = groups.next() else {
        return not_implemented(NON_GRAPH_COLUMN);
    };
    // Resolved after the columns, so a topology they share is built once.
    let node_set = node_set
        .map(|v| view_index(v, inputs, &mut scratch))
        .transpose()?;
    let ctx = crate::lower::session();
    let mut frame = match node_set {
        Some(set) if !Arc::ptr_eq(&set.topo, &first.index.topo) => {
            let leaf = graph_leaf(&ctx, set.topo, set.ids, vec![], None);
            let mut frame = Frame::from_ordered(ctx.clone(), leaf);
            attach_group(&mut frame, first, "id")?;
            frame
        }
        _ => {
            let leaf = graph_leaf(
                &ctx,
                first.index.topo,
                first.index.ids,
                first.columns,
                first.mask,
            );
            Frame::from_ordered(ctx, leaf)
        }
    };
    for g in groups {
        attach_group(&mut frame, g, "id")?;
    }
    let before: Vec<String> = vec!["id".to_string()];
    scalar_columns(&mut frame, exprs, &before, &[("id", "id".to_string())])?;
    Ok(frame)
}

/// Attach one edge frame's graph columns to `frame`'s rows by the id column
/// `id_col`.
fn attach_group(frame: &mut Frame, g: GraphColumns, id_col: &str) -> PlanResult<()> {
    let ctx = frame.ctx().clone();
    let rows = frame.ordered_batches()?;
    let attached = attach_columns(
        &ctx,
        g.index.topo,
        g.index.ids,
        g.columns,
        g.mask,
        rows,
        id_col,
    )?;
    frame.set_batches(attached)?;
    Ok(())
}

/// Lower the steps of a frame, in order: relational steps, `with_columns` (graph
/// columns attached to the rows by id) and joins. `table` is the frame's
/// attribute table, which a neighbour aggregation reads.
fn lower_node_steps(
    mut frame: Frame,
    steps: &[&Plan],
    roles: &Roles,
    table: Option<&[RecordBatch]>,
    inputs: &dyn Inputs,
) -> PlanResult<Frame> {
    // The id column graph columns attach by (a node frame's `id` role).
    let id_col = roles.first().map_or("id", |(_, c)| c.as_str());
    let mut rest = steps;
    while let Some(i) = rest
        .iter()
        .position(|s| matches!(s, Plan::WithColumns { .. } | Plan::Join { .. }))
    {
        lower_steps(&mut frame, &rest[..i], roles, inputs)?;
        match rest[i] {
            Plan::WithColumns { columns, .. } => {
                with_columns(&mut frame, columns, id_col, table, roles, inputs)?
            }
            join => frame = run_join(frame, join, inputs)?,
        }
        rest = &rest[i + 1..];
    }
    lower_steps(&mut frame, rest, roles, inputs)?;
    Ok(frame)
}

/// `with_columns` on a frame with rows: the graph columns attached by the id
/// column `id_col`, then the other expressions, evaluated over the frame's
/// columns. A column that already exists is replaced in place; new columns are
/// appended in the order written.
fn with_columns(
    frame: &mut Frame,
    exprs: &[NamedExpr],
    id_col: &str,
    table: Option<&[RecordBatch]>,
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<()> {
    let before = frame.names();
    let nodes = table.map(|t| (t, id_col));
    let groups = graph_columns_for(exprs, nodes, inputs, &mut Scratch::default())?;
    if !groups.is_empty() && !frame.has_column(id_col) {
        return Err(PlanError::ColumnNotFound(format!(
            "with_columns() attaches graph columns by the node id column {}, which an \
             earlier step removed or renamed.",
            py_str(id_col)
        )));
    }
    // Each edge frame's columns attach by id; the attribute frame's rows decide.
    for g in groups {
        attach_group(frame, g, id_col)?;
    }
    scalar_columns(frame, exprs, &before, roles)
}

/// The non-graph expressions of a `with_columns`, evaluated over the frame (its
/// columns `before` the graph columns were attached), and the column order:
/// `before`, with replaced columns in place, then each new column as written.
fn scalar_columns(
    frame: &mut Frame,
    exprs: &[NamedExpr],
    before: &[String],
    roles: &Roles,
) -> PlanResult<()> {
    if exprs.iter().all(|e| matches!(e.expr, Expr::Graph(_))) {
        // The graph columns were attached one edge frame at a time, after the
        // `before` columns: in written order unless the edge frames interleave.
        let names = frame.names();
        let attached = names.get(before.len()..).unwrap_or_default();
        if attached.iter().eq(exprs.iter().map(|e| &e.name)) {
            return Ok(());
        }
    }
    let mut lowered: HashMap<&str, DfExpr> = HashMap::new();
    for e in exprs {
        if let Expr::Scalar(v) = &e.expr {
            lowered.insert(&e.name, scalar_expr(frame, v, roles, "with_columns()")?);
        }
    }
    let mut out: Vec<DfExpr> = before
        .iter()
        .map(|c| match lowered.remove(c.as_str()) {
            Some(e) => e.alias(c),
            None => col_named(c),
        })
        .collect();
    for e in exprs {
        if before.contains(&e.name) {
            continue;
        }
        out.push(match lowered.remove(e.name.as_str()) {
            Some(x) => x.alias(&e.name),
            None => col_named(&e.name),
        });
    }
    frame.project(out)?;
    Ok(())
}

/// A scalar expression over the frame's columns (`what` names the step, for its
/// errors): its role references resolved, its columns checked, lowered with
/// comparisons resolved as a filter's are.
fn scalar_expr(frame: &Frame, v: &Value, roles: &Roles, what: &str) -> PlanResult<DfExpr> {
    Ok(scalar_check(frame, v, roles, what)?.expect("a frame knows its types"))
}

/// [`scalar_expr`] over any [`View`]: its checks, in order, and the lowered
/// expression when the view knows its column types.
pub(crate) fn scalar_check(
    view: &dyn View,
    v: &Value,
    roles: &Roles,
    what: &str,
) -> PlanResult<Option<DfExpr>> {
    if !matches!(
        kind(v),
        "col" | "lit" | "binary" | "unary" | "src" | "dst" | "id" | "alias"
    ) {
        return not_implemented(NON_GRAPH_COLUMN);
    }
    check_literals(v, what)?;
    let v = resolve_roles(v, roles)?;
    let missing: Vec<String> = value_columns(&v, &[])
        .into_iter()
        .filter(|c| !view.has_column(c))
        .collect();
    if let Some(c) = missing.first() {
        return Err(unknown_column(what, c, &view.names()));
    }
    let e = crate::expr::lower(&crate::expr::parse_ursa_expr(&v)?)?;
    match view.typed() {
        Some(schema) => Ok(Some(crate::compare::resolve_comparisons(e, schema)?)),
        None => Ok(None),
    }
}

/// The columns a step's checks read: a frame being lowered, or the columns a
/// frame is known to have before it runs (`crate::infer`).
pub(crate) trait View {
    /// The column names, in order.
    fn names(&self) -> Vec<String>;
    fn has_column(&self, name: &str) -> bool;
    /// The columns as an Arrow schema (only the names are read where the view
    /// does not know its types).
    fn arrow(&self) -> arrow::datatypes::SchemaRef;
    /// The schema to type expressions against, when the types are known.
    fn typed(&self) -> Option<&datafusion::common::DFSchema>;
}

impl View for Frame {
    fn names(&self) -> Vec<String> {
        Frame::names(self)
    }
    fn has_column(&self, name: &str) -> bool {
        Frame::has_column(self, name)
    }
    fn arrow(&self) -> arrow::datatypes::SchemaRef {
        self.schema()
    }
    fn typed(&self) -> Option<&datafusion::common::DFSchema> {
        Some(self.df_schema())
    }
}

/// The host slot of a node source (`None` for a `nodes()`-derived frame).
pub(crate) fn node_source_slot(chain: &[&Plan]) -> Option<Slot> {
    match chain[0] {
        Plan::ScanNodes { frame, .. }
        | Plan::FromArrow {
            frame, id: Some(_), ..
        } => Some(*frame),
        _ => None,
    }
}

/// The checks `_algo_column` ran on each graph column before it reached the engine.
pub(crate) fn check_verb(verb: &GraphVerb) -> PlanResult<()> {
    let name = verb.verb.as_str();
    if !EXECUTABLE.contains(&name) {
        return not_implemented(format!(
            "the '{name}' kernel is not wired into the execution path yet (wired: {}).",
            EXECUTABLE.join(", ")
        ));
    }
    if name == "connected_components" {
        let mode = verb.mode.clone().unwrap_or_else(|| Value::from("weak"));
        if !matches!(mode.as_str(), Some("weak" | "strong")) {
            return not_implemented(format!(
                "connected_components(mode={}) is not supported; use mode='weak' or \
                 mode='strong'.",
                py_value(&mode)
            ));
        }
    }
    if name == "neighbors_agg" {
        if verb.from == Some(true) {
            return not_implemented(
                "neighbors(from_=...) is not supported yet; the aggregation resolves \
                 against the frame it runs in.",
            );
        }
        let is_agg = verb.agg.as_ref().is_some_and(|a| kind(a) == "agg");
        if !is_agg || verb.agg_column().is_none() {
            return not_implemented(
                "neighbors().agg() supports ur.col(<name>).<fn>() with fn in \
                 mean/sum/min/max/count/n_unique (mean/sum/min/max need a numeric \
                 attribute column; count/n_unique also accept strings).",
            );
        }
    }
    // A weight= on a verb that doesn't consume it would be silently ignored.
    let weighted = ["pagerank", "closeness", "betweenness", "louvain"];
    if verb.weight.is_some() && !weighted.contains(&name) {
        return not_implemented(format!("weight= is not supported for '{name}'."));
    }
    // Output dtype narrowing (#117). Positions are f32 at the source, so dtype has
    // no meaning on a layout; integer kernels have no f32 form.
    let dtype = verb.dtype.as_ref().filter(|d| !d.is_null());
    if MULTI_OUTPUT_VERBS.contains(&name) && dtype.is_some() {
        return not_implemented(format!(
            "'{name}' already emits f32 positions; dtype= has nothing to narrow."
        ));
    }
    if let Some(dtype) = dtype.filter(|d| d.as_str() != Some("f64")) {
        if dtype.as_str() != Some("f32") {
            return not_implemented(format!(
                "dtype must be 'f32' or 'f64'; got {}.",
                py_value(dtype)
            ));
        }
        if !FLOAT_VALUED_VERBS.contains(&name) {
            return not_implemented(format!(
                "dtype='f32' is only supported for float-valued kernels ({}); '{name}' \
                 emits integers.",
                FLOAT_VALUED_VERBS.join(", ")
            ));
        }
    }
    Ok(())
}

/// The graph verbs wired into execution, sorted.
const EXECUTABLE: [&str; 13] = [
    "betweenness",
    "closeness",
    "clustering_coefficient",
    "connected_components",
    "degree",
    "label_propagation",
    "layout_circle",
    "layout_fr",
    "layout_random",
    "louvain",
    "neighbors_agg",
    "pagerank",
    "triangle_count",
];

/// Kernels that emit more than one column from one run (#115).
const MULTI_OUTPUT_VERBS: [&str; 3] = ["layout_circle", "layout_fr", "layout_random"];

/// The kernels whose output is floating-point (the only ones `dtype="f32"` fits),
/// sorted.
const FLOAT_VALUED_VERBS: [&str; 5] = [
    "betweenness",
    "closeness",
    "clustering_coefficient",
    "neighbors_agg",
    "pagerank",
];

/// The edge rows a weight reads: `columns` of the view's rows, aligned with its
/// index's edge rows, or `None` when the index already caches every weight in
/// `keys` (#179). For a scan with no index yet the same read builds the index.
fn prepare_weighted(
    view: &EdgeView<'_>,
    columns: &BTreeSet<String>,
    keys: &[String],
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<Option<Vec<RecordBatch>>> {
    let frame = view.frame;
    if !keys.is_empty() {
        if let Some(index) = built_view_index(view, inputs, scratch)? {
            let scope = weight_scope(view);
            if keys
                .iter()
                .all(|k| index.weights.get(&format!("{scope}{k}")).is_some())
            {
                return Ok(None);
            }
        }
    }
    const NO_SOURCE: &str = "a weighted algorithm needs an in-memory or scan_edges source so \
         the weight columns can be read.";
    let columns: Vec<String> = columns.iter().cloned().collect();
    if view.sourced && (view.reshaped || view.has_column_steps()) {
        // The rows as the steps above the topology leave their columns.
        let roles = view_roles(view);
        let mut rows = topology_rows(view, None, inputs, scratch)?;
        for step in view.steps.iter().filter(|s| s.op() != "filter") {
            rows = column_step(rows, step, &roles, inputs)?;
        }
        return weight_columns(&rows, &columns).map(Some);
    }
    let chain = view.topology.chain();
    match chain[0] {
        _ if !view.sourced => not_implemented(NO_SOURCE),
        Plan::FromArrow { .. } => {
            let Some(table) = inputs.table(frame)? else {
                return not_implemented(NO_SOURCE);
            };
            let table = table.into_batches(inputs)?;
            weight_columns(&table, &columns).map(Some)
        }
        Plan::ScanEdges { path, on_null, .. } => {
            let path = single_path(
                path,
                "weighted algorithms over a multi-path scan_edges source are not supported yet.",
            )?;
            let opts = inputs.storage_options(frame)?;
            // The frame's own roles: a reversed scan reads its endpoints swapped,
            // as its index was built.
            let (src, dst) = &view.roles;
            let mut batches = scan_edges_batch(path, src, dst, &opts, &columns)?;
            if on_null == "drop" {
                let dropped = drop_null_endpoints(&mut batches)?;
                if dropped > 0 {
                    inputs.warn_dropped(dropped)?;
                }
            }
            // One scan, src/dst first: with no index yet, these rows build it, so
            // the weights line up with its edge rows by construction.
            if inputs.built_index(frame)?.is_none() {
                let pairs: Vec<(&dyn Array, &dyn Array)> = batches
                    .iter()
                    .map(|b| (b.column(0).as_ref(), b.column(1).as_ref()))
                    .collect();
                let (topo, ids) = crate::topology::build_topology_batches(&pairs)
                    .map_err(|e| PlanError::Value(e.to_string()))?;
                count_topology_build();
                inputs.put_index(frame, GraphIndex::new(topo, ids))?;
            }
            Ok(Some(batches))
        }
        _ => not_implemented(NO_SOURCE),
    }
}

/// `columns` of edge rows, for a weight; an unknown one is an error.
fn weight_columns(table: &[RecordBatch], columns: &[String]) -> PlanResult<Vec<RecordBatch>> {
    let schema = table[0].schema();
    let missing: Vec<&String> = columns
        .iter()
        .filter(|c| schema.index_of(c).is_err())
        .collect();
    if !missing.is_empty() {
        return Err(PlanError::ColumnNotFound(format!(
            "weight expression references unknown edge column(s): {}",
            py_list(missing.into_iter())
        )));
    }
    let indices: Vec<usize> = columns
        .iter()
        .map(|c| schema.index_of(c).expect("checked above"))
        .collect();
    table
        .iter()
        .map(|b| b.project(&indices))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| PlanError::Engine(DataFusionError::ArrowError(Box::new(e), None)))
}

/// Remove the rows of `batches` (src/dst first) with a null endpoint; the number
/// removed.
fn drop_null_endpoints(batches: &mut [RecordBatch]) -> PlanResult<usize> {
    use arrow::compute::kernels::boolean::{and, is_not_null};
    let arrow = |e| PlanError::Engine(DataFusionError::ArrowError(Box::new(e), None));
    let mut dropped = 0;
    for b in batches.iter_mut() {
        let keep = and(
            &is_not_null(b.column(0)).map_err(arrow)?,
            &is_not_null(b.column(1)).map_err(arrow)?,
        )
        .map_err(arrow)?;
        let kept = arrow::compute::filter_record_batch(b, &keep).map_err(arrow)?;
        dropped += b.num_rows() - kept.num_rows();
        *b = kept;
    }
    Ok(dropped)
}

/// A scan's one path, or the error for a list of them (`msg`).
pub(crate) fn single_path<'a>(path: &'a Option<ScanPath>, msg: &str) -> PlanResult<&'a str> {
    match path {
        Some(ScanPath::One(p)) => Ok(p),
        _ => not_implemented(msg),
    }
}

pub(crate) const MULTI_PATH_EDGES: &str =
    "collect() over a scan_edges source supports a single string \
     path (glob included) for now, not a list of paths.";

pub(crate) const MULTI_PATH_NODES: &str =
    "collect() over a scan_nodes source supports a single string \
     path (glob included) for now, not a list of paths.";

/// The subgraph-view mask of a view over `index`, or `None` for the whole graph.
fn edge_mask(
    view: &EdgeView<'_>,
    index: &GraphIndex,
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<Option<Arc<EdgeMask>>> {
    view_mask(view, index, inputs, scratch)
}

// --- subgraph-view masks (#114, #116, #192) -----------------------------------

/// A view's mask over `index`: one bit per edge row, served from the index's
/// [`MaskCache`] under the view's fingerprint, computed on a miss.
fn view_mask(
    view: &EdgeView<'_>,
    index: &GraphIndex,
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<Option<Arc<EdgeMask>>> {
    if !view.has_mask() {
        return Ok(None);
    }
    let key = view_key(view, inputs, scratch)?;
    index
        .masks
        .get_or_build(&key, || build_mask(view, index, inputs, scratch))
        .map(Some)
}

/// The fingerprint of a view's mask: what it is a function of, given the rows of
/// its index.
fn view_key(view: &EdgeView<'_>, inputs: &dyn Inputs, scratch: &mut Scratch) -> PlanResult<String> {
    let roles = view_roles(view);
    let mut key = String::new();
    for step in &view.steps {
        let Plan::Filter { predicate: p, .. } = step else {
            key.push_str(&column_step_key(step));
            continue;
        };
        let resolved = match p {
            Expr::Scalar(v) => resolve_roles(v, &roles).unwrap_or_else(|_| v.clone()),
            Expr::Graph(_) => Value::from("graph"),
        };
        key.push_str(&format!("filter {resolved};"));
    }
    if let Some(Induced {
        traversal, input, ..
    }) = &view.induced_by
    {
        let inner = view_key(input, inputs, scratch)?;
        match traversal {
            Plan::Hop {
                n,
                direction,
                seeds: slot,
                ..
            } => {
                let set = scratch.seed_set(*slot, inputs)?;
                key.push_str(&format!(
                    "hop n={} direction={direction} seeds={} over [{inner}];",
                    n.0,
                    array_digest(set.as_ref())
                ));
            }
            Plan::ShortestPath {
                source,
                target,
                weight,
                direction,
                ..
            } => {
                let (s, t) = (
                    scratch.seed_set(*source, inputs)?,
                    scratch.seed_set(*target, inputs)?,
                );
                key.push_str(&format!(
                    "shortest_path direction={direction} source={} target={} weight={} over [{inner}];",
                    array_digest(s.as_ref()),
                    array_digest(t.as_ref()),
                    weight.as_ref().map(Value::to_string).unwrap_or_default()
                ));
            }
            _ => unreachable!("a traversal is a hop or a shortest_path"),
        }
    }
    Ok(key)
}

/// A content digest of an id array (int64 or string), for a fingerprint.
pub(crate) fn array_digest(a: &dyn Array) -> String {
    use std::hash::{Hash, Hasher};
    let strings = arrow::compute::cast(a, &arrow::datatypes::DataType::Utf8).ok();
    let mut parts = [
        std::collections::hash_map::DefaultHasher::new(),
        std::collections::hash_map::DefaultHasher::new(),
    ];
    for (salt, h) in parts.iter_mut().enumerate() {
        salt.hash(h);
        a.data_type().to_string().hash(h);
        a.len().hash(h);
        match strings
            .as_ref()
            .map(|s| s.as_any().downcast_ref::<arrow::array::StringArray>())
        {
            Some(Some(s)) => s.iter().for_each(|v| v.hash(h)),
            _ => format!("{a:?}").hash(h),
        }
    }
    format!("{:016x}{:016x}", parts[0].finish(), parts[1].finish())
}

/// The role map of a view's rows.
fn view_roles(view: &EdgeView<'_>) -> [(&'static str, String); 2] {
    [("src", view.roles.0.clone()), ("dst", view.roles.1.clone())]
}

/// Compute a view's mask (uncached).
fn build_mask(
    view: &EdgeView<'_>,
    index: &GraphIndex,
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<EdgeMask> {
    let mut mask: Option<EdgeMask> = None;
    if let Some(Induced {
        traversal, input, ..
    }) = &view.induced_by
    {
        // The reached node set, found over the input's own view, induces the
        // subgraph: an edge is kept iff both its endpoints are reached (#116).
        let within = view_mask(input, index, inputs, scratch)?;
        let reached = reached_nodes(traversal, input, index, within.as_deref(), inputs, scratch)?;
        mask = Some(EdgeMask::induced(&index.topo, &reached, within.as_deref()));
    }
    if !view.predicates.is_empty() {
        let m = predicate_mask(view, index.topo.n_edges(), inputs, scratch)?;
        mask = Some(match mask {
            Some(t) => t.intersect(&m),
            None => m,
        });
    }
    Ok(mask.expect("a view with a mask has a traversal or a filter"))
}

/// The nodes a traversal reaches over `within` (one flag per dense node): a hop's
/// region, seeds included, or the nodes on a shortest path.
fn reached_nodes(
    traversal: &Plan,
    edges: &EdgeView<'_>,
    index: &GraphIndex,
    within: Option<&EdgeMask>,
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<Vec<bool>> {
    let (topo, ids) = (&index.topo, &index.ids);
    let mut reached = vec![false; topo.n_nodes()];
    match traversal {
        Plan::Hop {
            n,
            direction,
            seeds: slot,
            ..
        } => {
            let set = scratch.seed_set(*slot, inputs)?;
            let dense: Vec<u32> = resolve_dense(ids, set.as_ref())?
                .into_iter()
                .flatten()
                .collect();
            let n = n.u32("n")?;
            let dir: ursa_core::Direction = parse_direction(direction)?.into();
            reached = ursa_core::algo::k_hop_reached_masked(topo, within, &dense, n, dir);
        }
        Plan::ShortestPath {
            source,
            target,
            weight,
            direction,
            ..
        } => {
            let s = scratch.seed_set(*source, inputs)?;
            let t = scratch.seed_set(*target, inputs)?;
            let weights = match weight {
                None => None,
                Some(w) => {
                    check_literals(w, "weight")?;
                    let cols = value_columns(w, &[]);
                    let attr = if cols.is_empty() {
                        None
                    } else {
                        prepare_weighted(edges, &cols, &[], inputs, scratch)?
                    };
                    Some(
                        attr.map(|a| (w.to_string(), a))
                            .unwrap_or((w.to_string(), vec![])),
                    )
                }
            };
            let dir: ursa_core::Direction = parse_direction(direction)?.into();
            let s = resolve_dense(ids, s.as_ref())?.into_iter().next().flatten();
            let t = resolve_dense(ids, t.as_ref())?.into_iter().next().flatten();
            let path = match (s, t) {
                (Some(s), Some(t)) => match weights {
                    None => ursa_core::algo::shortest_path_masked(topo, within, s, t, dir),
                    Some((json, attr)) => {
                        let attr = (!attr.is_empty()).then_some(attr);
                        let w = edge_weights(
                            attr.as_deref(),
                            &json,
                            topo.n_edges(),
                            "weighted shortest_path",
                        )?;
                        ursa_core::algo::shortest_path_weighted_with_cost_masked(
                            topo, within, &w, s, t, dir,
                        )
                        .map(|(p, _)| p)
                    }
                },
                _ => None,
            };
            for v in path.unwrap_or_default() {
                reached[v as usize] = true;
            }
        }
        _ => unreachable!("a traversal is a hop or a shortest_path"),
    }
    Ok(reached)
}

/// The mask of a view's filters over its topology's edge rows (#114): each
/// predicate evaluated over those rows, in index order, ANDed. A filter written
/// after a column step reads the columns that step leaves.
fn predicate_mask(
    view: &EdgeView<'_>,
    n_edges: usize,
    inputs: &dyn Inputs,
    scratch: &mut Scratch,
) -> PlanResult<EdgeMask> {
    let roles = view_roles(view);
    debug_assert!(view.sourced, "a filter view is over a source's rows");
    let flags = if view.has_column_steps() {
        let mut rows = topology_rows(view, None, inputs, scratch)?;
        let mut flags: Option<arrow::array::BooleanArray> = None;
        for step in &view.steps {
            let Plan::Filter { predicate, .. } = step else {
                rows = column_step(rows, step, &roles, inputs)?;
                continue;
            };
            let compiled = host_predicate(predicate, &roles, &rows[0].schema())?;
            let keep = crate::eval::keep_flags(&rows, &[compiled])?;
            flags = Some(match flags {
                Some(f) => arrow::compute::and(&f, &keep)?,
                None => keep,
            });
        }
        flags.expect("a view with a predicate has a filter step")
    } else {
        // Only what the predicates read: the index build already checked the
        // endpoints, and a scan reads them anyway to drop null-endpoint rows.
        let mut needed: BTreeSet<String> = BTreeSet::new();
        for p in &view.predicates {
            needed.extend(expr_columns(p, &roles));
        }
        let needed: Vec<String> = needed.into_iter().collect();
        let rows = topology_rows(view, Some(&needed), inputs, scratch)?;
        let schema = rows[0].schema();
        let mut compiled = Vec::with_capacity(view.predicates.len());
        for p in &view.predicates {
            compiled.push(host_predicate(p, &roles, &schema)?);
        }
        crate::eval::keep_flags(&rows, &compiled)?
    };
    if flags.len() != n_edges {
        return Err(PlanError::Value(format!(
            "edge_mask length ({}) does not match the edge count ({n_edges})",
            flags.len()
        )));
    }
    Ok(EdgeMask::from_boolean_array(&flags))
}

/// The rows of an edge source (`base`, a frame's first step) in index order:
/// the in-memory table, or a scan reading at least `columns` (`None`: all).
fn edge_rows(
    base: &Plan,
    frame: Slot,
    columns: Option<&[String]>,
    inputs: &dyn Inputs,
) -> PlanResult<Vec<RecordBatch>> {
    match base {
        Plan::FromArrow { .. } => match inputs.table(frame)? {
            Some(rows) => rows.into_batches(inputs),
            None => not_implemented(NO_EDGE_SOURCE),
        },
        Plan::ScanEdges {
            path,
            src,
            dst,
            on_null,
            ..
        } => {
            let path = single_path(path, MULTI_PATH_EDGES)?;
            let opts = inputs.storage_options(frame)?;
            let rows = scan_edges_rows(
                path,
                src,
                dst,
                &opts,
                columns.unwrap_or(&[]),
                &[],
                on_null == "drop",
            )?;
            if rows.dropped > 0 {
                inputs.warn_dropped(rows.dropped)?;
            }
            Ok(rows.batches)
        }
        _ => not_implemented(NO_EDGE_SOURCE),
    }
}

pub(crate) const NO_EDGE_SOURCE: &str =
    "collect() needs edges from ur.from_arrow(...), ur.from_polars(...), \
     or ur.scan_edges(<.parquet|.csv>); this frame has no resolvable source.";

/// The rows of a node source frame: its in-memory table, or its scan (through the
/// frame's [`NodeTableCache`]) reading at least `columns` (`None`: all).
fn node_rows(
    chain: &[&Plan],
    frame: Slot,
    columns: Option<&[String]>,
    inputs: &dyn Inputs,
) -> PlanResult<Rows> {
    match chain[0] {
        Plan::ScanNodes { path, id, .. } => {
            let path = single_path(path, MULTI_PATH_NODES)?;
            let opts = inputs.storage_options(frame)?;
            let cache = inputs.node_cache(frame)?;
            Ok(Rows::Batches(cache.read(path, id, &opts, columns)?))
        }
        _ => inputs
            .table(frame)?
            .ok_or_else(|| PlanError::NotImplemented(NO_EDGE_SOURCE.into())),
    }
}

/// A random walk and the steps written after it.
fn random_walk_frame(chain: &[&Plan], walk: &Plan, inputs: &dyn Inputs) -> PlanResult<Built> {
    let Plan::RandomWalk {
        edges,
        start,
        steps: n_steps,
        walks_per_node,
        seed,
    } = walk
    else {
        unreachable!("random_walk_frame takes a random_walk step")
    };
    let mut steps: Vec<&Plan> = vec![];
    for &s in chain {
        let op = s.op();
        if matches!(op, "random_walk" | "nodes" | "from_arrow" | "scan_nodes") {
            continue;
        }
        match s {
            Plan::Join { how, .. } => check_join_how(how)?,
            _ if is_tail(s) => {}
            _ => {
                return not_implemented(format!(
                    "collect() does not yet support the '{op}' step after a random_walk."
                ))
            }
        }
        check_step(s, inputs)?;
        steps.push(s);
    }
    let view = edges.view()?;
    let mut scratch = Scratch::default();
    let index = view_index(&view, inputs, &mut scratch)?;
    let starts = inputs.seeds(*start)?;
    // A walk over a subgraph view follows only its edges (#192).
    let mask = edge_mask(&view, &index, inputs, &mut scratch)?;
    let ctx = crate::lower::session();
    let leaf = walk_leaf(
        &ctx,
        index.topo,
        index.ids,
        mask,
        starts.as_ref(),
        n_steps.u32("steps")?,
        walks_per_node.u32("walks_per_node")?,
        seed_arg(seed, "seed")?,
    )?;
    let frame = Frame::from_ordered(ctx, leaf);
    // Walk output columns are (walk_id, step, node); no role applies.
    Ok(Built::frame(lower_node_steps(
        frame,
        &steps,
        &[],
        None,
        inputs,
    )?))
}

// --- edge frames ---------------------------------------------------------------

/// An edge frame: a plain edge frame, or a `hop` / `shortest_path` result, then
/// its steps in the order written (joins among them).
fn edge_frame(chain: &[&Plan], inputs: &dyn Inputs) -> PlanResult<Built> {
    let (src_col, dst_col) = edge_roles(chain);
    let mut traversal: Option<&Plan> = None;
    let mut steps: Vec<&Plan> = vec![];

    for &step in chain {
        let op = step.op();
        match step {
            Plan::Hop { .. } | Plan::ShortestPath { .. } => traversal = Some(step),
            Plan::ScanEdges { .. } | Plan::FromArrow { .. } | Plan::Nodes { .. } => {}
            Plan::Join { how, .. } => {
                check_join_how(how)?;
                check_step(step, inputs)?;
                steps.push(step);
            }
            Plan::Reverse { .. } => {
                // After a traversal its semantics are undesigned.
                if traversal.is_some() {
                    return not_implemented(
                        "reverse() after a traversal (hop/shortest_path) is not supported; \
                         reverse the edges before the traversal.",
                    );
                }
            }
            _ if is_tail(step) => {
                check_step(step, inputs)?;
                steps.push(step);
            }
            // Scalar columns (#191); graph columns belong on `edges.nodes()`.
            Plan::WithColumns { columns, .. }
                if columns.iter().all(|c| matches!(c.expr, Expr::Scalar(_))) =>
            {
                steps.push(step);
            }
            _ => {
                return not_implemented(format!(
                    "collect() does not yet support the '{op}' step on an edge frame."
                ))
            }
        }
    }

    let Some(traversal) = traversal else {
        let roles = [("src", src_col), ("dst", dst_col)];
        return plain_edges_frame(chain, &steps, &roles, inputs);
    };

    let (Plan::Hop { edges, .. } | Plan::ShortestPath { edges, .. }) = traversal else {
        unreachable!("a traversal is a hop or a shortest_path")
    };
    // A traversal over a traversal result has no topology to run on: the inner
    // result's edges are synthetic (seed, reached) pairs, not the parent graph.
    if edges.traversal().is_some() {
        return not_implemented(
            "chaining a traversal (hop/shortest_path) off a traversal result isn't \
             supported — the inner result's edges are synthetic reached-pairs, not the \
             parent graph. Collect it and re-ingest via ur.from_arrow(...) to traverse \
             further.",
        );
    }

    // A traversal result carries reserved `src`/`dst` columns.
    let roles = [("src", "src".to_string()), ("dst", "dst".to_string())];
    let ctx = crate::lower::session();
    let frame = match traversal {
        Plan::Hop {
            edges,
            n,
            direction,
            seeds,
        } => {
            let view = edges.view()?;
            let mut scratch = Scratch::default();
            let index = view_index(&view, inputs, &mut scratch)?;
            let seeds = inputs.seeds(*seeds)?;
            // A traversal over a subgraph view follows only its edges (#192).
            let mask = edge_mask(&view, &index, inputs, &mut scratch)?;
            let leaf = hop_leaf(
                &ctx,
                index.topo,
                index.ids,
                mask,
                seeds.as_ref(),
                n.u32("n")?,
                direction,
            )?;
            Frame::from_ordered(ctx, leaf)
        }
        Plan::ShortestPath {
            edges,
            source,
            target,
            weight,
            direction,
        } => {
            // A weight= expression over edge columns selects weighted Dijkstra.
            // Its columns are read first: for a scan with no index yet, that read
            // builds the index too.
            let mut edge_attr = None;
            if let Some(w) = weight {
                check_literals(w, "weight")?;
            }
            let weight_json = weight.as_ref().map(Value::to_string);
            let view = edges.view()?;
            let mut scratch = Scratch::default();
            if let Some(w) = weight {
                let cols: BTreeSet<String> = value_columns(w, &[]);
                if !cols.is_empty() {
                    edge_attr = prepare_weighted(&view, &cols, &[], inputs, &mut scratch)?;
                }
            }
            let index = view_index(&view, inputs, &mut scratch)?;
            let source = inputs.seeds(*source)?;
            let target = inputs.seeds(*target)?;
            let mask = edge_mask(&view, &index, inputs, &mut scratch)?;
            path_leaf(
                &ctx,
                index.topo,
                index.ids,
                mask,
                source.as_ref(),
                target.as_ref(),
                direction,
                weight_json.as_deref(),
                edge_attr,
            )?
        }
        _ => unreachable!("a traversal is a hop or a shortest_path"),
    };
    Ok(Built::frame(lower_node_steps(
        frame, &steps, &roles, None, inputs,
    )?))
}

/// A plain edge source and its steps. A scan reads the columns the steps need and
/// takes its leading filters with it (#192); the other steps are lowered on its
/// rows, in order.
fn plain_edges_frame(
    chain: &[&Plan],
    steps: &[&Plan],
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<Built> {
    // The endpoints plus what the steps read before a select or group_by narrows
    // the columns.
    let required: BTreeSet<String> = roles.iter().map(|(_, c)| c.clone()).collect();
    let needed = projection(steps, required, roles);
    let Some(slot) = source_slot(chain) else {
        return not_implemented(NO_EDGE_SOURCE);
    };
    let (rows, steps) = match chain[0] {
        Plan::ScanEdges {
            path,
            src,
            dst,
            on_null,
            ..
        } => {
            let path = single_path(path, MULTI_PATH_EDGES)?;
            let opts = inputs.storage_options(slot)?;
            let drop_nulls = on_null == "drop";
            // The leading filters go into the scan, unless `on_null="drop"` must
            // count the null rows of the whole file first.
            let mut pushed: Vec<Value> = vec![];
            if !drop_nulls {
                for step in steps {
                    match pushable(step, roles) {
                        Some(v) => pushed.push(v),
                        None => break,
                    }
                }
            }
            let (rows, n_pushed) = crate::scan::scan_edges_rows_pushing(
                path,
                src,
                dst,
                &opts,
                needed.as_deref().unwrap_or(&[]),
                &pushed,
                drop_nulls,
            )?;
            if rows.dropped > 0 {
                inputs.warn_dropped(rows.dropped)?;
            }
            (Rows::Batches(rows.batches), &steps[n_pushed..])
        }
        _ => match inputs.table(slot)? {
            Some(table) => (table, steps),
            None => return not_implemented(NO_EDGE_SOURCE),
        },
    };
    if steps.is_empty() {
        return Ok(Built::Rows(rows));
    }
    let frame = Frame::from_batches(crate::lower::session(), rows.into_batches(inputs)?)?;
    let frame = lower_node_steps(frame, steps, roles, None, inputs)?;
    Ok(Built::frame(frame))
}

/// A step a scan can take into its read: a well-formed `filter`, roles resolved.
/// The scan pushes it only if it reads columns the scan reads.
fn pushable(step: &Plan, roles: &Roles) -> Option<Value> {
    let Plan::Filter {
        predicate: Expr::Scalar(v),
        invalid: false,
        ..
    } = step
    else {
        return None;
    };
    require_predicate(v).ok()?;
    host_check(v, roles, None).ok()?;
    resolve_roles(v, roles).ok()
}

// --- join --------------------------------------------------------------------

pub(crate) fn check_join_how(how: &str) -> PlanResult<()> {
    if How::parse(how).is_some() {
        return Ok(());
    }
    not_implemented(format!(
        "join how={} is not supported (use ['anti', 'full', 'inner', 'left', 'outer', \
         'right', 'semi']).",
        py_str(how)
    ))
}

/// Join `left` with the join step's right side: build the right side as a frame
/// of its own, check the keys and column collisions, and join the two in one
/// plan ([`crate::lower::join`]).
fn run_join(left: Frame, join: &Plan, inputs: &dyn Inputs) -> PlanResult<Frame> {
    let Plan::Join { right, on, how, .. } = join else {
        unreachable!("run_join takes a join step")
    };
    let how = How::parse(how).expect("checked by check_join_how");
    // The right side's errors name it, building it or running it.
    let wrap = |e: PlanError| match e {
        PlanError::NotImplemented(_)
        | PlanError::Wrapped { .. }
        | PlanError::Host(HostError {
            not_implemented: true,
            ..
        }) => PlanError::Wrapped {
            message: format!("join(other): could not materialize the right-hand frame ({e})."),
            cause: Box::new(e),
        },
        e => e,
    };
    // In the order each always ran: the left rows, then the right side, then
    // the keys.
    let mut left = left;
    left.materialize()?;
    let mut right = frame_of(right, inputs).map_err(wrap)?;
    right.materialize().map_err(|e| wrap(e.into()))?;
    let (left_cols, right_cols) = (left.names(), right.names());
    for k in on {
        if !left_cols.contains(k) {
            return Err(PlanError::ColumnNotFound(format!(
                "join(on=): key {} not in the left frame {}.",
                py_str(k),
                py_list(left_cols.iter())
            )));
        }
        if !right_cols.contains(k) {
            return Err(PlanError::ColumnNotFound(format!(
                "join(on=): key {} not in the right frame {}.",
                py_str(k),
                py_list(right_cols.iter())
            )));
        }
    }
    // A same-named non-key column on both sides would collide in the output
    // (`semi` and `anti` keep the left columns only).
    let right_set: HashSet<&String> = right_cols.iter().filter(|c| !on.contains(c)).collect();
    let collide: BTreeSet<&String> = left_cols
        .iter()
        .filter(|c| how.keeps_right() && !on.contains(c) && right_set.contains(c))
        .collect();
    if !collide.is_empty() {
        return Err(PlanError::Value(format!(
            "join produces duplicate column name(s) {}; rename() one side before joining.",
            py_list(collide.into_iter())
        )));
    }
    if how == How::Right || how == How::Outer {
        if let Some((k, lt, rt)) = crate::lower::unfit_right_key(&left, &mut right, on)? {
            return Err(PlanError::Value(format!(
                "join(on=): key {} keeps the left frame's type ({lt}), which can't hold \
                 every value of the right frame's key ({rt}); cast one side first.",
                py_str(&k)
            )));
        }
    }
    Ok(crate::lower::join(left, right, on, how)?)
}

// --- relational steps --------------------------------------------------------

/// Lower relational `steps` onto `frame`, in the order written (#191). `roles`
/// resolves `ur.src()` / `ur.dst()` / `ur.id()` in their expressions.
fn lower_steps(
    frame: &mut Frame,
    steps: &[&Plan],
    roles: &Roles,
    inputs: &dyn Inputs,
) -> PlanResult<()> {
    let mut i = 0;
    while i < steps.len() {
        let step = steps[i];
        if step.step().is_some() && step.invalid() {
            return Err(inputs.step_error(step.step().expect("checked")));
        }
        match step {
            Plan::Filter { predicate, .. } => {
                let compiled = filter_expr(predicate, roles, frame)?;
                frame.filter(compiled)?;
            }
            Plan::Select { columns, .. } => select(frame, columns, roles)?,
            Plan::Rename { mapping, .. } => {
                rename_check(frame, mapping)?;
                if !mapping.is_empty() {
                    frame.rename(mapping)?;
                }
            }
            Plan::Sort { by, .. } => {
                let keys = sort_keys(frame, by, roles)?;
                // A head written right after the sort is its top-k.
                let limit = match steps.get(i + 1) {
                    Some(Plan::Head {
                        n, invalid: false, ..
                    }) => {
                        i += 1;
                        Some(head_n(n)?)
                    }
                    _ => None,
                };
                if !keys.is_empty() {
                    frame.sort(keys, limit)?;
                } else if let Some(n) = limit {
                    frame.head(n)?;
                }
            }
            Plan::Head { n, .. } => frame.head(head_n(n)?)?,
            Plan::Distinct { .. } => frame.distinct()?,
            Plan::Sample { .. } => {
                let handle = step
                    .step()
                    .ok_or_else(|| PlanError::Invalid("a sample step has no host handle".into()))?;
                frame.sample(|rows| inputs.sample(handle, rows))?;
            }
            Plan::GroupByAgg { keys, aggs, .. } => group_by(frame, keys, aggs)?,
            other => {
                return Err(PlanError::Invalid(format!(
                    "the '{}' step is not a relational step",
                    other.op()
                )))
            }
        }
        i += 1;
    }
    Ok(())
}

/// A `rename`'s mapping checked against `view`: the renamed column names.
pub(crate) fn rename_check(
    view: &dyn View,
    mapping: &[(String, String)],
) -> PlanResult<Vec<String>> {
    if let Some((old, _)) = mapping.iter().find(|(old, _)| !view.has_column(old)) {
        return Err(unknown_column("rename()", old, &view.names()));
    }
    let renamed: Vec<String> = view
        .names()
        .into_iter()
        .map(|n| {
            mapping
                .iter()
                .find(|(old, _)| *old == n)
                .map_or(n, |(_, new)| new.clone())
        })
        .collect();
    check_unique("rename()", renamed.iter())?;
    Ok(renamed)
}

/// A `group_by().agg()`'s columns checked against `view`.
pub(crate) fn group_by_check(view: &dyn View, keys: &[String], aggs: &[AggSpec]) -> PlanResult<()> {
    for k in keys {
        if !view.has_column(k) {
            return Err(unknown_column("group_by()", k, &view.names()));
        }
    }
    for a in aggs {
        if !view.has_column(&a.column) {
            return Err(unknown_column("agg()", &a.column, &view.names()));
        }
    }
    Ok(())
}

/// A `head` count: a negative one keeps no rows, as a slice of that length did.
fn head_n(n: &IntArg) -> PlanResult<usize> {
    Ok(match n.int("n")? {
        Some(i) if i < 0 => 0,
        Some(i) => usize::try_from(i).unwrap_or(usize::MAX),
        None => usize::MAX,
    })
}

/// A `filter` predicate over the frame: the checks and errors filters have
/// always had ([`host_check`]), then lowered with comparisons resolved.
fn filter_expr(predicate: &Expr, roles: &Roles, frame: &Frame) -> PlanResult<DfExpr> {
    Ok(filter_check(predicate, roles, frame)?.expect("a frame knows its types"))
}

/// [`filter_expr`] over any [`View`]: its checks, in order, and the lowered
/// predicate when the view knows its column types.
pub(crate) fn filter_check(
    predicate: &Expr,
    roles: &Roles,
    view: &dyn View,
) -> PlanResult<Option<DfExpr>> {
    let Expr::Scalar(v) = predicate else {
        return require_predicate(&Value::Null).map(|_| unreachable!());
    };
    require_predicate(v)?;
    host_check(v, roles, Some(&view.arrow()))?;
    let resolved = resolve_roles(v, roles)?;
    match view.typed() {
        Some(schema) => Ok(Some(crate::eval::logical(&resolved, schema)?)),
        None => {
            crate::expr::lower(&crate::expr::parse_ursa_expr(&resolved)?)?;
            Ok(None)
        }
    }
}

/// `select`: column names and expressions, in order. An expression's column is
/// named by its alias, or else by its first column (as in Polars).
fn select(frame: &mut Frame, items: &[SelectItem], roles: &Roles) -> PlanResult<()> {
    let exprs = select_check(frame, items, roles)?
        .into_iter()
        .map(|(_, e)| e.expect("a frame knows its types"))
        .collect();
    frame.project(exprs)?;
    Ok(())
}

/// A `select`'s items checked against `view`, in order: each output's name, and
/// its expression when the view knows its column types.
pub(crate) fn select_check(
    view: &dyn View,
    items: &[SelectItem],
    roles: &Roles,
) -> PlanResult<Vec<(String, Option<DfExpr>)>> {
    let missing: Vec<&String> = items
        .iter()
        .filter_map(|c| match c {
            SelectItem::Name(n) if !view.has_column(n) => Some(n),
            _ => None,
        })
        .collect();
    if let Some(c) = missing.first() {
        return Err(unknown_column("select()", c, &view.names()));
    }
    let typed = view.typed().is_some();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(match item {
            SelectItem::Name(n) => (n.clone(), typed.then(|| col_named(n))),
            SelectItem::Expr(v) => {
                let e = scalar_check(view, v, roles, "select()")?;
                let name = expr_name(&resolve_roles(v, roles)?);
                let e = e.map(|e| match e {
                    DfExpr::Alias(_) => e,
                    e => e.alias(name.clone()),
                });
                let name = match &e {
                    Some(DfExpr::Alias(a)) => a.name.clone(),
                    _ => name,
                };
                (name, e)
            }
        });
    }
    check_unique("select()", out.iter().map(|(n, _)| n))?;
    Ok(out)
}

/// A step's output keeps one column per name.
pub(crate) fn check_unique<'a>(
    step: &str,
    names: impl Iterator<Item = &'a String>,
) -> PlanResult<()> {
    let mut seen = BTreeSet::new();
    for n in names {
        if !seen.insert(n) {
            return Err(PlanError::Value(format!(
                "{step} would produce duplicate column {}.",
                py_str(n)
            )));
        }
    }
    Ok(())
}

/// The output name of an unaliased expression: its first column's, else
/// `literal`.
pub(crate) fn expr_name(v: &Value) -> String {
    match kind(v) {
        "col" => v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "alias" => v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "binary" => {
            let l = v.get("left").unwrap_or(&Value::Null);
            if value_columns(l, &[]).is_empty() {
                expr_name(v.get("right").unwrap_or(&Value::Null))
            } else {
                expr_name(l)
            }
        }
        "unary" | "agg" => expr_name(v.get("operand").unwrap_or(&Value::Null)),
        _ => "literal".to_string(),
    }
}

/// The keys of a `sort`, checked against the frame: a column (which must exist)
/// or an expression.
fn sort_keys(frame: &Frame, by: &[SortKey], roles: &Roles) -> PlanResult<Vec<(DfExpr, bool)>> {
    Ok(sort_check(frame, by, roles)?
        .into_iter()
        .map(|(e, d)| (e.expect("a frame knows its types"), d))
        .collect())
}

/// [`sort_keys`] over any [`View`].
pub(crate) fn sort_check(
    view: &dyn View,
    by: &[SortKey],
    roles: &Roles,
) -> PlanResult<Vec<(Option<DfExpr>, bool)>> {
    for k in by {
        if let Some(c) = &k.column {
            if !view.has_column(c) {
                return Err(unknown_column("sort()", c, &view.names()));
            }
        }
    }
    let typed = view.typed().is_some();
    by.iter()
        .map(|k| {
            let e = match (&k.column, &k.expr) {
                (Some(c), _) => typed.then(|| col_named(c)),
                (None, Some(v)) => scalar_check(view, v, roles, "sort()")?,
                (None, None) => {
                    return Err(PlanError::Invalid(
                        "a sort key needs a column or an expr".into(),
                    ))
                }
            };
            Ok((e, k.descending))
        })
        .collect()
}

/// `group_by(keys).agg(aggs)`, its columns checked as they always were.
fn group_by(frame: &mut Frame, keys: &[String], aggs: &[AggSpec]) -> PlanResult<()> {
    use datafusion::functions_aggregate::expr_fn::{avg, count, count_distinct, max, min, sum};
    group_by_check(frame, keys, aggs)?;
    use crate::agg::{float_agg, has_nulls, FloatAgg};
    // The rows, in order: a float sum's NaN rule depends on whether its column
    // holds a null.
    let rows = frame.ordered_batches()?;
    let row_schema = rows[0].schema();
    let schema = frame.schema();
    let mut exprs = Vec::with_capacity(aggs.len());
    let mut ordered = false;
    for a in aggs {
        // A dictionary (categorical) operand aggregates its values.
        let mut operand = col_named(&a.column);
        let mut dt = schema.field_with_name(&a.column)?.data_type().clone();
        if let DataType::Dictionary(_, value) = dt {
            dt = value.as_ref().clone();
            operand = cast_to(operand, dt.clone());
        }
        // Booleans and integers average in f64, folded in input order like
        // pyarrow's; a boolean sum counts the trues.
        let numeric = dt.is_integer() || dt == DataType::Boolean;
        if numeric && a.func == "mean" {
            operand = cast_to(operand, DataType::Float64);
            dt = DataType::Float64;
        } else if dt == DataType::Boolean && a.func == "sum" {
            operand = cast_to(operand, DataType::UInt64);
        }
        if dt.is_floating() {
            let kind = match a.func.as_str() {
                "sum" => Some(FloatAgg::Sum),
                "mean" => Some(FloatAgg::Mean),
                "min" => Some(FloatAgg::Min),
                "max" => Some(FloatAgg::Max),
                _ => None,
            };
            if let Some(kind) = kind {
                let nulls = has_nulls(&rows, row_schema.index_of(&a.column)?);
                exprs.push(float_agg(kind, &dt, nulls, operand).alias(&a.output));
                ordered = true;
                continue;
            }
        }
        let e = match a.func.as_str() {
            "mean" => avg(operand),
            "sum" => sum(operand),
            "min" => min(operand),
            "max" => max(operand),
            "count" => count(operand),
            // Floats count canonical (#215): one -0.0/0.0, one NaN.
            "n_unique" if dt.is_floating() => count_distinct(crate::keys::canonical_expr(operand)),
            "n_unique" => count_distinct(operand),
            other => return check_agg_fn(other),
        };
        exprs.push(e.alias(&a.output));
    }
    frame.group_by(keys.iter().map(|k| col_named(k)).collect(), exprs, ordered)?;
    Ok(())
}

fn cast_to(e: DfExpr, dt: DataType) -> DfExpr {
    DfExpr::Cast(datafusion::logical_expr::Cast::new(Box::new(e), dt))
}

/// Compile a filter predicate that runs outside an engine query against `schema`,
/// with the checks and errors the host's evaluator had (`_eval_pyarrow`).
fn host_predicate(
    predicate: &Expr,
    roles: &Roles,
    schema: &arrow::datatypes::SchemaRef,
) -> PlanResult<crate::eval::Compiled> {
    let Expr::Scalar(v) = predicate else {
        return require_predicate(&Value::Null).map(|_| unreachable!());
    };
    require_predicate(v)?;
    host_check(v, roles, Some(schema))?;
    Ok(crate::eval::compile(&resolve_roles(v, roles)?, schema)?)
}

/// The binary operators the host's evaluator supported.
const HOST_BINARY_OPS: [&str; 12] = [
    "+", "-", "*", "/", ">", ">=", "<", "<=", "==", "!=", "&", "|",
];

/// Walk a predicate the way the host's evaluator did, raising its errors in its
/// order: an unknown column (checked against `schema`, when given), a role not
/// available here, an operator or node it doesn't evaluate.
pub(crate) fn host_check(
    v: &Value,
    roles: &Roles,
    schema: Option<&arrow::datatypes::SchemaRef>,
) -> PlanResult<()> {
    let has = |name: &str| schema.is_none_or(|s| s.index_of(name).is_ok());
    let unknown = |name: &str| {
        let names: Vec<String> = schema
            .map(|s| s.fields().iter().map(|f| f.name().clone()).collect())
            .unwrap_or_default();
        Err(unknown_column("filter()", name, &names))
    };
    let k = kind(v);
    match k {
        "col" => {
            let name = v.get("name").and_then(Value::as_str).unwrap_or("");
            if !has(name) {
                return unknown(name);
            }
            Ok(())
        }
        "lit" => check_literals(v, "filter"),
        "src" | "dst" | "id" => {
            let Some((_, column)) = roles.iter().find(|(r, _)| *r == k) else {
                // The same message as an engine filter's.
                return resolve_roles(v, roles).map(|_| ());
            };
            if !has(column) {
                return unknown(column);
            }
            Ok(())
        }
        "binary" => {
            let op = v.get("op").and_then(Value::as_str).unwrap_or("");
            if !HOST_BINARY_OPS.contains(&op) {
                return not_implemented(format!(
                    "filter operator {} is not supported.",
                    py_str(op)
                ));
            }
            for side in ["left", "right"] {
                host_check(v.get(side).unwrap_or(&Value::Null), roles, schema)?;
            }
            Ok(())
        }
        "unary" => {
            let op = v.get("op").and_then(Value::as_str).unwrap_or("");
            if op != "~" {
                return not_implemented(format!(
                    "unary filter operator {} is not supported.",
                    py_str(op)
                ));
            }
            host_check(v.get("operand").unwrap_or(&Value::Null), roles, schema)
        }
        other => not_implemented(format!(
            "filter expression node {} is not supported (aggregations belong in group_by).",
            py_str(other)
        )),
    }
}

/// The columns a scan must read for `steps` (projection pushdown), or `None` to
/// read them all.
///
/// Walks the steps in written order, tracking renames back to source names and
/// the columns `with_columns` computes (which no file holds), up to the first
/// `select` or `group_by`, the first step that narrows the columns. A `distinct`
/// before that reads every column. Without a narrowing step every column reaches
/// the output. `required` is always read.
fn projection(steps: &[&Plan], required: BTreeSet<String>, roles: &Roles) -> Option<Vec<String>> {
    // current name -> source column (None: no source column)
    let mut source: HashMap<String, Option<String>> = HashMap::new();
    let mut needed = required;
    let src = |source: &HashMap<String, Option<String>>, names: Vec<String>| -> Vec<String> {
        names
            .into_iter()
            .filter_map(|n| source.get(&n).cloned().unwrap_or(Some(n)))
            .collect()
    };
    let scalar = |v: &Value| -> Vec<String> { value_columns(v, roles).into_iter().collect() };
    for step in steps {
        match step {
            Plan::Filter { predicate, .. } => {
                let cols = expr_columns(predicate, roles).into_iter().collect();
                needed.extend(src(&source, cols));
            }
            Plan::Sort { by, .. } => {
                for k in by {
                    let cols = match (&k.column, &k.expr) {
                        (Some(c), _) => vec![c.clone()],
                        (None, Some(v)) => scalar(v),
                        (None, None) => vec![],
                    };
                    needed.extend(src(&source, cols));
                }
            }
            Plan::WithColumns { columns, .. } => {
                for e in columns {
                    if let Expr::Scalar(v) = &e.expr {
                        needed.extend(src(&source, scalar(v)));
                    }
                }
                for e in columns {
                    source.insert(e.name.clone(), None); // computed, not read
                }
            }
            Plan::Rename { mapping, .. } => {
                // A rename reads its sources (an unknown one must still error).
                let olds: BTreeSet<String> = mapping.iter().map(|(o, _)| o.clone()).collect();
                needed.extend(src(&source, olds.into_iter().collect()));
                let relabelled: Vec<(String, Option<String>)> = mapping
                    .iter()
                    .map(|(old, new)| {
                        let s = source.get(old).cloned().unwrap_or(Some(old.clone()));
                        (new.clone(), s)
                    })
                    .collect();
                for (old, _) in mapping {
                    source.insert(old.clone(), None); // the old names are gone
                }
                source.extend(relabelled);
            }
            Plan::Select { columns, .. } => {
                for c in columns {
                    let cols = match c {
                        SelectItem::Name(n) => vec![n.clone()],
                        SelectItem::Expr(v) => scalar(v),
                    };
                    needed.extend(src(&source, cols));
                }
                return Some(needed.into_iter().collect());
            }
            Plan::GroupByAgg { keys, aggs, .. } => {
                let mut cols: BTreeSet<String> = keys.iter().cloned().collect();
                cols.extend(aggs.iter().map(|a| a.column.clone()));
                needed.extend(src(&source, cols.into_iter().collect()));
                return Some(needed.into_iter().collect());
            }
            Plan::Distinct { .. } | Plan::Join { .. } => return None,
            _ => {} // head / (position-based) sample read no column
        }
    }
    None
}

// --- expressions -------------------------------------------------------------

/// The operators valid at the top of a filter predicate.
const PREDICATE_OPS: [&str; 8] = [">", "<", ">=", "<=", "==", "!=", "&", "|"];

/// A step the host could not parse raises the host's error where the step is
/// read, as it did when the host parsed each step on reaching it.
fn check_step(step: &Plan, inputs: &dyn Inputs) -> PlanResult<()> {
    match step.step() {
        Some(handle) if step.invalid() => Err(inputs.step_error(handle)),
        _ => Ok(()),
    }
}

/// Reject a literal the host could not put in JSON (`{"kind": "lit",
/// "unrepresentable": <type>}`), with the error serializing it used to raise:
/// a `TypeError` for a value of another type, an engine JSON error for a
/// non-finite float. The host's own evaluator still reads the original value.
pub(crate) fn check_literals(v: &Value, what: &str) -> PlanResult<()> {
    if kind(v) == "lit" {
        if let Some(ty) = v.get("unrepresentable").and_then(Value::as_str) {
            if ty == "float" {
                return Err(PlanError::Engine(DataFusionError::Execution(format!(
                    "invalid {what} expression JSON: NaN and infinity have no JSON form"
                ))));
            }
            if let Some(reason) = v.get("reason").and_then(Value::as_str) {
                return Err(PlanError::Type(reason.to_string()));
            }
            return Err(PlanError::Type(format!(
                "Object of type {ty} is not JSON serializable"
            )));
        }
    }
    for key in ["left", "right", "operand"] {
        if let Some(e) = v.get(key) {
            check_literals(e, what)?;
        }
    }
    Ok(())
}

pub(crate) fn require_predicate(p: &Value) -> PlanResult<()> {
    let op = p.get("op").and_then(Value::as_str).unwrap_or("");
    let ok = match kind(p) {
        "binary" => PREDICATE_OPS.contains(&op),
        "unary" => op == "~",
        _ => false,
    };
    if ok {
        return Ok(());
    }
    not_implemented(
        "filter() expects a boolean predicate — a comparison (>, >=, <, <=, ==, !=) or a \
         boolean combination (&, |, ~), e.g. .filter(ur.col('deg') > 2) or \
         .filter((ur.col('a') > 1) & (ur.col('b') == 'x')).",
    )
}

/// An expression with `ur.src()` / `ur.dst()` / `ur.id()` resolved to the columns
/// `roles` names; a role not in `roles` is an error.
pub(crate) fn resolve_roles(v: &Value, roles: &Roles) -> PlanResult<Value> {
    let k = kind(v);
    let mut out = v.clone();
    match k {
        "binary" => {
            for side in ["left", "right"] {
                if let Some(e) = v.get(side) {
                    out[side] = resolve_roles(e, roles)?;
                }
            }
        }
        "unary" | "agg" | "alias" => {
            if let Some(e) = v.get("operand") {
                out["operand"] = resolve_roles(e, roles)?;
            }
        }
        "src" | "dst" | "id" => {
            let Some((_, column)) = roles.iter().find(|(r, _)| *r == k) else {
                let mut names: Vec<&str> = roles.iter().map(|(r, _)| *r).collect();
                names.sort_unstable();
                return not_implemented(format!(
                    "ur.{k}() is not available in this context (resolvable roles here: {}).",
                    py_list(names.into_iter())
                ));
            };
            return Ok(serde_json::json!({"kind": "col", "name": column}));
        }
        _ => {}
    }
    Ok(out)
}

/// The columns an expression reads, role references resolved through `roles`.
pub(crate) fn expr_columns(e: &Expr, roles: &Roles) -> BTreeSet<String> {
    match e {
        Expr::Scalar(v) => value_columns(v, roles),
        Expr::Graph(_) => BTreeSet::new(),
    }
}

pub(crate) fn value_columns(v: &Value, roles: &Roles) -> BTreeSet<String> {
    let k = kind(v);
    let mut out = BTreeSet::new();
    match k {
        "col" => {
            if let Some(name) = v.get("name").and_then(Value::as_str) {
                out.insert(name.to_string());
            }
        }
        "binary" => {
            for side in ["left", "right"] {
                if let Some(e) = v.get(side) {
                    out.extend(value_columns(e, roles));
                }
            }
        }
        "unary" | "agg" | "alias" => {
            if let Some(e) = v.get("operand") {
                out.extend(value_columns(e, roles));
            }
        }
        "src" | "dst" | "id" => {
            if let Some((_, column)) = roles.iter().find(|(r, _)| *r == k) {
                out.insert(column.clone());
            }
        }
        _ => {}
    }
    out
}

/// The error for a step that names a column the frame doesn't have (Python's
/// `ColumnNotFoundError`, which is also a `ValueError`), with the columns it has.
pub(crate) fn unknown_column(step: &str, name: &str, available: &[String]) -> PlanError {
    PlanError::ColumnNotFound(format!(
        "{step} references unknown column {}; the frame has {}.",
        py_str(name),
        py_list(available.iter())
    ))
}

// --- Python-compatible message formatting --------------------------------------
//
// The guards keep the messages the Python layer raised, which quote values with
// Python's `repr`.

/// `repr(s)` for a Python `str`.
pub(crate) fn py_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `repr(list_of_str)`.
pub(crate) fn py_list<S: AsRef<str>>(items: impl Iterator<Item = S>) -> String {
    let parts: Vec<String> = items.map(|s| py_str(s.as_ref())).collect();
    format!("[{}]", parts.join(", "))
}

/// `repr(value)` for a JSON-representable Python value.
pub(crate) fn py_value(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => py_str(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(_) => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_means_fold_in_order_for_any_partition_count() {
        use crate::lower::{session, Frame, TEST_PARTITIONS};
        use arrow::array::{Float64Array, Int64Array};
        use arrow::datatypes::{Field, Schema};
        // Large values: their f64 sum depends on the order they are added in.
        let n = 50_000usize;
        let k: Vec<i64> = (0..n).map(|i| (i * 7 % 3) as i64).collect();
        let v: Vec<i64> = (0..n)
            .map(|i| (1i64 << 58) + (i as i64).wrapping_mul(0x9E37_79B9_7F4A) % (3i64 << 58))
            .collect();
        let mut sums = [0.0f64; 3];
        let mut counts = [0usize; 3];
        for (g, x) in k.iter().zip(&v) {
            sums[*g as usize] += *x as f64;
            counts[*g as usize] += 1;
        }
        let want: Vec<u64> = (0..3)
            .map(|g| (sums[g] / counts[g] as f64).to_bits())
            .collect();
        let schema = Arc::new(Schema::new(vec![
            Field::new("k", DataType::Int64, false),
            Field::new("v", DataType::Int64, false),
        ]));
        let batches: Vec<RecordBatch> = (0..n)
            .step_by(4096)
            .map(|s| {
                let e = (s + 4096).min(n);
                RecordBatch::try_new(
                    schema.clone(),
                    vec![
                        Arc::new(Int64Array::from(k[s..e].to_vec())),
                        Arc::new(Int64Array::from(v[s..e].to_vec())),
                    ],
                )
                .unwrap()
            })
            .collect();
        let mean = AggSpec {
            output: "m".into(),
            func: "mean".into(),
            column: "v".into(),
        };
        for p in [1, 3, 8] {
            TEST_PARTITIONS.with(|c| c.set(Some(p)));
            let mut f = Frame::from_batches(session(), batches.clone()).unwrap();
            group_by(&mut f, &["k".to_string()], std::slice::from_ref(&mean)).unwrap();
            let out = f.collect().unwrap();
            TEST_PARTITIONS.with(|c| c.set(None));
            let mut got = vec![0u64; 3];
            for b in &out {
                let ks = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
                let ms = b.column(1).as_any().downcast_ref::<Float64Array>().unwrap();
                for r in 0..b.num_rows() {
                    got[ks.value(r) as usize] = ms.value(r).to_bits();
                }
            }
            assert_eq!(got, want, "p={p}");
        }
    }

    /// The rows of a grouped result as text, floats by their bits.
    fn grouped_rows(batches: &[RecordBatch]) -> Vec<String> {
        use arrow::array::{Float64Array, Int64Array, StringArray};
        let mut rows = vec![];
        for b in batches {
            for r in 0..b.num_rows() {
                let cells: Vec<String> = b
                    .columns()
                    .iter()
                    .map(|c| {
                        if c.is_null(r) {
                            return "null".to_string();
                        }
                        let any = c.as_any();
                        if let Some(a) = any.downcast_ref::<Float64Array>() {
                            format!("{:#x}", a.value(r).to_bits())
                        } else if let Some(a) = any.downcast_ref::<Int64Array>() {
                            a.value(r).to_string()
                        } else if let Some(a) = any.downcast_ref::<StringArray>() {
                            a.value(r).to_string()
                        } else {
                            arrow::util::display::array_value_to_string(c, r).unwrap()
                        }
                    })
                    .collect();
                rows.push(cells.join(","));
            }
        }
        rows
    }

    #[test]
    fn float_folds_through_the_parallel_partition_match_a_sequential_fold() {
        use crate::agg::{step, FloatAgg};
        use crate::lower::{session, Frame, TEST_PARTITIONS};
        use arrow::array::{Float64Array, Int64Array, StringArray};
        use arrow::datatypes::{Field, Schema};
        use std::collections::HashMap;
        let neg_nan = f64::from_bits(0xfff8_0000_0000_0000);
        let specials = [
            f64::NAN,
            neg_nan,
            0.0,
            -0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e16,
            -1e16,
            1.0,
        ];
        let n = 120_000usize;
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        // Keys: half the rows in one group, the rest over 7000 groups and a
        // null key; a second, string key splits some of them.
        let mut k1 = Vec::with_capacity(n);
        let mut k2 = Vec::with_capacity(n);
        let mut v = Vec::with_capacity(n);
        let mut w = Vec::with_capacity(n);
        for i in 0..n {
            let r = rand();
            k1.push(match r % 100 {
                0..=49 => Some(0i64),
                50 => None,
                _ => Some((r >> 8) as i64 % 7000),
            });
            k2.push(match (r >> 20) % 3 {
                0 => None,
                1 => Some("a"),
                _ => Some("é"),
            });
            let s = rand();
            let val = if s % 3 == 0 {
                specials[(s >> 8) as usize % specials.len()]
            } else {
                0.1 * i as f64 * if s % 2 == 0 { 1.0 } else { -1e3 }
            };
            v.push((s % 17 != 0).then_some(val));
            w.push(val);
        }
        let schema = Arc::new(Schema::new(vec![
            Field::new("k1", DataType::Int64, true),
            Field::new("k2", DataType::Utf8, true),
            Field::new("v", DataType::Float64, true),
            Field::new("w", DataType::Float64, false),
        ]));
        // Batches of uneven sizes, one over two chunks, splitting groups.
        let mut batches = vec![];
        let mut s = 0;
        for size in [1usize, 7, 4096, 20_000, 333, 8192, 8193].iter().cycle() {
            if s >= n {
                break;
            }
            let e = (s + size).min(n);
            batches.push(
                RecordBatch::try_new(
                    schema.clone(),
                    vec![
                        Arc::new(Int64Array::from(k1[s..e].to_vec())),
                        Arc::new(StringArray::from(k2[s..e].to_vec())),
                        Arc::new(Float64Array::from(v[s..e].to_vec())),
                        Arc::new(Float64Array::from(w[s..e].to_vec())),
                    ],
                )
                .unwrap(),
            );
            s = e;
        }
        let aggs: Vec<AggSpec> = [
            ("vs", "sum", "v"),
            ("vm", "mean", "v"),
            ("vmin", "min", "v"),
            ("vmax", "max", "v"),
            ("ws", "sum", "w"),
            ("wmax", "max", "w"),
            ("n", "count", "v"),
        ]
        .iter()
        .map(|(o, f, c)| AggSpec {
            output: o.to_string(),
            func: f.to_string(),
            column: c.to_string(),
        })
        .collect();
        // The reference: each group folded sequentially, in first-appearance order.
        type Key = (Option<i64>, Option<&'static str>);
        let folds = [
            (FloatAgg::Sum, true),
            (FloatAgg::Mean, true),
            (FloatAgg::Min, true),
            (FloatAgg::Max, true),
            (FloatAgg::Sum, false),
            (FloatAgg::Max, false),
        ];
        let mut order: Vec<Key> = vec![];
        let mut state: HashMap<Key, (Vec<Option<f64>>, u64, u64)> = HashMap::new();
        for i in 0..n {
            let key = (k1[i], k2[i]);
            let e = state.entry(key).or_insert_with(|| {
                order.push(key);
                (vec![None; folds.len()], 0, 0)
            });
            for (j, &(agg, nulls)) in folds.iter().enumerate() {
                let val = if nulls { v[i] } else { Some(w[i]) };
                if let Some(val) = val {
                    e.0[j] = Some(step(agg, nulls, e.0[j], val));
                }
            }
            if v[i].is_some() {
                e.1 += 1;
            }
            e.2 += 1;
        }
        let cell = |x: Option<f64>| x.map_or("null".to_string(), |f| format!("{:#x}", f.to_bits()));
        let want: Vec<String> = order
            .iter()
            .map(|key| {
                let (accs, nv, _) = &state[key];
                let mut cells = vec![
                    key.0.map_or("null".into(), |k| k.to_string()),
                    key.1.map_or("null".into(), |k| k.to_string()),
                ];
                for (j, &(agg, _)) in folds.iter().enumerate() {
                    let a = accs[j];
                    cells.push(cell(match agg {
                        FloatAgg::Mean => a.map(|s| s / *nv as f64),
                        _ => a,
                    }));
                }
                cells.push(nv.to_string());
                cells.join(",")
            })
            .collect();
        assert!(order.len() > 10_000, "{}", order.len());
        let cores = std::thread::available_parallelism().map_or(4, |c| c.get());
        for p in [1, 3, 8, cores] {
            TEST_PARTITIONS.with(|c| c.set(Some(p)));
            let mut f = Frame::from_batches(session(), batches.clone()).unwrap();
            group_by(&mut f, &["k1".to_string(), "k2".to_string()], &aggs).unwrap();
            let out = f.collect().unwrap();
            TEST_PARTITIONS.with(|c| c.set(None));
            let got = grouped_rows(&out);
            assert_eq!(got.len(), want.len(), "p={p}");
            for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                assert_eq!(g, w, "p={p}, group {i}");
            }
        }
    }

    #[test]
    fn the_ordered_aggregate_reads_the_parallel_partition_without_a_repartition() {
        use crate::lower::{session, Frame, TEST_PARTITIONS};
        use arrow::array::{Float64Array, Int64Array};
        use arrow::datatypes::{Field, Schema};
        let schema = Arc::new(Schema::new(vec![
            Field::new("k", DataType::Int64, false),
            Field::new("v", DataType::Float64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from_iter_values((0..20_000).map(|i| i % 97))),
                Arc::new(Float64Array::from_iter_values(
                    (0..20_000).map(|i| i as f64),
                )),
            ],
        )
        .unwrap();
        let sum = AggSpec {
            output: "s".into(),
            func: "sum".into(),
            column: "v".into(),
        };
        TEST_PARTITIONS.with(|c| c.set(Some(4)));
        let mut f = Frame::from_batches(session(), vec![batch]).unwrap();
        group_by(&mut f, &["k".to_string()], std::slice::from_ref(&sum)).unwrap();
        let plan = f.physical_plan_text().unwrap();
        TEST_PARTITIONS.with(|c| c.set(None));
        assert!(plan.contains("OrderedHashPartitionExec"), "{plan}");
        assert!(plan.contains("SinglePartitioned"), "{plan}");
        assert!(!plan.contains("RepartitionExec"), "{plan}");
    }

    fn specs(aggs: &[(&str, &str, &str)]) -> Vec<AggSpec> {
        aggs.iter()
            .map(|(o, f, c)| AggSpec {
                output: o.to_string(),
                func: f.to_string(),
                column: c.to_string(),
            })
            .collect()
    }

    /// `group_by(keys).agg(aggs)` over `batches` on `p` partitions: the result's
    /// schema and its rows (floats by their bits).
    fn grouped_on(
        p: usize,
        batches: &[RecordBatch],
        keys: &[&str],
        aggs: &[AggSpec],
    ) -> PlanResult<(arrow::datatypes::SchemaRef, Vec<String>)> {
        use crate::lower::{session, Frame, TEST_PARTITIONS};
        TEST_PARTITIONS.with(|c| c.set(Some(p)));
        let run = || -> PlanResult<Vec<RecordBatch>> {
            let mut f = Frame::from_batches(session(), batches.to_vec())?;
            let keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
            group_by(&mut f, &keys, aggs)?;
            Ok(f.collect()?)
        };
        let out = run();
        TEST_PARTITIONS.with(|c| c.set(None));
        let out = out?;
        Ok((out[0].schema(), grouped_rows(&out)))
    }

    /// Batches whose dictionary column `cat` (keys of type `K`) has a
    /// dictionary of its own per batch, `values` strings each, over `batches`
    /// batches of 8192 rows; `same` gives every batch equal (not shared) values.
    fn dictionary_batches<K: arrow::datatypes::ArrowDictionaryKeyType>(
        batches: usize,
        values: usize,
        same: bool,
    ) -> Vec<RecordBatch> {
        use arrow::array::{
            DictionaryArray, Float64Array, Int64Array, PrimitiveArray, StringArray,
        };
        use arrow::datatypes::{ArrowNativeType, Field, Schema};
        let n = 8192;
        let value_type = DataType::Dictionary(Box::new(K::DATA_TYPE), Box::new(DataType::Utf8));
        let schema = Arc::new(Schema::new(vec![
            Field::new("k", DataType::Int64, false),
            Field::new("cat", value_type, true),
            Field::new("v", DataType::Float64, false),
        ]));
        (0..batches)
            .map(|b| {
                let keys: PrimitiveArray<K> = (0..n)
                    .map(|i| {
                        let key = (i * 7919 + b * 31) % values;
                        (i % 97 != 0).then(|| K::Native::from_usize(key).unwrap())
                    })
                    .collect();
                let names = StringArray::from_iter_values((0..values).map(|j| {
                    if same {
                        format!("v{j}")
                    } else {
                        format!("b{b}_{j}")
                    }
                }));
                let cat = DictionaryArray::<K>::try_new(keys, Arc::new(names)).unwrap();
                RecordBatch::try_new(
                    schema.clone(),
                    vec![
                        Arc::new(Int64Array::from_iter_values(
                            (0..n).map(|i| ((i * 13 + b) % 50) as i64),
                        )),
                        Arc::new(cat),
                        Arc::new(Float64Array::from_iter_values((0..n).map(|i| {
                            (i as f64 + 0.1) * if i % 2 == 0 { 1e9 } else { -1e-3 }
                        }))),
                    ],
                )
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn narrow_dictionaries_spanning_many_chunks_aggregate_on_any_partition_count() {
        use arrow::datatypes::{ArrowDictionaryKeyType, Int16Type, Int8Type, UInt8Type};
        fn check<K: ArrowDictionaryKeyType>(values: usize) {
            let cores = std::thread::available_parallelism().map_or(4, |c| c.get());
            for same in [false, true] {
                let batches = dictionary_batches::<K>(16, values, same);
                let cases: Vec<(Vec<&str>, Vec<AggSpec>)> = vec![
                    // The dictionary as an operand, alongside an ordered fold.
                    (
                        vec!["k"],
                        specs(&[("s", "sum", "v"), ("c", "count", "cat")]),
                    ),
                    (
                        vec!["k"],
                        specs(&[("m", "max", "v"), ("u", "n_unique", "cat")]),
                    ),
                    (vec!["k"], specs(&[("c", "count", "cat")])),
                    // The dictionary as a key, ordered and not.
                    (vec!["cat"], specs(&[("s", "sum", "v")])),
                    (vec!["cat"], specs(&[("n", "count", "v")])),
                    (vec!["cat", "k"], specs(&[("s", "mean", "v")])),
                ];
                for (keys, aggs) in &cases {
                    let want = grouped_on(1, &batches, keys, aggs).unwrap();
                    let key_type = want.0.field_with_name("cat").map(|f| f.data_type().clone());
                    if let Ok(DataType::Dictionary(k, _)) = &key_type {
                        // A key's dictionary narrows back where its values fit
                        // (each result batch carries its own dictionary): always
                        // when every batch has the same values.
                        let k = k.as_ref();
                        assert!(
                            *k == K::DATA_TYPE || (!same && *k == DataType::Int32),
                            "{k}"
                        );
                    }
                    for p in [3, 8, cores] {
                        let got = grouped_on(p, &batches, keys, aggs).unwrap_or_else(|e| {
                            panic!("{keys:?} {aggs:?} p={p} same={same} {}: {e}", K::DATA_TYPE)
                        });
                        assert_eq!(got.0, want.0, "{keys:?} {aggs:?} p={p} same={same}");
                        assert_eq!(got.1, want.1, "{keys:?} {aggs:?} p={p} same={same}");
                    }
                }
            }
        }
        // 16 dictionaries of 120 (int8) / 3000 (int16) values: more than the
        // key type indexes once concatenated.
        check::<Int8Type>(120);
        check::<UInt8Type>(250);
        check::<Int16Type>(3000);
    }

    #[test]
    fn distinct_over_a_narrow_dictionary_spanning_many_dictionaries() {
        use crate::lower::{session, Frame};
        use arrow::datatypes::Int8Type;
        for same in [false, true] {
            let batches = dictionary_batches::<Int8Type>(16, 120, same);
            let mut f = Frame::from_batches(session(), batches).unwrap();
            f.project(vec![col_named("cat")]).unwrap();
            f.distinct().unwrap();
            let out = f.collect().unwrap();
            let rows = grouped_rows(&out);
            // Every value once, and the null, in first-appearance order.
            assert_eq!(
                rows.len(),
                if same { 121 } else { 16 * 120 + 1 },
                "same={same}"
            );
            let unique: std::collections::BTreeSet<&String> = rows.iter().collect();
            assert_eq!(unique.len(), rows.len());
            let key = match out[0].schema().field(0).data_type() {
                DataType::Dictionary(k, _) => k.as_ref().clone(),
                other => panic!("{other}"),
            };
            assert_eq!(
                key,
                if same {
                    DataType::Int8
                } else {
                    DataType::Int32
                }
            );
        }
    }

    #[test]
    fn float_and_string_keys_group_alike_on_any_partition_count() {
        use arrow::array::{Float64Array, LargeStringArray, StringViewArray};
        use arrow::datatypes::{Field, Schema};
        let neg_nan = f64::from_bits(0xfff8_0000_0000_0000);
        let specials = [
            f64::NAN,
            neg_nan,
            0.0,
            -0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1.5,
        ];
        let n = 30_000usize;
        let schema = Arc::new(Schema::new(vec![
            Field::new("f", DataType::Float64, true),
            Field::new("ls", DataType::LargeUtf8, true),
            Field::new("sv", DataType::Utf8View, true),
            Field::new("v", DataType::Float64, false),
        ]));
        let word = |i: usize| match i % 5 {
            0 => None,
            1 => Some(String::new()),
            2 => Some("a".to_string()),
            3 => Some(format!("a long string value past twelve bytes {}", i % 11)),
            _ => Some(format!("k{}", i % 13)),
        };
        let batches: Vec<RecordBatch> = (0..n)
            .step_by(7000)
            .map(|s| {
                let e = (s + 7000).min(n);
                let r = s..e;
                RecordBatch::try_new(
                    schema.clone(),
                    vec![
                        Arc::new(Float64Array::from_iter(
                            r.clone()
                                .map(|i| (i % 29 != 0).then(|| specials[i * 31 % 7])),
                        )),
                        Arc::new(LargeStringArray::from_iter(r.clone().map(|i| word(i * 3)))),
                        Arc::new(StringViewArray::from_iter(r.clone().map(|i| word(i * 7)))),
                        Arc::new(Float64Array::from_iter_values(r.map(|i| {
                            (i as f64).sqrt() * if i % 3 == 0 { -1e12 } else { 1.0 }
                        }))),
                    ],
                )
                .unwrap()
            })
            .collect();
        let aggs = specs(&[
            ("s", "sum", "v"),
            ("m", "mean", "v"),
            ("x", "max", "v"),
            ("n", "count", "v"),
        ]);
        let cores = std::thread::available_parallelism().map_or(4, |c| c.get());
        for keys in [vec!["f"], vec!["ls"], vec!["sv"], vec!["f", "ls", "sv"]] {
            // One partition reads the rows unpartitioned, as before #204.
            let want = grouped_on(1, &batches, &keys, &aggs).unwrap();
            assert!(want.1.len() > 3, "{keys:?}");
            for p in [3, 8, cores] {
                let got = grouped_on(p, &batches, &keys, &aggs).unwrap();
                assert_eq!(got.0, want.0, "{keys:?} p={p}");
                assert_eq!(got.1, want.1, "{keys:?} p={p}");
            }
        }
        // The float key keeps NaN signs and both zeros as their own groups, as
        // the unpartitioned aggregate does, keyed by their bits.
        let (_, rows) = grouped_on(4, &batches, &["f"], &aggs).unwrap();
        let firsts: Vec<&str> = rows.iter().map(|r| r.split(',').next().unwrap()).collect();
        for x in specials {
            let bits = format!("{:#x}", x.to_bits());
            assert!(
                firsts.contains(&bits.as_str()) || x.is_nan(),
                "{bits} {firsts:?}"
            );
        }
    }

    #[test]
    fn python_reprs() {
        assert_eq!(py_str("a"), "'a'");
        assert_eq!(py_str("it's"), "\"it's\"");
        assert_eq!(py_str("a'\"b"), "'a\\'\"b'");
        assert_eq!(py_list(["dst", "src"].iter()), "['dst', 'src']");
        assert_eq!(py_list(Vec::<String>::new().iter()), "[]");
        assert_eq!(py_value(&serde_json::json!(null)), "None");
        assert_eq!(py_value(&serde_json::json!("x")), "'x'");
    }

    fn plan(json: serde_json::Value) -> Plan {
        Plan::from_json(&json.to_string()).unwrap()
    }

    fn src() -> serde_json::Value {
        serde_json::json!({"op": "from_arrow", "frame": 0, "id": "id"})
    }

    fn steps_of(p: &Plan) -> Vec<&Plan> {
        p.chain().into_iter().skip(1).collect()
    }

    fn col_pred(c: &str) -> serde_json::Value {
        serde_json::json!({"kind": "binary", "op": ">",
            "left": {"kind": "col", "name": c}, "right": {"kind": "lit", "value": 1}})
    }

    #[test]
    fn projection_reads_every_sort_key_through_a_rename() {
        // rename(c -> k).sort([k, v]).head(3).select(id): the scan reads c, id, v.
        let p = plan(
            serde_json::json!({"op": "select", "columns": ["id"], "input":
            {"op": "head", "n": 3, "input":
            {"op": "sort", "by": [{"column": "k", "descending": true}, {"column": "v"}], "input":
            {"op": "rename", "mapping": [["c", "k"]], "input": src()}}}}),
        );
        let steps = steps_of(&p);
        let got = projection(
            &steps,
            BTreeSet::from(["id".to_string()]),
            &[("id", "id".to_string())],
        );
        assert_eq!(got, Some(vec!["c".into(), "id".into(), "v".into()]));
    }

    #[test]
    fn int_args_convert_like_python_integers() {
        let arg = |v: serde_json::Value| IntArg(v);
        assert_eq!(arg(serde_json::json!(5)).u64("n").unwrap(), 5);
        assert_eq!(arg(serde_json::json!(true)).u32("n").unwrap(), 1);
        let msg = |e: PlanError| e.to_string();
        assert_eq!(
            msg(arg(serde_json::json!(-1)).u64("n").unwrap_err()),
            "can't convert negative int to unsigned"
        );
        assert_eq!(
            msg(arg(serde_json::json!(1e21)).u64("n").unwrap_err()),
            "int too big to convert"
        );
        assert_eq!(
            msg(arg(serde_json::json!(-1)).u32("n").unwrap_err()),
            "out of range integral type conversion attempted"
        );
        assert!(matches!(
            arg(serde_json::json!("x")).u64("seed"),
            Err(PlanError::Type(m)) if m == "argument 'seed': 'str' object cannot be interpreted as an integer"
        ));
        assert!(matches!(
            arg(serde_json::json!({"type": "Decimal"})).u64("seed"),
            Err(PlanError::Type(m)) if m.contains("'Decimal' object")
        ));
    }

    #[test]
    fn unparsed_graph_nodes_stay_scalar() {
        // The stub a graph verb serializes to as a predicate is not a GraphVerb;
        // it parses, so the filter can reject it with its own error.
        let p = plan(
            serde_json::json!({"op": "filter", "predicate": {"kind": "graph"},
            "input": src()}),
        );
        let Plan::Filter { predicate, .. } = &p else {
            unreachable!()
        };
        assert!(matches!(predicate, Expr::Scalar(_)));
        assert!(matches!(
            require_predicate(&serde_json::json!({"kind": "graph"})),
            Err(PlanError::NotImplemented(_))
        ));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err =
            Plan::from_json(r#"{"op": "from_arrow", "frame": 0, "id": "id", "x": 1}"#).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
        let err = Plan::from_json(
            r#"{"op": "with_columns", "input": {"op": "from_arrow", "frame": 0, "id": "id"},
                "columns": [{"name": "d", "expr": {"kind": "graph", "verb": "degree", "bogus": 1}}]}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn deep_plans_parse() {
        let mut json = src();
        for _ in 0..500 {
            json = serde_json::json!({"op": "head", "n": 1, "input": json});
        }
        assert_eq!(plan(json).chain().len(), 501);
    }

    #[test]
    fn edge_view_splits_topology_and_mask() {
        let edges = serde_json::json!({"op": "from_arrow", "frame": 0, "src": "s", "dst": "d"});
        let input: EdgeInput = serde_json::from_value(serde_json::json!({"frame": 1, "plan":
            {"op": "filter", "predicate": col_pred("w"), "input": {"op": "reverse", "input": edges}}}))
        .unwrap();
        let view = input.view().unwrap();
        assert_eq!(view.topology.op(), "reverse");
        assert_eq!(view.predicates.len(), 1);
        assert!(view.induced_by.is_none() && view.has_mask());

        let hop: EdgeInput = serde_json::from_value(serde_json::json!({"frame": 2, "plan":
            {"op": "hop", "n": 1, "direction": "out", "seeds": 0,
             "edges": {"frame": 0, "plan": edges}}}))
        .unwrap();
        let view = hop.view().unwrap();
        assert_eq!(view.topology.op(), "from_arrow");
        assert_eq!(view.induced_by.map(|i| i.traversal.op()), Some("hop"));

        // A reshaping step is the topology; the filter and column steps above it
        // keep its rows, and the filter is the mask (#191).
        let reshaped: EdgeInput = serde_json::from_value(serde_json::json!({"frame": 3, "plan":
            {"op": "rename", "mapping": [["w", "x"]], "input": {"op": "filter",
             "predicate": col_pred("w"), "input": {"op": "distinct",
             "input": {"op": "filter", "predicate": col_pred("w"), "input": edges}}}}}))
        .unwrap();
        let view = reshaped.view().unwrap();
        assert!(view.reshaped);
        assert_eq!(view.topology.op(), "distinct");
        let ops: Vec<&str> = view.steps.iter().map(|s| s.op()).collect();
        assert_eq!(ops, ["filter", "rename"]);
        assert_eq!(view.predicates.len(), 1);

        // A step that leaves no endpoint column is an error naming it.
        let dropped: EdgeInput = serde_json::from_value(serde_json::json!({"frame": 4, "plan":
            {"op": "select", "columns": ["s", "w"], "input": edges}}))
        .unwrap();
        let err = dropped.view().unwrap_err();
        assert!(matches!(err, PlanError::ColumnNotFound(_)), "{err}");
        assert!(err
            .to_string()
            .contains("dst column 'd', but select() removed it"));
    }

    // --- one float semantics for keys (#215) ---------------------------------

    /// Float key values: NaN of both signs and several payloads, ±0.0, ±inf,
    /// nulls and plain numbers.
    fn key_pool() -> Vec<Option<f64>> {
        vec![
            Some(-0.0),
            Some(1.5),
            Some(0.0),
            Some(f64::NAN),
            Some(f64::from_bits(0xfff8_0000_0000_0001)),
            None,
            Some(f64::from_bits(0x7ff0_0000_0000_0005)),
            Some(f64::INFINITY),
            Some(f64::NEG_INFINITY),
            Some(f64::from_bits(0xfff8_0000_0000_0000)),
            Some(-1.5),
            Some(f64::from_bits(0x7ff8_dead_beef_0000)),
        ]
    }

    /// The class a float key compares as: canonical bits, `None` for null.
    fn key_class(v: Option<f64>) -> Option<u64> {
        v.map(|x| if x.is_nan() { f64::NAN } else { x + 0.0 }.to_bits())
    }

    /// `n` rows of `(k, s, w, v)`: `k` and `w` drawn from [`key_pool`] (so a
    /// group's first value varies), `s` a second key, `v` an integer-valued
    /// float, cut into uneven batches; `k` cast to `key_type`.
    fn float_key_batches(n: usize, key_type: &DataType) -> Vec<RecordBatch> {
        use arrow::array::{Float64Array, StringArray};
        let pool = key_pool();
        let pick = |i: usize, m: usize| pool[(i.wrapping_mul(2_654_435_761) >> 7) % m];
        let k: Vec<Option<f64>> = (0..n).map(|i| pick(i, pool.len())).collect();
        let w: Vec<Option<f64>> = (0..n).map(|i| pick(i * 31 + 5, pool.len())).collect();
        let s: Vec<String> = (0..n).map(|i| format!("s{}", (i * 7) % 3)).collect();
        let v: Vec<f64> = (0..n).map(|i| (i % 101) as f64).collect();
        let schema = Arc::new(arrow::datatypes::Schema::new(vec![
            arrow::datatypes::Field::new("k", key_type.clone(), true),
            arrow::datatypes::Field::new("s", DataType::Utf8, false),
            arrow::datatypes::Field::new("w", DataType::Float64, true),
            arrow::datatypes::Field::new("v", DataType::Float64, false),
        ]));
        let mut out = vec![];
        let mut at = 0;
        for size in [1usize, 7, 4096, 9000, 333, 8193, usize::MAX] {
            let end = n.min(at.saturating_add(size));
            if end <= at {
                break;
            }
            let k64: ArrayRef = Arc::new(Float64Array::from(k[at..end].to_vec()));
            let cols: Vec<ArrayRef> = vec![
                arrow::compute::cast(&k64, key_type).unwrap(),
                Arc::new(StringArray::from(s[at..end].to_vec())),
                Arc::new(Float64Array::from(w[at..end].to_vec())),
                Arc::new(Float64Array::from(v[at..end].to_vec())),
            ];
            out.push(RecordBatch::try_new(schema.clone(), cols).unwrap());
            at = end;
        }
        out
    }

    /// Column `name` of `batches` as `float64` (each value by its bits), with
    /// nulls.
    fn as_f64(batches: &[RecordBatch], name: &str) -> Vec<Option<u64>> {
        use arrow::array::Float64Array;
        batches
            .iter()
            .flat_map(|b| {
                let c = arrow::compute::cast(b.column_by_name(name).unwrap(), &DataType::Float64)
                    .unwrap();
                let c = c.as_any().downcast_ref::<Float64Array>().unwrap().clone();
                (0..c.len())
                    .map(|i| c.is_valid(i).then(|| c.value(i).to_bits()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn strings(batches: &[RecordBatch], name: &str) -> Vec<Option<String>> {
        batches
            .iter()
            .flat_map(|b| {
                let c = b.column_by_name(name).unwrap();
                (0..c.len())
                    .map(|i| {
                        c.is_valid(i)
                            .then(|| arrow::util::display::array_value_to_string(c, i).unwrap())
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn class_of_bits(b: Option<u64>) -> Option<u64> {
        key_class(b.map(f64::from_bits))
    }

    fn float_key_types() -> Vec<DataType> {
        vec![
            DataType::Float64,
            DataType::Float32,
            DataType::Float16,
            DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Float64)),
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Float32)),
        ]
    }

    #[test]
    fn float_keys_group_in_total_order_on_any_partition_count() {
        use crate::lower::{session, Frame, TEST_PARTITIONS};
        let n = 30_000;
        let aggs = specs(&[
            ("n", "count", "v"),
            ("t", "sum", "v"),
            ("u", "n_unique", "w"),
        ]);
        for key_type in float_key_types() {
            let batches = float_key_batches(n, &key_type);
            let k = as_f64(&batches, "k");
            let s = strings(&batches, "s");
            let w = as_f64(&batches, "w");
            let v: Vec<f64> = as_f64(&batches, "v")
                .into_iter()
                .map(|b| f64::from_bits(b.unwrap()))
                .collect();
            for multi in [false, true] {
                // The reference: groups by class in first-appearance order, each
                // showing its first row's key bits.
                type Group = (usize, usize, f64, std::collections::BTreeSet<Option<u64>>);
                let mut order: Vec<(Option<u64>, Option<String>)> = vec![];
                let mut groups: HashMap<(Option<u64>, Option<String>), Group> = HashMap::new();
                for r in 0..n {
                    let id = (class_of_bits(k[r]), multi.then(|| s[r].clone().unwrap()));
                    let g = groups.entry(id.clone()).or_insert_with(|| {
                        order.push(id.clone());
                        (r, 0, 0.0, Default::default())
                    });
                    g.1 += 1;
                    g.2 += v[r];
                    if w[r].is_some() {
                        g.3.insert(class_of_bits(w[r]));
                    }
                }
                let want: Vec<String> = order
                    .iter()
                    .map(|id| {
                        let (first, count, sum, uniq) = &groups[id];
                        format!(
                            "{:?},{:?},{count},{:#x},{}",
                            k[*first],
                            s[*first].as_ref().filter(|_| multi),
                            sum.to_bits(),
                            uniq.len()
                        )
                    })
                    .collect();
                for p in [1, 3, 8] {
                    TEST_PARTITIONS.with(|c| c.set(Some(p)));
                    let mut f = Frame::from_batches(session(), batches.clone()).unwrap();
                    let keys: Vec<String> = if multi {
                        vec!["k".into(), "s".into()]
                    } else {
                        vec!["k".into()]
                    };
                    group_by(&mut f, &keys, &aggs).unwrap();
                    let out = f.collect().unwrap();
                    TEST_PARTITIONS.with(|c| c.set(None));
                    let schema = out[0].schema();
                    assert_eq!(schema.field_with_name("k").unwrap().data_type(), &key_type);
                    let (gk, gs, gn, gt, gu) = (
                        as_f64(&out, "k"),
                        strings(&out, "s"),
                        strings(&out, "n"),
                        as_f64(&out, "t"),
                        strings(&out, "u"),
                    );
                    let got: Vec<String> = (0..gk.len())
                        .map(|i| {
                            format!(
                                "{:?},{:?},{},{:#x},{}",
                                gk[i],
                                if multi { gs[i].clone() } else { None },
                                gn[i].clone().unwrap(),
                                gt[i].unwrap(),
                                gu[i].clone().unwrap()
                            )
                        })
                        .collect();
                    assert_eq!(got, want, "key_type={key_type} multi={multi} p={p}");
                }
            }
        }
    }

    #[test]
    fn distinct_compares_every_float_column_in_total_order() {
        use crate::lower::{session, Frame, TEST_PARTITIONS};
        let n = 30_000;
        for key_type in float_key_types() {
            let batches = float_key_batches(n, &key_type);
            let (k, s, w) = (
                as_f64(&batches, "k"),
                strings(&batches, "s"),
                as_f64(&batches, "w"),
            );
            let mut seen = std::collections::HashSet::new();
            let want: Vec<String> = (0..n)
                .filter(|&r| seen.insert((class_of_bits(k[r]), s[r].clone(), class_of_bits(w[r]))))
                .map(|r| format!("{:?},{:?},{:?}", k[r], s[r], w[r]))
                .collect();
            for p in [1, 3, 8] {
                TEST_PARTITIONS.with(|c| c.set(Some(p)));
                let mut f = Frame::from_batches(session(), batches.clone()).unwrap();
                f.project(vec![col_named("k"), col_named("s"), col_named("w")])
                    .unwrap();
                f.distinct().unwrap();
                let out = f.collect().unwrap();
                TEST_PARTITIONS.with(|c| c.set(None));
                assert_eq!(
                    out[0]
                        .schema()
                        .fields()
                        .iter()
                        .map(|f| (f.name().clone(), f.data_type().clone()))
                        .collect::<Vec<_>>(),
                    vec![
                        ("k".to_string(), key_type.clone()),
                        ("s".to_string(), DataType::Utf8),
                        ("w".to_string(), DataType::Float64)
                    ]
                );
                let (gk, gs, gw) = (as_f64(&out, "k"), strings(&out, "s"), as_f64(&out, "w"));
                let got: Vec<String> = (0..gk.len())
                    .map(|i| format!("{:?},{:?},{:?}", gk[i], gs[i], gw[i]))
                    .collect();
                assert_eq!(got, want, "key_type={key_type} p={p}");
            }
        }
    }

    #[test]
    fn float_join_keys_match_in_total_order_for_every_how() {
        use crate::lower::{join, read_ordered, session, Frame, How, TEST_PARTITIONS};
        use arrow::array::Int64Array;
        let side = |n: usize, salt: usize, key_type: &DataType, value: &str| {
            let b = float_key_batches(n, key_type);
            let mut out = vec![];
            let mut at = 0i64;
            for b in b {
                let rows = b.num_rows() as i64;
                let vals: ArrayRef = Arc::new(Int64Array::from_iter_values(
                    (at..at + rows).map(|i| i * 10 + salt as i64),
                ));
                at += rows;
                let schema = Arc::new(arrow::datatypes::Schema::new(vec![
                    b.schema().field(0).clone(),
                    arrow::datatypes::Field::new(value, DataType::Int64, false),
                ]));
                out.push(RecordBatch::try_new(schema, vec![b.column(0).clone(), vals]).unwrap());
            }
            out
        };
        let hows = [
            How::Inner,
            How::Left,
            How::Right,
            How::Outer,
            How::Semi,
            How::Anti,
        ];
        for key_type in float_key_types() {
            let (lb, rb) = (side(60, 1, &key_type, "a"), side(25, 2, &key_type, "b"));
            let lk = as_f64(&lb, "k");
            let rk = as_f64(&rb, "k");
            let la: Vec<i64> = strings(&lb, "a")
                .iter()
                .map(|x| x.clone().unwrap().parse().unwrap())
                .collect();
            let rbv: Vec<i64> = strings(&rb, "b")
                .iter()
                .map(|x| x.clone().unwrap().parse().unwrap())
                .collect();
            let eq = |a: Option<u64>, b: Option<u64>| {
                a.is_some() && class_of_bits(a) == class_of_bits(b)
            };
            for how in hows {
                // The nested-loop reference: (key bits, a, b) per row, in the
                // order the join guarantees; each row keeps its own key.
                let mut want: Vec<(Option<u64>, Option<i64>, Option<i64>)> = vec![];
                if how == How::Right {
                    for (j, &k) in rk.iter().enumerate() {
                        let m: Vec<usize> = (0..lk.len()).filter(|&i| eq(lk[i], k)).collect();
                        if m.is_empty() {
                            want.push((k, None, Some(rbv[j])));
                        }
                        want.extend(m.iter().map(|&i| (lk[i], Some(la[i]), Some(rbv[j]))));
                    }
                } else {
                    for (i, &k) in lk.iter().enumerate() {
                        let m: Vec<usize> = (0..rk.len()).filter(|&j| eq(k, rk[j])).collect();
                        match how {
                            How::Semi | How::Anti => {
                                if m.is_empty() == (how == How::Anti) {
                                    want.push((k, Some(la[i]), None));
                                }
                            }
                            _ => {
                                if m.is_empty() && how != How::Inner {
                                    want.push((k, Some(la[i]), None));
                                }
                                want.extend(m.iter().map(|&j| (k, Some(la[i]), Some(rbv[j]))));
                            }
                        }
                    }
                    if how == How::Outer {
                        for (j, &k) in rk.iter().enumerate() {
                            if !lk.iter().any(|&l| eq(l, k)) {
                                want.push((k, None, Some(rbv[j])));
                            }
                        }
                    }
                }
                for p in [1, 3, 8] {
                    TEST_PARTITIONS.with(|c| c.set(Some(p)));
                    for lazy in [false, true] {
                        let ctx = session();
                        let frame = |b: &Vec<RecordBatch>| {
                            if lazy {
                                Frame::from_ordered(
                                    ctx.clone(),
                                    read_ordered(&ctx, b.clone()).unwrap(),
                                )
                            } else {
                                Frame::from_batches(ctx.clone(), b.clone()).unwrap()
                            }
                        };
                        let j = join(frame(&lb), frame(&rb), &["k".to_string()], how).unwrap();
                        let out = j.collect().unwrap();
                        assert_eq!(
                            out[0].schema().field_with_name("k").unwrap().data_type(),
                            &key_type
                        );
                        let gk = as_f64(&out, "k");
                        let ga = strings(&out, "a");
                        let gb = if how.keeps_right() {
                            strings(&out, "b")
                        } else {
                            vec![None; gk.len()]
                        };
                        let got: Vec<_> = (0..gk.len())
                            .map(|i| {
                                (
                                    gk[i],
                                    ga[i].as_ref().map(|x| x.parse().unwrap()),
                                    gb[i].as_ref().map(|x| x.parse().unwrap()),
                                )
                            })
                            .collect();
                        assert_eq!(
                            got, want,
                            "key_type={key_type} how={how:?} p={p} lazy={lazy}"
                        );
                    }
                    TEST_PARTITIONS.with(|c| c.set(None));
                }
            }
        }
    }

    #[test]
    fn float_keys_without_signed_zeros_or_odd_nans_group_as_before() {
        // Nothing to canonicalize: the plan is the one main ran, no hidden key.
        use crate::lower::{session, Frame};
        use arrow::array::Float64Array;
        let k: ArrayRef = Arc::new(Float64Array::from(vec![
            Some(1.0),
            Some(f64::NAN),
            None,
            Some(0.0),
            Some(1.0),
        ]));
        let schema = Arc::new(arrow::datatypes::Schema::new(vec![
            arrow::datatypes::Field::new("k", DataType::Float64, true),
        ]));
        let batches = vec![RecordBatch::try_new(schema, vec![k]).unwrap()];
        let mut f = Frame::from_batches(session(), batches).unwrap();
        group_by(&mut f, &["k".to_string()], &specs(&[("n", "count", "k")])).unwrap();
        let text = f.physical_plan_text().unwrap();
        assert!(!text.contains("ursa_first_key"), "{text}");
        let out = f.collect().unwrap();
        assert_eq!(
            grouped_rows(&out),
            vec![
                "0x3ff0000000000000,2",
                "0x7ff8000000000000,1",
                "null,0",
                "0x0,1"
            ]
        );
    }
}
