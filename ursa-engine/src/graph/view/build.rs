//! Ids to dense node indexes: the nodes and edges of a drawing, numbered in
//! the order they are first seen, so the same rows always give the same
//! graph.

use std::collections::HashMap;
use std::sync::Arc;

use datafusion::arrow::array::{Array, ArrayRef, AsArray, Float32Array, RecordBatch, UInt32Array};
use datafusion::arrow::compute::{cast, take};
use datafusion::arrow::datatypes::{DataType, UInt64Type};
use datafusion::arrow::row::{RowConverter, SortField};
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::error::{DataFusionError, Result};

use ursa_viz::model::{Column as ModelColumn, Layout, Path, Role, Values, NO_ROW, NULL};

#[cfg(test)]
use super::rules::Shape;
use super::rules::{MatchEdge, Reading};
use super::style::{ColorBy, SizeBy};
use super::{plan_err, thousands, MAX_EDGES, MAX_NODES};

/// A drawing before it is a `GraphResult`: `n` nodes, the edges
/// `(src[i], dst[i])` between them, and each node's id as the shell prints
/// it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EdgeList {
    pub n: u32,
    pub src: Vec<u32>,
    pub dst: Vec<u32>,
    pub ids: Vec<String>,
    /// Rows left out because an endpoint is NULL.
    pub skipped: usize,
    /// What sizes the nodes (`SIZE BY`), if asked.
    pub size: Option<SizeBy>,
    /// What colours the nodes (`COLOR BY`), if asked.
    pub color: Option<ColorBy>,
    /// The vertex tables the nodes belong to; empty for one, named `""`.
    pub labels: Vec<String>,
    /// Each node's vertex table, as an index into `labels`; empty when
    /// every node is of the first.
    pub node_label: Vec<u16>,
    /// The edge tables; empty for one, named `""`.
    pub edge_labels: Vec<String>,
    /// Each edge's edge table; empty when every edge is of the first.
    pub edge_label: Vec<u16>,
    /// Each edge's row in its edge table ([`NO_ROW`]: unknown), if any is
    /// known.
    pub edge_rows: Option<Vec<u64>>,
    /// Columns the rule made: a node's distance or step, its properties.
    pub node_columns: Vec<ModelColumn>,
    /// The edges' attributes.
    pub edge_columns: Vec<ModelColumn>,
    /// The paths drawn, the seeds of a hop, and the `u32` node column that
    /// holds each node's level (its distance from the seeds, or its step
    /// along a path).
    pub paths: Vec<Path>,
    pub seeds: Vec<u32>,
    pub level: Option<String>,
    /// The layout the rule asks for.
    pub layout: Layout,
}

/// The drawing of a result's rows (`batches`), read as `shape` says.
#[cfg(test)]
pub(crate) fn from_shape(shape: Shape, batches: &[RecordBatch]) -> Result<(EdgeList, NodeIndex)> {
    match shape {
        Shape::Edges { src, dst } => edges_of(batches, src, dst),
        other => panic!("from_shape reads edges, not {other:?}"),
    }
}

/// Rule 3: a row an edge, between the ids in columns `src` and `dst`.
pub(crate) fn edges_of(
    batches: &[RecordBatch],
    src: usize,
    dst: usize,
) -> Result<(EdgeList, NodeIndex)> {
    let edges: Vec<(ArrayRef, ArrayRef)> = batches
        .iter()
        .map(|b| (Arc::clone(b.column(src)), Arc::clone(b.column(dst))))
        .collect();
    build_indexed(&[], &edges)
}

/// The graph with the nodes `vertices` (keys, in order) and the edges
/// `edges` (source and destination arrays of equal length). Nodes are
/// numbered as first seen: the vertices, then the edges' endpoints.
#[cfg(test)]
pub(crate) fn build(vertices: &[ArrayRef], edges: &[(ArrayRef, ArrayRef)]) -> Result<EdgeList> {
    Ok(build_indexed(vertices, edges)?.0)
}

