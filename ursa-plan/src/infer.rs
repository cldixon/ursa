//! Validating a frame as it is built (#193): what a collect of it would raise
//! that the plan alone can tell, raised by the step that adds the problem.
//!
//! Two checks run when a [`Node`] is built, in the order a collect makes them:
//!
//! 1. **The shape.** The plan is walked by the executor itself
//!    ([`crate::plan::check_shape`]), with a host that has no inputs: every
//!    guard on the shape of the plan and every step's arguments are checked up
//!    to the first input the executor would read. The errors are the collect's
//!    own. The walk reads what a step's guards read and no more: the steps a
//!    node frame's executor checks one by one are left out below the new step
//!    (see `elides`), and a join's right side, which the walk never reaches, is
//!    a stub, so a step's check does not grow with the chain before it. One is not raised here: `edges.nodes()` with no columns does not
//!    collect, but a later `with_columns` makes it collect.
//! 2. **The columns.** Each step is checked against the columns the frame has
//!    when the step runs, with the checks the executor's lowering makes
//!    ([`crate::plan`]'s `*_check` functions, which the lowering itself calls):
//!    unknown columns, duplicate names, comparisons between types that do not
//!    compare, roles not available on the frame, join keys, the columns a graph
//!    op's edges keep. The columns are inferred from the step before
//!    ([`Info`]), so a step costs only its own checks.
//!
//! A frame over a scan is checked for its shape only: its columns are the file's,
//! which a collect reads then (the file may also change between the two). So is
//! anything downstream of a step whose output is not known exactly here.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::common::DFSchema;
use datafusion::logical_expr::ExprSchemable;

use crate::lazy::{Column, ColumnExpr, Host, Kind, Lowering, Node, Op, Step};
use crate::lower::How;
use crate::plan::{
    check_literals, filter_check, group_by_check, py_list, py_str, rename_check, select_check,
    sort_check, PlanError, PlanResult, View, BARE_NODES, NON_GRAPH_COLUMN,
};

/// A column's name and type (when known).
type Typed = (String, Option<DataType>);

/// What validation knows of a frame after its last step.
#[derive(Default, Clone)]
pub struct Info {
    /// The columns the frame has, when known.
    pub cols: Option<Arc<Cols>>,
    /// An `edges.nodes()` frame before its first `with_columns`: the steps written
    /// so far (filters), which run once that `with_columns` gives the node set.
    pub pending: Option<Vec<Arc<Node>>>,
    /// The frame does not collect as it is, for a reason left to `collect()`
    /// (the error's message; see `at_collect`).
    pub terminal: Option<String>,
    /// What validation knows of the frame's chain, kept step by step.
    pub chain: ChainInfo,
}

/// What a frame's chain holds, kept step by step so a step reads it in O(1).
#[derive(Default, Clone)]
pub struct ChainInfo {
    /// The chain's first step.
    pub root: Root,
    /// The chain crosses `nodes()`.
    pub nodes: bool,
    /// A `with_columns` comes after `nodes()` (or, on a frame with no
    /// `nodes()`, anywhere): the node frame's first one is in the chain.
    pub columns: bool,
    /// For a step whose shape check is its own (see `elides`): the nearest step
    /// below it that is not, which a shape check reads in its place.
    pub anchor: Option<Arc<Node>>,
}

/// A chain's first step, as the executor tells chains apart.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    #[default]
    Source,
    Traversal,
    Walk,
    Describe,
}

/// A column's shared name and type (when known).
type Field_ = (Arc<str>, Option<DataType>);

/// A frame's columns: names in order, and each type where it is known.
///
/// A step that appends columns (`with_columns`) shares its input's: its columns
/// are a level over them, so a step on a wide frame costs what it adds, not the
/// frame's width. Every `MAX_DEPTH` levels they are copied into one.
pub struct Cols {
    /// The columns before this level's, if any.
    below: Option<Arc<Cols>>,
    /// This level's columns, in order.
    own: Vec<Field_>,
    /// `own` by name, for a wide level.
    index: Option<HashMap<Arc<str>, usize>>,
    depth: usize,
    /// Every column's type is known.
    all_typed: bool,
    /// All the columns in order, for a frame of more than one level.
    flat: OnceLock<Vec<Field_>>,
    /// The columns as an Arrow schema, built when first needed.
    arrow: OnceLock<SchemaRef>,
    /// The schema expressions are typed against, built when first needed.
    df: OnceLock<Option<DFSchema>>,
}

