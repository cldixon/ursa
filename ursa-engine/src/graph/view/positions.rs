//! `POSITIONS (query)` (design §5, #502): a drawing at saved positions, with
//! no layout to run.
//!
//! The query gives a row per node: its id (the column `id`, or else the
//! first), and where it is (`x` and `y`, or else the next two columns). It
//! is what `layout_fa2(g)` returns, so positions computed once (`COPY (SELECT
//! * FROM layout_fa2(g)) TO 'pos.parquet'`) draw a graph too large to lay out
//! while one waits: `.graph g POSITIONS (SELECT * FROM 'pos.parquet')`.
//!
//! Ids are matched to the drawing's keys, as `NODES (query)`'s are. A node
//! the query has no place for (no row, or a NULL or a non-finite `x` or `y`)
//! is placed by one warm pass ([`ursa_core::algo::fa2_grow`]): beside the
//! placed node it is tied to, then a short ForceAtlas2 schedule in which
//! only the nodes without a place move. The nodes the query placed are where
//! it put them, to the bit.
//!
//! # Determinism
//!
//! The same drawing and the same rows give the same bits on any thread
//! count: the placed nodes are copied, and the others are
//! [`ursa_core::algo::fa2_grow`]'s, in the drawing's node order.

use datafusion::arrow::array::{Array, ArrayRef, AsArray};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::{DataType, Float32Type, SchemaRef};
use datafusion::error::Result;
use datafusion::prelude::SessionContext;
use ursa_core::algo::{fa2_grow, Fa2State, MultilevelParams, WarmParams};
use ursa_core::topology::UndirectedCsr;
use ursa_viz::model::Positions;

use super::build::{EdgeList, NodeIndex};
use super::{plan_err, thousands, RunSql};

/// How the clause is written, for a message.
const FORM: &str = "POSITIONS (SELECT id, x, y FROM …)";

/// The most work the warm pass does for the nodes without a place, in
/// node-iterations: a schedule of [`ursa_core::algo::warm_iterations`] is cut
/// to this many iterations' worth of the whole drawing. An iteration reads
/// every node (the placed ones repel), so 60 of them at two million nodes
/// would take as long as the layout the positions were saved to avoid.
const WARM_WORK: usize = 20_000_000;

/// A drawing's saved positions, and what is said of them.
pub(crate) struct Saved {
    pub positions: Positions,
    /// For the provenance's notes: how many nodes had no place, and how many
    /// rows were of no node.
    pub notes: Vec<String>,
}

/// The column of `schema` called `name`, or else column `or`.
fn column(schema: &SchemaRef, name: &str, or: usize) -> usize {
    schema
        .fields()
        .iter()
        .position(|f| f.name().eq_ignore_ascii_case(name))
        .unwrap_or(or)
}

/// Column `i` of `batch` as `f32`s, for `what` (`x` or `y`).
fn floats(column: &ArrayRef, name: &str, what: &str) -> Result<ArrayRef> {
    let numeric = column.data_type().is_numeric();
    match cast(column, &DataType::Float32) {
        Ok(values) if numeric => Ok(values),
        _ => plan_err(format!(
            "POSITIONS (…): the column '{name}' is read as each node's {what}, and it is {}, not \
             a number. Give the ids and then the positions: {FORM}",
            column.data_type()
        )),
    }
}