/// [`build`], and the index from node ids to node indexes, to join node
/// values on.
pub(crate) fn build_indexed(
    vertices: &[ArrayRef],
    edges: &[(ArrayRef, ArrayRef)],
) -> Result<(EdgeList, NodeIndex)> {
    let rows: usize = edges.iter().map(|(s, _)| s.len()).sum();
    if rows == 0 {
        return plan_err("the result has no rows: there are no edges to draw");
    }
    if rows > MAX_EDGES {
        return plan_err(format!(
            "the result has {} edges; the graph view draws at most {}. Draw part of it, \
             with a WHERE or a LIMIT: .graph SELECT src, dst FROM … LIMIT 100000",
            thousands(rows),
            thousands(MAX_EDGES)
        ));
    }
    let keys: usize = vertices.iter().map(|v| v.len()).sum();
    if keys > MAX_NODES {
        return plan_err(format!(
            "the graph has {} nodes; the graph view draws at most {}",
            thousands(keys),
            thousands(MAX_NODES)
        ));
    }
    // One key type for every id, so 7 in one column is 7 in the other: the
    // columns' own when they agree, else their text.
    let mut types = vertices
        .iter()
        .chain(edges.iter().flat_map(|(s, d)| [s, d]))
        .map(|a| a.data_type());
    let first = types.next().cloned().unwrap_or(DataType::Null);
    let key_type = if types.all(|t| t == &first) {
        first
    } else {
        DataType::Utf8
    };
    let converter = RowConverter::new(vec![SortField::new(key_type.clone())]).map_err(|_| {
        DataFusionError::Plan(format!("node ids of type {key_type} cannot be drawn"))
    })?;
    let mut nodes = Nodes {
        index: HashMap::new(),
        ids: Vec::new(),
    };
    let options = FormatOptions::default();
    let prepare = |array: &ArrayRef| -> Result<ArrayRef> {
        Ok(if array.data_type() == &key_type {
            Arc::clone(array)
        } else {
            cast(array, &key_type)?
        })
    };

    for keys in vertices {
        let keys = prepare(keys)?;
        let rows = converter.convert_columns(&[Arc::clone(&keys)])?;
        let shown = ArrayFormatter::try_new(keys.as_ref(), &options)?;
        for i in 0..keys.len() {
            if keys.is_valid(i) {
                nodes.intern(rows.row(i).as_ref(), || shown.value(i).to_string());
            }
        }
    }
    let mut out = EdgeList {
        n: 0,
        src: Vec::with_capacity(rows),
        dst: Vec::with_capacity(rows),
        ids: Vec::new(),
        ..EdgeList::default()
    };
    for (s, d) in edges {
        let (s, d) = (prepare(s)?, prepare(d)?);
        let (s_rows, d_rows) = (
            converter.convert_columns(&[Arc::clone(&s)])?,
            converter.convert_columns(&[Arc::clone(&d)])?,
        );
        let (s_shown, d_shown) = (
            ArrayFormatter::try_new(s.as_ref(), &options)?,
            ArrayFormatter::try_new(d.as_ref(), &options)?,
        );
        for i in 0..s.len() {
            if s.is_null(i) || d.is_null(i) {
                out.skipped += 1;
                continue;
            }
            out.src
                .push(nodes.intern(s_rows.row(i).as_ref(), || s_shown.value(i).to_string()));
            out.dst
                .push(nodes.intern(d_rows.row(i).as_ref(), || d_shown.value(i).to_string()));
        }
    }
    if out.src.is_empty() {
        return plan_err(format!(
            "every row has a NULL source or destination: there are no edges to draw ({} rows)",
            thousands(rows)
        ));
    }
    out.n = nodes.ids.len() as u32;
    out.ids = nodes.ids;
    Ok((
        out,
        NodeIndex {
            key_type,
            index: nodes.index,
            labels: 0,
        },
    ))
}

/// Node indexes by id, as [`build`] numbered them: what node values are
/// joined on.
pub(crate) struct NodeIndex {
    /// The one type every id was read as.
    pub(crate) key_type: DataType,
    /// With several labels, a key is its label's two bytes and then the id.
    index: HashMap<Vec<u8>, u32>,
    /// The number of labels, when there are several; else 0.
    labels: u16,
}

impl NodeIndex {
    /// The node each of `ids` is, if it is one: with several labels, of the
    /// first label that has the id.
    pub(crate) fn lookup(&self, ids: &ArrayRef) -> Result<Vec<Option<u32>>> {
        self.lookup_in(None, ids)
    }