/// How many levels of appended columns a frame's columns keep before they are
/// copied into one.
const MAX_DEPTH: usize = 64;

/// How wide a level is before it is looked up by name through an index.
const INDEXED: usize = 16;

impl Cols {
    fn new(fields: Vec<(String, Option<DataType>)>) -> Arc<Cols> {
        Cols::shared(fields.into_iter().map(|(n, t)| (n.into(), t)).collect())
    }

    fn shared(own: Vec<Field_>) -> Arc<Cols> {
        Arc::new(Cols {
            below: None,
            index: (own.len() > INDEXED).then(|| {
                own.iter()
                    .enumerate()
                    .map(|(i, (n, _))| (n.clone(), i))
                    .collect()
            }),
            all_typed: own.iter().all(|(_, t)| t.is_some()),
            own,
            depth: 0,
            flat: OnceLock::new(),
            arrow: OnceLock::new(),
            df: OnceLock::new(),
        })
    }

    /// `base`'s columns, then `extra` (whose names `base` does not have).
    fn append(base: &Arc<Cols>, extra: Vec<Field_>) -> Arc<Cols> {
        if extra.is_empty() {
            return base.clone();
        }
        if base.depth + 1 >= MAX_DEPTH {
            let mut all = base.fields().to_vec();
            all.extend(extra);
            return Cols::shared(all);
        }
        Arc::new(Cols {
            all_typed: base.all_typed && extra.iter().all(|(_, t)| t.is_some()),
            below: Some(base.clone()),
            own: extra,
            index: None,
            depth: base.depth + 1,
            flat: OnceLock::new(),
            arrow: OnceLock::new(),
            df: OnceLock::new(),
        })
    }

    fn of_schema(schema: &Schema) -> Arc<Cols> {
        Cols::new(
            schema
                .fields()
                .iter()
                .map(|f| (f.name().clone(), Some(f.data_type().clone())))
                .collect(),
        )
    }

    /// The columns, in order.
    fn fields(&self) -> &[Field_] {
        let Some(below) = &self.below else {
            return &self.own;
        };
        self.flat.get_or_init(|| {
            // The levels from the first, then each one's columns in order (no
            // level below caches its own copy).
            let mut levels = vec![self];
            let mut next = below.as_ref();
            loop {
                levels.push(next);
                match &next.below {
                    Some(b) => next = b,
                    None => break,
                }
            }
            levels
                .iter()
                .rev()
                .flat_map(|l| l.own.iter().cloned())
                .collect()
        })
    }

    /// A column's type (`Some(None)` for a column of unknown type).
    fn find(&self, name: &str) -> Option<&Option<DataType>> {
        let mut level = Some(self);
        while let Some(cols) = level {
            let at = match &cols.index {
                Some(index) => index.get(name).copied(),
                None => cols.own.iter().position(|(n, _)| &**n == name),
            };
            if let Some(i) = at {
                return Some(&cols.own[i].1);
            }
            level = cols.below.as_deref();
        }
        None
    }

    fn type_of(&self, name: &str) -> Option<DataType> {
        self.find(name).cloned().flatten()
    }

    /// The type of an expression lowered against these columns.
    fn type_of_expr(&self, e: &Option<datafusion::logical_expr::Expr>) -> Option<DataType> {
        type_in(e, self)
    }
}

/// The type of an expression lowered against `view`.
fn type_in(e: &Option<datafusion::logical_expr::Expr>, view: &dyn View) -> Option<DataType> {
    let (Some(e), Some(df)) = (e, view.typed()) else {
        return None;
    };
    e.get_type(df).ok()
}

/// A frame's columns as one expression reads them: its own columns are typed
/// alone, so checking an expression over a wide frame costs what it reads, not
/// the frame's width. Names and errors are the whole frame's.
struct Narrow<'a> {
    cols: &'a Cols,
    /// The columns the expression reads that the frame has.
    read: Arc<Cols>,
    /// The frame has every column the expression reads.
    complete: bool,
}

impl<'a> Narrow<'a> {
    fn new(cols: &'a Cols, v: &serde_json::Value, roles: &[(&'static str, String)]) -> Narrow<'a> {
        let names = crate::plan::value_columns(v, roles);
        let read: Vec<Field_> = names
            .iter()
            .filter_map(|n| cols.find(n).map(|t| (n.as_str().into(), t.clone())))
            .collect();
        Narrow {
            complete: read.len() == names.len(),
            read: Cols::shared(read),
            cols,
        }
    }
}

