//! Validating a frame as it is built (#193): what a collect of it would raise
//! that the plan alone can tell, raised by the step that adds the problem.
//!
//! Two checks run when a [`Node`] is built, in the order a collect makes them:
//!
//! 1. **The shape.** The plan is walked by the executor itself
//!    ([`crate::plan::check_shape`]), with a host that has no inputs: every
//!    guard on the shape of the plan and every step's arguments are checked up
//!    to the first input the executor would read. The errors are the collect's
//!    own. One is not raised here: `edges.nodes()` with no columns does not
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

use std::collections::HashSet;
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
}

/// A frame's columns: names in order, and each type where it is known.
pub struct Cols {
    fields: Vec<(String, Option<DataType>)>,
    arrow: SchemaRef,
    /// The schema expressions are typed against, built when first needed.
    df: OnceLock<Option<DFSchema>>,
}

impl Cols {
    fn new(fields: Vec<(String, Option<DataType>)>) -> Arc<Cols> {
        let arrow = Arc::new(Schema::new(
            fields
                .iter()
                .map(|(n, t)| Field::new(n, t.clone().unwrap_or(DataType::Null), true))
                .collect::<Vec<_>>(),
        ));
        Arc::new(Cols {
            fields,
            arrow,
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

    fn type_of(&self, name: &str) -> Option<DataType> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, t)| t.clone())
    }

    /// The type of an expression lowered against these columns.
    fn type_of_expr(&self, e: &Option<datafusion::logical_expr::Expr>) -> Option<DataType> {
        let (Some(e), Some(df)) = (e, self.typed()) else {
            return None;
        };
        e.get_type(df).ok()
    }
}

impl View for Cols {
    fn names(&self) -> Vec<String> {
        self.fields.iter().map(|(n, _)| n.clone()).collect()
    }
    fn has_column(&self, name: &str) -> bool {
        self.fields.iter().any(|(n, _)| n == name)
    }
    fn arrow(&self) -> SchemaRef {
        self.arrow.clone()
    }
    fn typed(&self) -> Option<&DFSchema> {
        self.df
            .get_or_init(|| {
                let typed = self.fields.iter().all(|(_, t)| t.is_some());
                typed
                    .then(|| DFSchema::try_from(self.arrow.as_ref().clone()).ok())
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
                    && !matches!(
                        node.chain()[0].op(),
                        Op::Describe { .. } | Op::RandomWalk { .. }
                    )
            }
            _ => true,
        };
        if tail {
            if let Some(e) = error {
                return Err(host.error(e));
            }
            let mut info = infer(node, host)?;
            info.terminal = input.info().terminal.clone();
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
    Ok(info)
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
    let chain = node.chain();
    let Some(at) = chain
        .iter()
        .position(|n| matches!(n.op(), Op::Nodes { .. }))
    else {
        return false;
    };
    !chain[at..].iter().any(|n| {
        matches!(
            n.op(),
            Op::Step {
                step: Step::WithColumns(_),
                ..
            }
        )
    })
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
    match &node.chain()[0].op {
        Op::RandomWalk { .. } | Op::Describe { .. } => vec![],
        Op::Hop { .. } | Op::ShortestPath { .. } => {
            vec![("src", "src".to_string()), ("dst", "dst".to_string())]
        }
        _ => match node.kind() {
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
            filter_check(p, &roles, cols.as_ref())?;
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
            let mut attached = cols.fields.clone();
            for (name, t) in graph.iter().flat_map(|g| g.iter()) {
                if cols.has_column(name) {
                    // The attach joins instead, and its result is not modelled.
                    return Ok(None);
                }
                attached.push((name.clone(), t.clone()));
            }
            match scalar_columns(columns, &cols.names(), Cols::new(attached), &roles)? {
                Some(c) => c,
                None => return Ok(None),
            }
        }
        Step::Rename(mapping) => {
            let renamed = rename_check(cols.as_ref(), mapping)?;
            Cols::new(
                renamed
                    .into_iter()
                    .zip(cols.fields.iter())
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
            let mut out = cols.fields.clone();
            if how.keeps_right() {
                out.extend(
                    rcols
                        .fields
                        .iter()
                        .filter(|(n, _)| !on.contains(n))
                        .cloned(),
                );
            }
            Cols::new(out)
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
    if matches!(
        edges.chain()[0].op(),
        Op::Hop { .. } | Op::ShortestPath { .. }
    ) {
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
    before: &[String],
    attached: Arc<Cols>,
    roles: &[(&'static str, String)],
) -> PlanResult<Option<Arc<Cols>>> {
    let mut types: Vec<(String, Option<DataType>)> = vec![];
    for c in columns {
        let t = match &c.expr {
            ColumnExpr::Scalar(v) => {
                let e = crate::plan::scalar_check(attached.as_ref(), v, roles, "with_columns()")?;
                attached.type_of_expr(&e)
            }
            ColumnExpr::Graph { .. } => attached.type_of(&c.name),
        };
        types.push((c.name.clone(), t));
    }
    let lookup = |n: &str| types.iter().find(|(c, _)| c == n).map(|(_, t)| t.clone());
    let mut out: Vec<(String, Option<DataType>)> = before
        .iter()
        .map(|c| (c.clone(), lookup(c).unwrap_or_else(|| attached.type_of(c))))
        .collect();
    for (name, t) in &types {
        if !before.contains(name) {
            out.push((name.clone(), t.clone()));
        }
    }
    Ok(Some(Cols::new(out)))
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
    let mut attached: Vec<(String, Option<DataType>)> = vec![("id".into(), id)];
    for g in &groups {
        attached.extend(g.iter().cloned());
    }
    let roles = roles_of(node);
    let Some(cols) = scalar_columns(columns, &["id".to_string()], Cols::new(attached), &roles)?
    else {
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