    /// The node of `label` each of `ids` is (of any label, for `None`).
    pub(crate) fn lookup_in(&self, label: Option<u16>, ids: &ArrayRef) -> Result<Vec<Option<u32>>> {
        let ids = if ids.data_type() == &self.key_type {
            Arc::clone(ids)
        } else {
            cast(ids, &self.key_type).map_err(|_| {
                DataFusionError::Plan(format!(
                    "node ids of type {} cannot be matched to the graph's, of type {}",
                    ids.data_type(),
                    self.key_type
                ))
            })?
        };
        let converter = RowConverter::new(vec![SortField::new(self.key_type.clone())])?;
        let rows = converter.convert_columns(&[Arc::clone(&ids)])?;
        let mut key = Vec::new();
        Ok((0..ids.len())
            .map(|i| {
                if ids.is_null(i) {
                    return None;
                }
                let row = rows.row(i);
                if self.labels == 0 {
                    return self.index.get(row.as_ref()).copied();
                }
                let labels = match label {
                    Some(l) => l..l + 1,
                    None => 0..self.labels,
                };
                labels.into_iter().find_map(|l| {
                    key.clear();
                    key.extend_from_slice(&l.to_le_bytes());
                    key.extend_from_slice(row.as_ref());
                    self.index.get(&key).copied()
                })
            })
            .collect())
    }
}

/// Node indexes by id (an id's row-format bytes), numbered as first seen.
struct Nodes {
    index: HashMap<Vec<u8>, u32>,
    ids: Vec<String>,
}

impl Nodes {
    fn intern(&mut self, key: &[u8], shown: impl FnOnce() -> String) -> u32 {
        if let Some(&i) = self.index.get(key) {
            return i;
        }
        let i = self.ids.len() as u32;
        self.index.insert(key.to_vec(), i);
        self.ids.push(shown());
        i
    }
}

/// Nodes numbered as first seen, a column of keys at a time: a node is its
/// label and its key.
struct Interner {
    key_type: DataType,
    converter: RowConverter,
    labels: u16,
    nodes: Nodes,
    node_label: Vec<u16>,
}

impl Interner {
    /// For keys of `types` (read as their own type when they agree, else as
    /// their text) in `labels` vertex tables.
    fn new<'a>(types: impl IntoIterator<Item = &'a DataType>, labels: usize) -> Result<Self> {
        let mut types = types.into_iter();
        let first = types.next().cloned().unwrap_or(DataType::Null);
        let key_type = if types.all(|t| t == &first) {
            first
        } else {
            DataType::Utf8
        };
        let converter =
            RowConverter::new(vec![SortField::new(key_type.clone())]).map_err(|_| {
                DataFusionError::Plan(format!("node ids of type {key_type} cannot be drawn"))
            })?;
        Ok(Interner {
            key_type,
            converter,
            labels: if labels > 1 { labels as u16 } else { 0 },
            nodes: Nodes {
                index: HashMap::new(),
                ids: Vec::new(),
            },
            node_label: Vec::new(),
        })
    }

    /// The node of each of `keys`, of vertex table `label`: [`NULL`] for a
    /// NULL key.
    fn column(&mut self, label: u16, keys: &ArrayRef) -> Result<Vec<u32>> {
        let keys = if keys.data_type() == &self.key_type {
            Arc::clone(keys)
        } else {
            cast(keys, &self.key_type)?
        };
        let rows = self.converter.convert_columns(&[Arc::clone(&keys)])?;
        let options = FormatOptions::default();
        let shown = ArrayFormatter::try_new(keys.as_ref(), &options)?;
        let mut out = Vec::with_capacity(keys.len());
        let mut key = Vec::new();
        for i in 0..keys.len() {
            if keys.is_null(i) {
                out.push(NULL);
                continue;
            }
            let row = rows.row(i);
            let bytes: &[u8] = if self.labels == 0 {
                row.as_ref()
            } else {
                key.clear();
                key.extend_from_slice(&label.to_le_bytes());
                key.extend_from_slice(row.as_ref());
                &key
            };
            let node = self.nodes.intern(bytes, || shown.value(i).to_string());
            if node as usize == self.node_label.len() {
                self.node_label.push(label);
            }
            out.push(node);
        }
        if self.nodes.ids.len() > MAX_NODES {
            return plan_err(format!(
                "the result has more than {} nodes, the most the graph view draws. Draw part \
                 of it, with a WHERE or a LIMIT",
                thousands(MAX_NODES)
            ));
        }
        Ok(out)
    }

    /// The nodes' ids and labels into `out`, and the index to join on.
    fn finish(self, out: &mut EdgeList) -> NodeIndex {
        out.n = self.nodes.ids.len() as u32;
        out.ids = self.nodes.ids;
        if self.labels > 0 {
            out.node_label = self.node_label;
        }
        NodeIndex {
            key_type: self.key_type,
            index: self.nodes.index,
            labels: self.labels,
        }
    }
}