impl View for Narrow<'_> {
    fn names(&self) -> Vec<String> {
        self.cols.names()
    }
    fn has_column(&self, name: &str) -> bool {
        self.cols.has_column(name)
    }
    fn arrow(&self) -> SchemaRef {
        if self.complete {
            self.read.arrow()
        } else {
            self.cols.arrow()
        }
    }
    fn typed(&self) -> Option<&DFSchema> {
        if self.cols.all_typed {
            self.read.typed()
        } else {
            None
        }
    }
}

impl View for Cols {
    fn names(&self) -> Vec<String> {
        self.fields().iter().map(|(n, _)| n.to_string()).collect()
    }
    fn has_column(&self, name: &str) -> bool {
        self.find(name).is_some()
    }
    fn arrow(&self) -> SchemaRef {
        self.arrow
            .get_or_init(|| {
                Arc::new(Schema::new(
                    self.fields()
                        .iter()
                        .map(|(n, t)| {
                            Field::new(n.as_ref(), t.clone().unwrap_or(DataType::Null), true)
                        })
                        .collect::<Vec<_>>(),
                ))
            })
            .clone()
    }
    fn typed(&self) -> Option<&DFSchema> {
        self.df
            .get_or_init(|| {
                self.all_typed
                    .then(|| DFSchema::try_from(self.arrow().as_ref().clone()).ok())
                    .flatten()
            })
            .as_ref()
    }
}

/// The node-id type a graph over endpoints of type `dt` has (`Int64` or
/// `Utf8`, as the id map keeps them), or `None` for a type that is no id.
pub fn canonical_id_type(dt: &DataType) -> Option<DataType> {
    match dt {
        t if t.is_integer() => Some(DataType::Int64),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Some(DataType::Utf8),
        DataType::Dictionary(_, value) => canonical_id_type(value),
        _ => None,
    }
}

/// Validate a node just built (see the module docs): its [`Info`], or the error
/// a collect of it would raise first.
pub(crate) fn validate(node: &Arc<Node>, host: &dyn Host) -> PlanResult<Info> {
    // A relational step appended to a frame keeps the frame's shape: the walk
    // over the longer plan meets the same guards, and this step's only (its
    // arguments), so it is not walked again. So does a `with_columns` of scalar
    // expressions, except as the first on `edges.nodes()` (which gives the node
    // set) and after `describe()` or a walk (which take none).
    if let Op::Step { input, step, error } = node.op() {
        let tail = match step {
            Step::Join { .. } => false,
            Step::WithColumns(columns) => {
                columns
                    .iter()
                    .all(|c| matches!(c.expr, ColumnExpr::Scalar(_)))
                    && input.info().pending.is_none()
                    && !matches!(input.info().chain.root, Root::Describe | Root::Walk)
            }
            _ => true,
        };
        if tail {
            if let Some(e) = error {
                return Err(host.error(e));
            }
            let mut info = infer(node, host)?;
            info.terminal = input.info().terminal.clone();
            info.chain = chain_info(node);
            return Ok(info);
        }
    }
    let mut terminal = None;
    if let Err(e) = crate::lazy::check_shape(node, host) {
        match &e {
            PlanError::NotImplemented(m) if at_collect(node, m) => terminal = Some(m.clone()),
            _ => return Err(e),
        }
    }
    let mut info = infer(node, host)?;
    info.terminal = terminal;
    info.chain = chain_info(node);
    Ok(info)
}

/// The [`ChainInfo`] of a node, from its input's.
fn chain_info(node: &Arc<Node>) -> ChainInfo {
    let root = |op: &Op| match op {
        Op::Hop { .. } | Op::ShortestPath { .. } => Root::Traversal,
        Op::RandomWalk { .. } => Root::Walk,
        Op::Describe { .. } => Root::Describe,
        _ => Root::Source,
    };
    let Some(input) = node.input() else {
        return ChainInfo {
            root: root(node.op()),
            ..ChainInfo::default()
        };
    };
    let parent = &input.info().chain;
    let mut info = ChainInfo {
        anchor: None,
        ..parent.clone()
    };
    match node.op() {
        Op::Nodes { .. } => {
            info.nodes = true;
            info.columns = false;
        }
        Op::Step {
            step: Step::WithColumns(_),
            ..
        } if parent.nodes || !matches!(node.kind(), Kind::Edges { .. }) => info.columns = true,
        _ => {}
    }
    if elides(node, parent) {
        info.anchor = Some(shape_input(input));
    }
    info
}

