//! Ids to dense node indexes: the nodes and edges of a drawing, numbered in
//! the order they are first seen, so the same rows always give the same
//! graph.

use std::collections::HashMap;
use std::sync::Arc;

use datafusion::arrow::array::{Array, ArrayRef, RecordBatch};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::DataType;
use datafusion::arrow::row::{RowConverter, SortField};
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::error::{DataFusionError, Result};

use super::rules::Shape;
use super::style::{ColorBy, SizeBy};
use super::{plan_err, thousands, MAX_EDGES, MAX_NODES};

/// A drawing before it is a `GraphResult`: `n` nodes, the edges
/// `(src[i], dst[i])` between them, and each node's id as the shell prints
/// it.
#[derive(Debug, Clone, PartialEq)]
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
}

/// The drawing of a result's rows (`batches`), read as `shape` says.
pub(crate) fn from_shape(shape: Shape, batches: &[RecordBatch]) -> Result<(EdgeList, NodeIndex)> {
    match shape {
        Shape::Edges { src, dst } => {
            let edges: Vec<(ArrayRef, ArrayRef)> = batches
                .iter()
                .map(|b| (Arc::clone(b.column(src)), Arc::clone(b.column(dst))))
                .collect();
            build_indexed(&[], &edges)
        }
    }
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
        skipped: 0,
        size: None,
        color: None,
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
        },
    ))
}

/// Node indexes by id, as [`build`] numbered them: what node values are
/// joined on.
pub(crate) struct NodeIndex {
    /// The one type every id was read as.
    pub(crate) key_type: DataType,
    index: HashMap<Vec<u8>, u32>,
}

impl NodeIndex {
    /// The node each of `ids` is, if it is one.
    pub(crate) fn lookup(&self, ids: &ArrayRef) -> Result<Vec<Option<u32>>> {
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
        Ok((0..ids.len())
            .map(|i| {
                if ids.is_null(i) {
                    None
                } else {
                    self.index.get(rows.row(i).as_ref()).copied()
                }
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