fn too_many_edges(edges: usize) -> DataFusionError {
    DataFusionError::Plan(format!(
        "the result has more than {} edges, the most the graph view draws (it has {} or \
         more). Draw part of it, with a WHERE or a LIMIT",
        thousands(MAX_EDGES),
        thousands(edges)
    ))
}

/// Each row's range of a list column's elements (`None` for a NULL row).
type Ranges = Vec<Option<(usize, usize)>>;

/// A list column as its elements and each row's range of them (`None` for a
/// NULL row).
fn lists(column: &ArrayRef) -> Result<(ArrayRef, Ranges)> {
    let list = match column.data_type() {
        DataType::List(_) => Arc::clone(column),
        DataType::LargeList(f)
        | DataType::FixedSizeList(f, _)
        | DataType::ListView(f)
        | DataType::LargeListView(f) => cast(column, &DataType::List(Arc::clone(f)))?,
        other => return plan_err(format!("a path is a list of node ids, not {other}")),
    };
    let list = list.as_list::<i32>();
    let offsets = list.value_offsets();
    let ranges = (0..list.len())
        .map(|i| {
            list.is_valid(i)
                .then(|| (offsets[i] as usize, offsets[i + 1] as usize))
        })
        .collect();
    Ok((Arc::clone(list.values()), ranges))
}