/// Whether a step's shape check is its own: whatever is appended to it walks
/// the same guards without it. So it is for a node frame's steps after its
/// first `with_columns` (all but that one, over a source with no `nodes()`):
/// the executor checks each of those steps alone, before it reads the frame's
/// rows, where a shape check ends. The steps of an edge frame are read by the
/// graph ops and `nodes()` over it, and those after `describe()` or a walk by
/// their own guards, so they are not elided.
fn elides(node: &Node, parent: &ChainInfo) -> bool {
    let Op::Step { step, .. } = node.op() else {
        return false;
    };
    matches!(node.kind(), Kind::Nodes { .. })
        && parent.root != Root::Describe
        && parent.root != Root::Walk
        && (parent.columns || (!parent.nodes && !matches!(step, Step::WithColumns(_))))
}

/// The step a shape check reads in place of `node` below a new step.
pub(crate) fn shape_input(node: &Arc<Node>) -> Arc<Node> {
    node.info()
        .chain
        .anchor
        .clone()
        .unwrap_or_else(|| node.clone())
}

/// Whether a shape error is left to `collect()`: one a later step can replace
/// or fix (`edges.nodes()` with no columns yet; the guard on the edges of a
/// traversal before `nodes()`, which a first `with_columns` over those edges
/// replaces with its own), or a source's paths, which a collect reads.
fn at_collect(node: &Node, message: &str) -> bool {
    use crate::plan::{MULTI_PATH_EDGES, MULTI_PATH_NODES};
    if [BARE_NODES, MULTI_PATH_EDGES, MULTI_PATH_NODES].contains(&message) {
        return true;
    }
    if !message.ends_with(crate::plan::STEPS_BEFORE_NODES) {
        return false;
    }
    // The chain crosses `nodes()` and no `with_columns` follows it: this node's
    // own step is not yet in its input's chain info.
    let parent = node.input().map(|i| i.info().chain.clone());
    let nodes = matches!(node.op(), Op::Nodes { .. }) || parent.as_ref().is_some_and(|c| c.nodes);
    let columns = matches!(
        node.op(),
        Op::Step {
            step: Step::WithColumns(_),
            ..
        }
    ) || parent.is_some_and(|c| c.nodes && c.columns);
    nodes && !columns
}

fn known(cols: Vec<(String, Option<DataType>)>) -> PlanResult<Info> {
    Ok(Info {
        cols: Some(Cols::new(cols)),
        ..Info::default()
    })
}

/// The roles a frame's expressions resolve (`ur.src()`, `ur.dst()`, `ur.id()`),
/// as the executor resolves them for it.
fn roles_of(node: &Node) -> Vec<(&'static str, String)> {
    let root = match node.input() {
        Some(input) => input.info().chain.root,
        None => match node.op() {
            Op::RandomWalk { .. } => Root::Walk,
            Op::Describe { .. } => Root::Describe,
            Op::Hop { .. } | Op::ShortestPath { .. } => Root::Traversal,
            _ => Root::Source,
        },
    };
    match root {
        Root::Walk | Root::Describe => vec![],
        Root::Traversal => {
            vec![("src", "src".to_string()), ("dst", "dst".to_string())]
        }
        Root::Source => match node.kind() {
            Kind::Edges { src, dst } => vec![("src", src.clone()), ("dst", dst.clone())],
            Kind::Nodes { id } => vec![("id", id.clone())],
        },
    }
}

/// The node-id type of the graph over an edge frame, when known.
fn id_type(edges: &Node) -> Option<DataType> {
    let Kind::Edges { src, .. } = edges.kind() else {
        return None;
    };
    let cols = edges.info().cols.clone()?;
    canonical_id_type(&cols.type_of(src)?)
}