/// The positions `query` gives the nodes of `edges`, which `index` finds by
/// id; the nodes it leaves out placed around their neighbours.
pub(crate) async fn saved(
    ctx: &SessionContext,
    sql: &dyn RunSql,
    query: &str,
    edges: &EdgeList,
    index: &NodeIndex,
) -> Result<Saved> {
    let (schema, batches) = sql.run(ctx, query).await?;
    let columns = schema.fields().len();
    if columns < 3 {
        return plan_err(format!(
            "POSITIONS (…) needs three columns, each node's id and its x and y; got {columns}. \
             Write {FORM}, or the rows layout_fa2 returns: POSITIONS (SELECT * FROM \
             layout_fa2(g))"
        ));
    }
    let id = column(&schema, "id", 0);
    // By name, or else the two columns after the ids, in order.
    let mut rest = (0..columns).filter(|&c| c != id);
    let first = rest.next().expect("three columns");
    let second = rest.next().expect("three columns");
    let (x_at, y_at) = (column(&schema, "x", first), column(&schema, "y", second));
    if x_at == id || y_at == id || x_at == y_at {
        return plan_err(format!(
            "POSITIONS (…) reads the column 'id' (or else the first) as the nodes' ids, 'x' and \
             'y' (or else the next two) as their positions, and here two of them are one \
             column. Name them: {FORM}"
        ));
    }
    let name = |c: usize| schema.field(c).name().as_str();

    let n = edges.n as usize;
    let mut x = vec![0.0f32; n];
    let mut y = vec![0.0f32; n];
    let mut placed = vec![false; n];
    let (mut rows, mut strangers, mut unplaced_rows) = (0usize, 0usize, 0usize);
    for batch in &batches {
        let nodes = index.lookup(batch.column(id)).map_err(|e| {
            datafusion::error::DataFusionError::Plan(format!(
                "POSITIONS (…): the column '{}' is read as the nodes' ids, and {}",
                name(id),
                crate::user_message(&e)
            ))
        })?;
        let xs = floats(batch.column(x_at), name(x_at), "x")?;
        let ys = floats(batch.column(y_at), name(y_at), "y")?;
        let (xs, ys) = (
            xs.as_primitive::<Float32Type>(),
            ys.as_primitive::<Float32Type>(),
        );
        rows += batch.num_rows();
        for (row, node) in nodes.into_iter().enumerate() {
            let Some(node) = node else {
                strangers += 1;
                continue;
            };
            let node = node as usize;
            let (px, py) = (xs.value(row), ys.value(row));
            if xs.is_null(row) || ys.is_null(row) || !px.is_finite() || !py.is_finite() {
                unplaced_rows += 1;
                continue;
            }
            // A node given twice is where its first row puts it.
            if !placed[node] {
                placed[node] = true;
                x[node] = px;
                y[node] = py;
            }
        }
    }
    let known = placed.iter().filter(|&&p| p).count();
    if known == 0 {
        return plan_err(format!(
            "POSITIONS (…) places none of the drawing's {} nodes: its {} rows have {}. The \
             column '{}' is matched to the nodes' ids (the drawing's are like {}); positions \
             saved from another graph, or from another id column, do not match",
            thousands(n),
            thousands(rows),
            if unplaced_rows > 0 && strangers == 0 {
                "no x and y that are numbers"
            } else {
                "no id that is a node of it"
            },
            name(id),
            edges.ids.first().map_or("none", String::as_str),
        ));
    }
    let mut notes = Vec::new();
    let missing = n - known;
    if missing > 0 {
        place_missing(edges, &placed, &mut x, &mut y);
        notes.push(format!(
            "{} of {} nodes had no saved position and {} placed beside {} neighbours",
            thousands(missing),
            thousands(n),
            if missing == 1 { "was" } else { "were" },
            if missing == 1 { "its" } else { "their" },
        ));
    }
    if strangers > 0 {
        notes.push(format!(
            "{} of POSITIONS's rows {} of no node drawn",
            thousands(strangers),
            if strangers == 1 { "is" } else { "are" },
        ));
    }
    Ok(Saved {
        positions: Positions { x, y },
        notes,
    })
}

/// Give the nodes of `edges` that are not `placed` a position: one warm pass
/// around their neighbours, the placed nodes held where they are.
///
/// [`fa2_grow`] grows a layout by nodes numbered after the ones it has, so
/// the drawing is renumbered for it: the placed nodes first, then the
/// others, each in the drawing's order.
fn place_missing(edges: &EdgeList, placed: &[bool], x: &mut [f32], y: &mut [f32]) {
    let n = placed.len();
    // new[i]: node i's number with the placed ones first; old[j]: its inverse.
    let mut new = vec![0u32; n];
    let mut old: Vec<u32> = Vec::with_capacity(n);
    old.extend((0..n as u32).filter(|&i| placed[i as usize]));
    let known = old.len();
    old.extend((0..n as u32).filter(|&i| !placed[i as usize]));
    for (j, &i) in old.iter().enumerate() {
        new[i as usize] = j as u32;
    }
    let renumber = |ends: &[u32]| -> Vec<u32> { ends.iter().map(|&e| new[e as usize]).collect() };
    let adjacency = UndirectedCsr::from_edges(n, &renumber(&edges.src), &renumber(&edges.dst));
    drop(new);
    let (kx, ky): (Vec<f32>, Vec<f32>) = old[..known]
        .iter()
        .map(|&i| (x[i as usize], y[i as usize]))
        .unzip();
    // The masses are set by `fa2_grow`, from the adjacency.
    let params = MultilevelParams::default().fa2;
    let mut state = Fa2State::from_positions(params, kx, ky, vec![1.0; known]);
    // Every placed node is held: the saved picture does not move.
    let warm = WarmParams {
        freeze_above: 0,
        ..WarmParams::default()
    };
    let schedule = fa2_grow(&mut state, &adjacency, None, &[], warm);
    let iterations = schedule.min((WARM_WORK / n.max(1)).max(1) as u32);
    state.step(&adjacency, iterations);
    let (sx, sy) = state.positions();
    for (j, &i) in old.iter().enumerate().skip(known) {
        x[i as usize] = sx[j];
        y[i as usize] = sy[j];
    }
}