/// Rule 1: each row a path. Its nodes are `nodes`' (a list of ids), or the
/// even elements of `elements` (nodes and edges alternating); `edges` is its
/// edges' rows in the edge table. The drawing is the union of the paths:
/// an edge between each two consecutive nodes, once.
pub(crate) fn paths_of(
    batch: &RecordBatch,
    nodes: Option<usize>,
    edges: Option<usize>,
    elements: Option<usize>,
    reading: &Reading,
) -> Result<(EdgeList, NodeIndex)> {
    // Each row's nodes, and its edges' rows if the result has them.
    let (keys, ranges, rows): (ArrayRef, Ranges, Option<Vec<Vec<u64>>>) = match (nodes, elements) {
        (Some(nodes), _) => {
            let (keys, ranges) = lists(batch.column(nodes))?;
            let rows = match edges {
                None => None,
                Some(edges) => {
                    let (values, of_row) = lists(batch.column(edges))?;
                    let values = cast(&values, &DataType::UInt64)?;
                    let values = values.as_primitive::<UInt64Type>();
                    Some(
                        of_row
                            .iter()
                            .map(|r| match r {
                                Some((a, b)) => (*a..*b)
                                    .map(|i| match values.is_valid(i) {
                                        true => values.value(i),
                                        false => NO_ROW,
                                    })
                                    .collect(),
                                None => Vec::new(),
                            })
                            .collect(),
                    )
                }
            };
            (keys, ranges, rows)
        }
        (None, Some(elements)) => {
            let (values, of_row) = lists(batch.column(elements))?;
            let mut picked = Vec::new();
            let mut ranges = Vec::with_capacity(of_row.len());
            let mut edge_at: Vec<Vec<u32>> = Vec::with_capacity(of_row.len());
            for r in &of_row {
                let Some((a, b)) = *r else {
                    ranges.push(None);
                    edge_at.push(Vec::new());
                    continue;
                };
                let start = picked.len();
                picked.extend((a..b).step_by(2).map(|i| i as u32));
                ranges.push(Some((start, picked.len())));
                edge_at.push((a + 1..b).step_by(2).map(|i| i as u32).collect());
            }
            let keys = take(values.as_ref(), &UInt32Array::from(picked), None)?;
            let numbers = cast(&values, &DataType::UInt64).ok();
            let rows = numbers.map(|numbers| {
                let numbers = numbers.as_primitive::<UInt64Type>();
                edge_at
                    .iter()
                    .map(|at| {
                        at.iter()
                            .map(|&i| match numbers.is_valid(i as usize) {
                                true => numbers.value(i as usize),
                                false => NO_ROW,
                            })
                            .collect()
                    })
                    .collect()
            });
            (keys, ranges, rows)
        }
        (None, None) => return plan_err("a path needs a column of its nodes"),
    };
    let mut interner = Interner::new([keys.data_type()], 1)?;
    let node_of = interner.column(0, &keys)?;
    let mut out = EdgeList::default();
    let mut by_row: HashMap<u64, u32> = HashMap::new();
    let mut by_ends: HashMap<(u32, u32), u32> = HashMap::new();
    let mut edge_rows: Vec<u64> = Vec::new();
    let mut step = vec![NULL; interner.nodes.ids.len()];
    for (r, range) in ranges.iter().enumerate() {
        let Some((a, b)) = *range else {
            out.skipped += 1;
            continue;
        };
        let path_nodes = &node_of[a..b];
        if path_nodes.is_empty() || path_nodes.contains(&NULL) {
            out.skipped += 1;
            continue;
        }
        // The edges' rows, when there is one for each step.
        let known = rows
            .as_ref()
            .map(|rows| rows[r].as_slice())
            .filter(|rows| rows.len() + 1 == path_nodes.len());
        let mut path = Path {
            nodes: path_nodes.to_vec(),
            edges: Vec::with_capacity(path_nodes.len() - 1),
        };
        for (i, &node) in path_nodes.iter().enumerate() {
            let at = &mut step[node as usize];
            *at = (*at).min(i as u32);
        }
        for (i, pair) in path_nodes.windows(2).enumerate() {
            let (u, v) = (pair[0], pair[1]);
            let row = known.map_or(NO_ROW, |rows| rows[i]);
            let next = out.src.len() as u32;
            let edge = if row != NO_ROW {
                *by_row.entry(row).or_insert(next)
            } else {
                *by_ends.entry((u.min(v), u.max(v))).or_insert(next)
            };
            if edge == next {
                // An edge row seen with other ends is not the same edge.
                out.src.push(u);
                out.dst.push(v);
                edge_rows.push(row);
                if out.src.len() > MAX_EDGES {
                    return Err(too_many_edges(out.src.len()));
                }
            }
            let ends = (out.src[edge as usize], out.dst[edge as usize]);
            let edge = if ends == (u, v) || ends == (v, u) {
                edge
            } else {
                out.src.push(u);
                out.dst.push(v);
                edge_rows.push(NO_ROW);
                out.src.len() as u32 - 1
            };
            path.edges.push(edge);
        }
        out.paths.push(path);
    }
    if out.paths.is_empty() {
        return plan_err("the result has no paths to draw: every row's path is NULL or empty");
    }
    if edge_rows.iter().any(|&r| r != NO_ROW) {
        out.edge_rows = Some(edge_rows);
    }
    if let Some(label) = &reading.edge_label {
        out.edge_labels = vec![label.clone()];
    }
    out.node_columns
        .push(ModelColumn::new(STEP, None, Values::U32(step)));
    out.level = Some(STEP.to_string());
    // One path is a route; several between the same two nodes are layers.
    let ends = |p: &Path| (p.nodes[0], *p.nodes.last().expect("a node"));
    let first = ends(&out.paths[0]);
    if out.paths.iter().all(|p| ends(p) == first) {
        out.layout = Layout::Layers;
    }
    let index = interner.finish(&mut out);
    Ok((out, index))
}

/// The node column of a path's steps: each node's place along its path (the
/// first, for a node on several).
pub(crate) const STEP: &str = "step";
/// The node column of a hop's distances: the fewest steps from a seed.
pub(crate) const DISTANCE: &str = "distance";