fn infer(node: &Arc<Node>, host: &dyn Host) -> PlanResult<Info> {
    let id = |edges: &Node| id_type(edges);
    match node.op() {
        Op::FromArrow { schema, .. } => Ok(Info {
            cols: Some(Cols::of_schema(schema)),
            ..Info::default()
        }),
        Op::ScanEdges { .. } | Op::ScanNodes { .. } => Ok(Info::default()),
        Op::Nodes { .. } => Ok(Info {
            pending: Some(vec![]),
            ..Info::default()
        }),
        Op::Reverse { input } => Ok(Info {
            cols: input.info().cols.clone(),
            ..Info::default()
        }),
        Op::Hop { edges, .. } => {
            let t = id(edges);
            known(vec![("src".into(), t.clone()), ("dst".into(), t)])
        }
        Op::ShortestPath { edges, weight, .. } => {
            check_weight_columns(edges, weight.iter())?;
            let t = id(edges);
            known(vec![
                ("src".into(), t.clone()),
                ("dst".into(), t),
                ("hop".into(), Some(DataType::Int64)),
                ("cost".into(), Some(DataType::Float64)),
            ])
        }
        Op::RandomWalk { edges, .. } => known(vec![
            ("walk_id".into(), Some(DataType::Int64)),
            ("step".into(), Some(DataType::Int64)),
            ("node".into(), id(edges)),
        ]),
        Op::Describe { .. } => known(vec![
            ("n_nodes".into(), Some(DataType::Int64)),
            ("n_edges".into(), Some(DataType::Int64)),
            ("density".into(), Some(DataType::Float64)),
            ("avg_degree".into(), Some(DataType::Float64)),
            ("n_components".into(), Some(DataType::Int64)),
        ]),
        Op::Step { input, step, error } => {
            if error.is_some() {
                return Ok(Info::default());
            }
            let parent = input.info();
            if let Some(pending) = &parent.pending {
                return match step {
                    Step::WithColumns(columns) => node_set(node, input, columns, pending, host),
                    _ => {
                        let mut steps = pending.clone();
                        steps.push(node.clone());
                        Ok(Info {
                            pending: Some(steps),
                            ..Info::default()
                        })
                    }
                };
            }
            let Some(cols) = parent.cols.clone() else {
                return Ok(Info::default());
            };
            let cols = relational(node, step, &cols, host)?;
            Ok(Info {
                cols,
                ..Info::default()
            })
        }
    }
}