/// Rule 1, for `shortest_path()`: each row a step of a path, an edge from
/// `src` to `dst`, at position `step`. Rows in order of `step` are one path;
/// rows whose steps repeat are several, each in the order of its rows. Also
/// the row each edge came from, for the edges' attributes.
pub(crate) fn steps_of(
    batch: &RecordBatch,
    src: usize,
    dst: usize,
    step: usize,
) -> Result<(EdgeList, NodeIndex, Vec<usize>)> {
    let (s, d) = (batch.column(src), batch.column(dst));
    let mut interner = Interner::new([s.data_type(), d.data_type()], 1)?;
    // Row by row, so a path's nodes are numbered along it.
    let steps = cast(batch.column(step), &DataType::Int64)?;
    let steps = steps.as_primitive::<datafusion::arrow::datatypes::Int64Type>();
    let mut order: Vec<usize> = (0..batch.num_rows()).collect();
    let mut distinct: Vec<i64> = steps.iter().flatten().collect();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() == batch.num_rows() {
        order.sort_by_key(|&i| steps.value(i));
    }
    let order_array = UInt32Array::from(order.iter().map(|&i| i as u32).collect::<Vec<_>>());
    let (s, d) = (
        take(s.as_ref(), &order_array, None)?,
        take(d.as_ref(), &order_array, None)?,
    );
    // Interleaved, so nodes are numbered in path order.
    let both = datafusion::arrow::compute::interleave(
        &[s.as_ref(), d.as_ref()],
        &(0..s.len())
            .flat_map(|i| [(0, i), (1, i)])
            .collect::<Vec<_>>(),
    )?;
    let nodes = interner.column(0, &both)?;
    let mut out = EdgeList::default();
    let mut kept = Vec::new();
    let mut last: Option<i64> = None;
    for (i, &row) in order.iter().enumerate() {
        let (u, v) = (nodes[2 * i], nodes[2 * i + 1]);
        if u == NULL || v == NULL {
            out.skipped += 1;
            continue;
        }
        let at = steps.is_valid(row).then(|| steps.value(row));
        let edge = out.src.len() as u32;
        out.src.push(u);
        out.dst.push(v);
        kept.push(row);
        let continues = match (out.paths.last(), last, at) {
            (Some(p), Some(before), Some(at)) => at > before && p.nodes.last() == Some(&u),
            _ => false,
        };
        if continues {
            let p = out.paths.last_mut().expect("a path");
            p.nodes.push(v);
            p.edges.push(edge);
        } else {
            out.paths.push(Path {
                nodes: vec![u, v],
                edges: vec![edge],
            });
        }
        last = at;
    }
    if out.src.is_empty() {
        return plan_err("the result has no steps to draw: there is no path");
    }
    let mut level = vec![NULL; interner.nodes.ids.len()];
    for p in &out.paths {
        for (i, &node) in p.nodes.iter().enumerate() {
            let at = &mut level[node as usize];
            *at = (*at).min(i as u32);
        }
    }
    out.node_columns
        .push(ModelColumn::new(STEP, None, Values::U32(level)));
    out.level = Some(STEP.to_string());
    let ends = |p: &Path| (p.nodes[0], *p.nodes.last().expect("a node"));
    let first = ends(&out.paths[0]);
    if out.paths.iter().all(|p| ends(p) == first) {
        out.layout = Layout::Layers;
    }
    let index = interner.finish(&mut out);
    Ok((out, index, kept))
}

/// Rule 2: each row a node `id` reached from `seed`, `distance` steps away.
/// The nodes are the seeds and the nodes reached; a node's distance is its
/// least, from the nearest seed, and a seed's is 0. The edges come from the
/// graph (`induced_edges`), not from the rows.
pub(crate) fn hop_of(
    batch: &RecordBatch,
    seed: usize,
    id: usize,
    distance: usize,
) -> Result<(EdgeList, NodeIndex, HopRows)> {
    let (s, d) = (batch.column(seed), batch.column(id));
    let mut interner = Interner::new([s.data_type(), d.data_type()], 1)?;
    let seeds = interner.column(0, s)?;
    let mut out = EdgeList::default();
    let mut is_seed = vec![false; interner.nodes.ids.len()];
    for &node in seeds.iter().filter(|&&n| n != NULL) {
        if !std::mem::replace(&mut is_seed[node as usize], true) {
            out.seeds.push(node);
        }
    }
    let reached = interner.column(0, d)?;
    let distances = cast(batch.column(distance), &DataType::UInt32)?;
    let distances = distances.as_primitive::<datafusion::arrow::datatypes::UInt32Type>();
    let mut level = vec![NULL; interner.nodes.ids.len()];
    for &seed in &out.seeds {
        level[seed as usize] = 0;
    }
    for (i, &node) in reached.iter().enumerate() {
        if node == NULL || seeds[i] == NULL {
            out.skipped += 1;
            continue;
        }
        if distances.is_valid(i) {
            let at = &mut level[node as usize];
            *at = (*at).min(distances.value(i));
        }
    }
    if interner.nodes.ids.is_empty() {
        return plan_err("the result has no rows: there are no nodes to draw");
    }
    out.node_columns.push(ModelColumn::new(
        DISTANCE,
        Some(Role::Distance),
        Values::U32(level),
    ));
    out.level = Some(DISTANCE.to_string());
    out.layout = if out.seeds.len() == 1 {
        Layout::Rings
    } else {
        Layout::Layers
    };
    let index = interner.finish(&mut out);
    let distance = (0..reached.len())
        .map(|i| match distances.is_valid(i) {
            true => distances.value(i),
            false => NULL,
        })
        .collect();
    Ok((
        out,
        index,
        HopRows {
            seeds,
            reached,
            distance,
        },
    ))
}

/// A hop result's rows as nodes: what its star is drawn from when there is
/// no graph to read the edges of.
pub(crate) struct HopRows {
    pub seeds: Vec<u32>,
    pub reached: Vec<u32>,
    pub distance: Vec<u32>,
}

/// Rule 4: each row a match of a pattern. `keys` are the key columns of its
/// node variables, each with its vertex table (an index into `labels`), and
/// `edges` the pattern's edges between them. Each distinct edge is drawn
/// once, however many matches it is in.
pub(crate) fn matches_of(
    batch: &RecordBatch,
    keys: &[(usize, u16)],
    labels: &[String],
    edges: &[MatchEdge],
) -> Result<(EdgeList, NodeIndex)> {
    let types: Vec<&DataType> = keys
        .iter()
        .map(|(c, _)| batch.column(*c).data_type())
        .collect();
    let mut interner = Interner::new(types, labels.len())?;
    let nodes: Vec<Vec<u32>> = keys
        .iter()
        .map(|(c, label)| interner.column(*label, batch.column(*c)))
        .collect::<Result<_>>()?;
    let mut out = EdgeList::default();
    // The edge tables, in the pattern's order.
    for e in edges {
        if !out.edge_labels.contains(&e.label) {
            out.edge_labels.push(e.label.clone());
        }
    }
    let mut seen: HashMap<(u16, u32, u32), ()> = HashMap::new();
    #[allow(clippy::needless_range_loop)]
    for row in 0..batch.num_rows() {
        for e in edges {
            let (u, v) = (nodes[e.from][row], nodes[e.to][row]);
            if u == NULL || v == NULL {
                out.skipped += 1;
                continue;
            }
            let label = out
                .edge_labels
                .iter()
                .position(|l| l == &e.label)
                .expect("listed") as u16;
            let ends = if e.directed {
                (u, v)
            } else {
                (u.min(v), u.max(v))
            };
            if seen.insert((label, ends.0, ends.1), ()).is_none() {
                out.src.push(u);
                out.dst.push(v);
                out.edge_label.push(label);
                if out.src.len() > MAX_EDGES {
                    return Err(too_many_edges(out.src.len()));
                }
            }
        }
    }
    if interner.nodes.ids.is_empty() {
        return plan_err("the result has no rows: there are no matches to draw");
    }
    if out.edge_labels.len() < 2 {
        out.edge_label = Vec::new();
    }
    out.labels = labels.to_vec();
    let index = interner.finish(&mut out);
    Ok((out, index))
}

/// Rule 5: each row a node, by the key in column `id`. A key given twice is
/// one node, and a NULL key is skipped. The edges come from the graph.
pub(crate) fn nodes_of(batch: &RecordBatch, id: usize) -> Result<(EdgeList, NodeIndex)> {
    let keys = batch.column(id);
    let mut interner = Interner::new([keys.data_type()], 1)?;
    let nodes = interner.column(0, keys)?;
    let mut out = EdgeList {
        skipped: nodes.iter().filter(|&&n| n == NULL).count(),
        ..EdgeList::default()
    };
    if interner.nodes.ids.is_empty() {
        return plan_err(if batch.num_rows() == 0 {
            "the result has no rows: there are no nodes to draw".to_string()
        } else {
            format!(
                "every row's {} is NULL: there are no nodes to draw",
                batch.schema().field(id).name()
            )
        });
    }
    let index = interner.finish(&mut out);
    Ok((out, index))
}

/// A numeric column as an edge column: one value for each of `rows` (NaN
/// for NULL).
pub(crate) fn edge_attribute(
    name: &str,
    role: Option<Role>,
    values: &ArrayRef,
    rows: &[usize],
) -> Result<ModelColumn> {
    let numbers = cast(values, &DataType::Float32)?;
    let numbers = numbers
        .as_any()
        .downcast_ref::<Float32Array>()
        .expect("cast to Float32");
    let values = rows
        .iter()
        .map(|&i| match numbers.is_valid(i) {
            true => numbers.value(i),
            false => f32::NAN,
        })
        .collect();
    Ok(ModelColumn::new(name, role, Values::F32(values)))
}