/// The columns after a relational step on a frame with columns `cols`, checked
/// (`None` when they are not known exactly).
fn relational(
    node: &Node,
    step: &Step,
    cols: &Arc<Cols>,
    host: &dyn Host,
) -> PlanResult<Option<Arc<Cols>>> {
    let roles = roles_of(node);
    Ok(Some(match step {
        Step::Filter(p) => {
            match p {
                crate::plan::Expr::Scalar(v) => {
                    filter_check(p, &roles, &Narrow::new(cols, v, &roles))?;
                }
                _ => {
                    filter_check(p, &roles, cols.as_ref())?;
                }
            }
            cols.clone()
        }
        Step::Select(items) => {
            let out = select_check(cols.as_ref(), items, &roles)?;
            Cols::new(
                out.into_iter()
                    .map(|(name, e)| {
                        let t = match &e {
                            Some(datafusion::logical_expr::Expr::Column(c)) => {
                                cols.type_of(&c.name)
                            }
                            _ => cols.type_of_expr(&e),
                        };
                        (name, t)
                    })
                    .collect(),
            )
        }
        Step::WithColumns(columns) => {
            let id_col = roles.first().map_or("id", |(_, c)| c.as_str()).to_string();
            let graph = graph_groups(columns, host)?;
            if !graph.is_empty() && !cols.has_column(&id_col) {
                return Err(PlanError::ColumnNotFound(format!(
                    "with_columns() attaches graph columns by the node id column {}, which an \
                     earlier step removed or renamed.",
                    py_str(&id_col)
                )));
            }
            let mut graph_fields: Vec<Field_> = vec![];
            for (name, t) in graph.iter().flat_map(|g| g.iter()) {
                if cols.has_column(name) {
                    // The attach joins instead, and its result is not modelled.
                    return Ok(None);
                }
                graph_fields.push((name.as_str().into(), t.clone()));
            }
            let attached = Cols::append(cols, graph_fields);
            match scalar_columns(columns, cols, &attached, &roles)? {
                Some(c) => c,
                None => return Ok(None),
            }
        }
        Step::Rename(mapping) => {
            let renamed = rename_check(cols.as_ref(), mapping)?;
            Cols::new(
                renamed
                    .into_iter()
                    .zip(cols.fields().iter())
                    .map(|(n, (_, t))| (n, t.clone()))
                    .collect(),
            )
        }
        Step::Sort(by) => {
            sort_check(cols.as_ref(), by, &roles)?;
            cols.clone()
        }
        Step::Head(_) | Step::Sample { .. } | Step::Distinct => cols.clone(),
        Step::GroupByAgg { keys, aggs } => {
            group_by_check(cols.as_ref(), keys, aggs)?;
            let mut out: Vec<(String, Option<DataType>)> =
                keys.iter().map(|k| (k.clone(), cols.type_of(k))).collect();
            for a in aggs {
                let t = matches!(a.func.as_str(), "count" | "n_unique").then_some(DataType::Int64);
                out.push((a.output.clone(), t));
            }
            Cols::new(out)
        }
        Step::Join { right, on, how } => {
            let r = right.info();
            if let Some(m) = &r.terminal {
                let e = PlanError::NotImplemented(m.clone());
                return Err(PlanError::Wrapped {
                    message: format!(
                        "join(other): could not materialize the right-hand frame ({e})."
                    ),
                    cause: Box::new(e),
                });
            }
            let Some(rcols) = r.cols.clone() else {
                return Ok(None);
            };
            let Some(how) = How::parse(how) else {
                return Ok(None);
            };
            let (left_names, right_names) = (cols.names(), rcols.names());
            for k in on {
                if !left_names.contains(k) {
                    return Err(PlanError::ColumnNotFound(format!(
                        "join(on=): key {} not in the left frame {}.",
                        py_str(k),
                        py_list(left_names.iter())
                    )));
                }
                if !right_names.contains(k) {
                    return Err(PlanError::ColumnNotFound(format!(
                        "join(on=): key {} not in the right frame {}.",
                        py_str(k),
                        py_list(right_names.iter())
                    )));
                }
            }
            let right_set: HashSet<&String> =
                right_names.iter().filter(|c| !on.contains(c)).collect();
            let collide: std::collections::BTreeSet<&String> = left_names
                .iter()
                .filter(|c| how.keeps_right() && !on.contains(c) && right_set.contains(c))
                .collect();
            if !collide.is_empty() {
                return Err(PlanError::Value(format!(
                    "join produces duplicate column name(s) {}; rename() one side before joining.",
                    py_list(collide.into_iter())
                )));
            }
            let mut out = cols.fields().to_vec();
            if how.keeps_right() {
                out.extend(
                    rcols
                        .fields()
                        .iter()
                        .filter(|(n, _)| !on.iter().any(|o| o.as_str() == &**n))
                        .cloned(),
                );
            }
            Cols::shared(out)
        }
    }))
}

/// The graph columns of a `with_columns`, one list per edge frame they run over
/// (in the order each frame first appears), checked as the executor checks
/// them before attaching them: each verb's weight literals, the edge view, and
/// the output names.
fn graph_groups(columns: &[Column], _host: &dyn Host) -> PlanResult<Vec<Vec<Typed>>> {
    let mut groups: Vec<(&Arc<Node>, Vec<&Column>)> = vec![];
    for c in columns {
        let ColumnExpr::Graph { edges, .. } = &c.expr else {
            continue;
        };
        let Some(edges) = edges else {
            return Err(PlanError::NotImplemented(NON_GRAPH_COLUMN.into()));
        };
        match groups.iter_mut().find(|(e, _)| Arc::ptr_eq(e, edges)) {
            Some((_, g)) => g.push(c),
            None => groups.push((edges, vec![c])),
        }
    }
    let mut out = Vec::with_capacity(groups.len());
    for (edges, group) in groups {
        for c in &group {
            if let ColumnExpr::Graph { verb, .. } = &c.expr {
                if let Some(w) = &verb.weight {
                    check_literals(w, "weight")?;
                }
            }
        }
        let (input, _) = Lowering::edges_of(edges);
        input.view()?;
        let weights = group.iter().filter_map(|c| match &c.expr {
            ColumnExpr::Graph { verb, .. } => verb.weight.as_ref(),
            ColumnExpr::Scalar(_) => None,
        });
        check_weight_columns(edges, weights)?;
        crate::query::check_output_names(group.iter().map(|c| c.name.as_str()))?;
        out.push(
            group
                .iter()
                .map(|c| {
                    let t = match &c.expr {
                        ColumnExpr::Graph { verb, .. } => crate::query::verb_value_type(verb),
                        ColumnExpr::Scalar(_) => None,
                    };
                    (c.name.clone(), t)
                })
                .collect(),
        );
    }
    Ok(out)
}

/// The edge columns the weights over `edges` read must exist: a collect reads
/// them from the edge rows (the columns the edge frame's steps leave). Not
/// checked over a traversal result, whose weights may be cached on its input's
/// index, or edges whose columns are not known.
fn check_weight_columns<'a>(
    edges: &Node,
    weights: impl Iterator<Item = &'a serde_json::Value>,
) -> PlanResult<()> {
    if edges.info().chain.root == Root::Traversal {
        return Ok(());
    }
    let Some(cols) = edges.info().cols.clone() else {
        return Ok(());
    };
    let mut read = std::collections::BTreeSet::new();
    for w in weights {
        read.extend(crate::plan::value_columns(w, &[]));
    }
    let missing: Vec<&String> = read.iter().filter(|c| !cols.has_column(c)).collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(PlanError::ColumnNotFound(format!(
        "weight expression references unknown edge column(s): {}",
        py_list(missing.into_iter())
    )))
}

/// The scalar expressions of a `with_columns`, checked over `attached` (the
/// frame's columns `before` with the graph columns appended), and the columns
/// after: `before` with replaced columns in place, then each new column as
/// written.
fn scalar_columns(
    columns: &[Column],
    before: &Arc<Cols>,
    attached: &Arc<Cols>,
    roles: &[(&'static str, String)],
) -> PlanResult<Option<Arc<Cols>>> {
    let mut types: Vec<Field_> = vec![];
    for c in columns {
        let t = match &c.expr {
            ColumnExpr::Scalar(v) => {
                let read = Narrow::new(attached, v, roles);
                let e = crate::plan::scalar_check(&read, v, roles, "with_columns()")?;
                type_in(&e, &read)
            }
            ColumnExpr::Graph { .. } => attached.type_of(&c.name),
        };
        types.push((c.name.as_str().into(), t));
    }
    if !types.iter().any(|(n, _)| before.has_column(n)) {
        return Ok(Some(Cols::append(before, types)));
    }
    let lookup = |n: &str| {
        types
            .iter()
            .find(|(c, _)| &**c == n)
            .map(|(_, t)| t.clone())
    };
    let mut out: Vec<Field_> = before
        .fields()
        .iter()
        .map(|(c, t)| (c.clone(), lookup(c).unwrap_or_else(|| t.clone())))
        .collect();
    for (name, t) in types {
        if !before.has_column(&name) {
            out.push((name, t));
        }
    }
    Ok(Some(Cols::shared(out)))
}

/// The first `with_columns` of an `edges.nodes()` frame: the node set and its
/// columns, then the filters written before it, which run over them.
fn node_set(
    node: &Arc<Node>,
    nodes: &Arc<Node>,
    columns: &[Column],
    pending: &[Arc<Node>],
    host: &dyn Host,
) -> PlanResult<Info> {
    let groups = graph_groups(columns, host)?;
    let Some(first) = columns.iter().find_map(|c| match &c.expr {
        ColumnExpr::Graph {
            edges: Some(edges), ..
        } => Some(edges),
        _ => None,
    }) else {
        return Err(PlanError::NotImplemented(NON_GRAPH_COLUMN.into()));
    };
    // The node set's ids: the first graph column's edges', unless steps on the
    // edges `nodes()` was called on shape the node set.
    let chain = nodes.chain();
    let at = chain
        .iter()
        .position(|n| matches!(n.op(), Op::Nodes { .. }))
        .unwrap_or(0);
    let shaped = chain[..at].iter().skip(1).any(|n| {
        !matches!(
            n.op(),
            Op::Step {
                step: Step::Filter(_),
                ..
            }
        )
    });
    let id = if shaped { None } else { id_type(first) };
    let before = Cols::new(vec![("id".into(), id)]);
    let graph: Vec<Field_> = groups
        .iter()
        .flatten()
        .map(|(n, t)| (n.as_str().into(), t.clone()))
        .collect();
    let attached = Cols::append(&before, graph);
    let roles = roles_of(node);
    let Some(cols) = scalar_columns(columns, &before, &attached, &roles)? else {
        return Ok(Info::default());
    };
    for step in pending {
        if let Op::Step {
            step: Step::Filter(p),
            ..
        } = step.op()
        {
            filter_check(p, &roles, cols.as_ref())?;
        }
    }
    Ok(Info {
        cols: Some(cols),
        ..Info::default()
    })
}
