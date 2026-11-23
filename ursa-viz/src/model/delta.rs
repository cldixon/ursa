//! [`GraphDelta`]: a change to a [`GraphResult`], and its exact inverse.

use std::collections::HashSet;

use super::result::{
    bits_eq, err, Column, Encodings, GraphResult, Highlights, Key, ModelError, NodeLabel,
    Positions, Provenance, Strings, Target, Values, NO_ROW, NULL, REMOVED,
};

/// The values a batch of new nodes or edges has in one column.
///
/// Equal when the bits are, as [`Values`] is.
#[derive(Debug, Clone)]
pub enum BatchValues {
    F32(Vec<f32>),
    U32(Vec<u32>),
    /// For a dictionary column: the values the column does not have yet, and
    /// the rows' codes into the column's values followed by these.
    Dict {
        new_values: Strings,
        codes: Vec<u32>,
    },
}

impl PartialEq for BatchValues {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (BatchValues::F32(a), BatchValues::F32(b)) => bits_eq(a, b),
            (BatchValues::U32(a), BatchValues::U32(b)) => a == b,
            (
                BatchValues::Dict { new_values, codes },
                BatchValues::Dict {
                    new_values: v,
                    codes: c,
                },
            ) => new_values == v && codes == c,
            _ => false,
        }
    }
}

impl BatchValues {
    /// Text for the dictionary column `column`, `None` for null: finds each
    /// text's code, and lists the ones the column lacks as new values.
    ///
    /// Panics if `column` is not a dictionary column.
    pub fn dict<'a>(column: &Values, texts: impl IntoIterator<Item = Option<&'a str>>) -> Self {
        let Values::Dict { values, .. } = column else {
            panic!("BatchValues::dict needs a dictionary column");
        };
        let mut seen: std::collections::HashMap<&str, u32> = values.iter().zip(0..).collect();
        let mut new_values = Strings::new();
        let mut codes = Vec::new();
        for text in texts {
            codes.push(match text {
                None => NULL,
                Some(text) => {
                    let next = (values.len() + new_values.len()) as u32;
                    *seen.entry(text).or_insert_with(|| {
                        new_values.push(text);
                        next
                    })
                }
            });
        }
        BatchValues::Dict { new_values, codes }
    }

    pub fn len(&self) -> usize {
        match self {
            BatchValues::F32(v) => v.len(),
            BatchValues::U32(v) => v.len(),
            BatchValues::Dict { codes, .. } => codes.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `"f32"`, `"u32"` or `"dict"`.
    pub fn type_name(&self) -> &'static str {
        match self {
            BatchValues::F32(_) => "f32",
            BatchValues::U32(_) => "u32",
            BatchValues::Dict { .. } => "dict",
        }
    }
}

/// One column's values for a batch, by the column's name.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchColumn {
    pub name: String,
    pub values: BatchValues,
}

impl BatchColumn {
    pub fn new(name: impl Into<String>, values: BatchValues) -> Self {
        Self {
            name: name.into(),
            values,
        }
    }
}

/// Nodes to add at the end of a result, or the ones to take back off it.
///
/// A column of the result that the batch leaves out gets nulls. `positions`
/// is given exactly when the result has positions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeBatch {
    /// Each node's label, as an index into the result's node labels.
    pub labels: Vec<u16>,
    /// Each node's key as text; an integer key in decimal.
    pub keys: Strings,
    pub columns: Vec<BatchColumn>,
    pub positions: Option<Positions>,
}

impl NodeBatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Nodes of one label, by key.
    pub fn of_label<K: Into<Key>>(label: u16, keys: impl IntoIterator<Item = K>) -> Self {
        let mut batch = Self::new();
        for key in keys {
            batch.push(label, &key.into());
        }
        batch
    }

    pub fn push(&mut self, label: u16, key: &Key) {
        self.labels.push(label);
        self.keys.push(&key.to_string());
    }

    pub fn with_column(mut self, name: impl Into<String>, values: BatchValues) -> Self {
        self.columns.push(BatchColumn::new(name, values));
        self
    }

    pub fn len(&self) -> usize {
        self.labels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }
}

/// Edges to add at the end of a result, or the ones to take back off it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EdgeBatch {
    pub src: Vec<u32>,
    pub dst: Vec<u32>,
    /// Each edge's label, as an index into the result's edge labels.
    pub labels: Vec<u16>,
    /// Each edge's row in its edge table ([`NO_ROW`]: unknown), or `None`
    /// when none is known.
    pub rows: Option<Vec<u64>>,
    pub columns: Vec<BatchColumn>,
}

impl EdgeBatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Edges of one label, with no rows known.
    pub fn of_label(label: u16, src: Vec<u32>, dst: Vec<u32>) -> Self {
        Self {
            labels: vec![label; src.len()],
            src,
            dst,
            rows: None,
            columns: Vec::new(),
        }
    }

    pub fn with_rows(mut self, rows: Vec<u64>) -> Self {
        self.rows = Some(rows);
        self
    }

    pub fn with_column(mut self, name: impl Into<String>, values: BatchValues) -> Self {
        self.columns.push(BatchColumn::new(name, values));
        self
    }

    pub fn len(&self) -> usize {
        self.src.len()
    }

    pub fn is_empty(&self) -> bool {
        self.src.is_empty()
    }
}

/// One step of a delta. Every op carries what its inverse needs, so
/// [`invert`](Op::invert) reads no result, and every op is strict: it
/// applies only to the state it was made for, and otherwise changes nothing
/// and says why. That is what makes the inverse exact.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// A new vertex table, at the end of the node labels.
    AddNodeLabel(NodeLabel),
    /// Undo [`AddNodeLabel`](Op::AddNodeLabel): the last label, which no
    /// node has.
    PopNodeLabel(NodeLabel),
    /// A new edge table, at the end of the edge labels.
    AddEdgeLabel(String),
    /// Undo [`AddEdgeLabel`](Op::AddEdgeLabel).
    PopEdgeLabel(String),
    /// New nodes, in new slots at the end. Their identities must be new.
    AddNodes(NodeBatch),
    /// Undo [`AddNodes`](Op::AddNodes): take these nodes, which are the
    /// last, off the end. No edge, seed or path may still name them.
    PopNodes(NodeBatch),
    /// New edges, in new slots at the end.
    AddEdges(EdgeBatch),
    /// Undo [`AddEdges`](Op::AddEdges): take these edges, which are the
    /// last, off the end. No path may still name them.
    PopEdges(EdgeBatch),
    /// Remove nodes by index. A removed node keeps its slot and its data,
    /// and its edges are not shown while it is removed.
    RemoveNodes(Vec<u32>),
    /// Undo [`RemoveNodes`](Op::RemoveNodes).
    RestoreNodes(Vec<u32>),
    /// Remove edges by index; they keep their slots.
    RemoveEdges(Vec<u32>),
    /// Undo [`RemoveEdges`](Op::RemoveEdges).
    RestoreEdges(Vec<u32>),
    /// Replace the column at `index` (`old` → `new`), add one there (`old`
    /// is `None`), or drop it (`new` is `None`). See
    /// [`GraphResult::set_column_op`].
    SetColumn {
        target: Target,
        index: u32,
        old: Option<Column>,
        new: Option<Column>,
    },
    SetHighlights {
        old: Highlights,
        new: Highlights,
    },
    SetEncodings {
        old: Encodings,
        new: Encodings,
    },
    SetPositions {
        old: Option<Positions>,
        new: Option<Positions>,
    },
}

impl Op {
    /// The op that undoes this one.
    pub fn invert(&self) -> Op {
        match self.clone() {
            Op::AddNodeLabel(l) => Op::PopNodeLabel(l),
            Op::PopNodeLabel(l) => Op::AddNodeLabel(l),
            Op::AddEdgeLabel(l) => Op::PopEdgeLabel(l),
            Op::PopEdgeLabel(l) => Op::AddEdgeLabel(l),
            Op::AddNodes(b) => Op::PopNodes(b),
            Op::PopNodes(b) => Op::AddNodes(b),
            Op::AddEdges(b) => Op::PopEdges(b),
            Op::PopEdges(b) => Op::AddEdges(b),
            Op::RemoveNodes(v) => Op::RestoreNodes(v),
            Op::RestoreNodes(v) => Op::RemoveNodes(v),
            Op::RemoveEdges(v) => Op::RestoreEdges(v),
            Op::RestoreEdges(v) => Op::RemoveEdges(v),
            Op::SetColumn {
                target,
                index,
                old,
                new,
            } => Op::SetColumn {
                target,
                index,
                old: new,
                new: old,
            },
            Op::SetHighlights { old, new } => Op::SetHighlights { old: new, new: old },
            Op::SetEncodings { old, new } => Op::SetEncodings { old: new, new: old },
            Op::SetPositions { old, new } => Op::SetPositions { old: new, new: old },
        }
    }

    /// The op's name, as the wire format writes it.
    pub fn name(&self) -> &'static str {
        match self {
            Op::AddNodeLabel(_) => "add_node_label",
            Op::PopNodeLabel(_) => "pop_node_label",
            Op::AddEdgeLabel(_) => "add_edge_label",
            Op::PopEdgeLabel(_) => "pop_edge_label",
            Op::AddNodes(_) => "add_nodes",
            Op::PopNodes(_) => "pop_nodes",
            Op::AddEdges(_) => "add_edges",
            Op::PopEdges(_) => "pop_edges",
            Op::RemoveNodes(_) => "remove_nodes",
            Op::RestoreNodes(_) => "restore_nodes",
            Op::RemoveEdges(_) => "remove_edges",
            Op::RestoreEdges(_) => "restore_edges",
            Op::SetColumn { .. } => "set_column",
            Op::SetHighlights { .. } => "set_highlights",
            Op::SetEncodings { .. } => "set_encodings",
            Op::SetPositions { .. } => "set_positions",
        }
    }
}

/// A change to a result: ops applied in order, all or none.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphDelta {
    pub ops: Vec<Op>,
    /// Where the change came from: the SQL of the step that made it. It is
    /// the same for a delta and its inverse.
    pub provenance: Provenance,
}

impl GraphDelta {
    pub fn new(ops: Vec<Op>) -> Self {
        Self {
            ops,
            provenance: Provenance::default(),
        }
    }

    pub fn with_provenance(mut self, provenance: Provenance) -> Self {
        self.provenance = provenance;
        self
    }

    /// The delta that undoes this one: each op's inverse, in reverse order.
    /// It reads no result, so undo costs no query.
    pub fn invert(&self) -> GraphDelta {
        GraphDelta {
            ops: self.ops.iter().rev().map(Op::invert).collect(),
            provenance: self.provenance.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

impl GraphResult {
    /// Apply a delta: every op, or, if one does not fit, none of them.
    ///
    /// Applying `delta` and then `delta.invert()` leaves the result exactly
    /// as it was.
    pub fn apply(&mut self, delta: &GraphDelta) -> Result<(), ModelError> {
        for (i, op) in delta.ops.iter().enumerate() {
            if let Err(e) = self.apply_op(op) {
                for done in delta.ops[..i].iter().rev() {
                    self.apply_op(&done.invert())
                        .expect("an applied op's inverse applies");
                }
                return err(format!("op {i} ({}): {e}", op.name()));
            }
        }
        Ok(())
    }

    /// A result made of one delta applied to an empty one.
    pub fn build(
        graph: Option<String>,
        provenance: Provenance,
        delta: &GraphDelta,
    ) -> Result<Self, ModelError> {
        let mut result = Self::new(graph, provenance);
        result.apply(delta)?;
        Ok(result)
    }

    /// The op that makes `column` a column of `target`: it replaces the
    /// column of that name where it is, or is added after the others.
    ///
    /// Like the other `_op` makers, it is made for the result as it is now:
    /// to add two columns, apply the first op before making the second.
    pub fn set_column_op(&self, target: Target, column: Column) -> Op {
        let columns = self.columns(target);
        let index = columns.iter().position(|c| c.name == column.name);
        Op::SetColumn {
            target,
            index: index.unwrap_or(columns.len()) as u32,
            old: index.map(|i| columns[i].clone()),
            new: Some(column),
        }
    }

    /// The op that drops the column `name` of `target`, if there is one.
    pub fn drop_column_op(&self, target: Target, name: &str) -> Option<Op> {
        let columns = self.columns(target);
        let index = columns.iter().position(|c| c.name == name)?;
        Some(Op::SetColumn {
            target,
            index: index as u32,
            old: Some(columns[index].clone()),
            new: None,
        })
    }

    pub fn set_highlights_op(&self, new: Highlights) -> Op {
        Op::SetHighlights {
            old: self.highlights.clone(),
            new,
        }
    }

    pub fn set_encodings_op(&self, new: Encodings) -> Op {
        Op::SetEncodings {
            old: self.encodings.clone(),
            new,
        }
    }

    pub fn set_positions_op(&self, new: Option<Positions>) -> Op {
        Op::SetPositions {
            old: self.positions.clone(),
            new,
        }
    }

    /// Apply one op, or change nothing and say why.
    fn apply_op(&mut self, op: &Op) -> Result<(), ModelError> {
        match op {
            Op::AddNodeLabel(label) => {
                if self.node_labels.iter().any(|l| l.name == label.name) {
                    return err(format!("there is a node label '{}' already", label.name));
                }
                if self.node_labels.len() >= u16::MAX as usize {
                    return err("more node labels than a u16 can number");
                }
                self.node_labels.push(label.clone());
            }
            Op::PopNodeLabel(label) => {
                if self.node_labels.last() != Some(label) {
                    return err(format!("the last node label is not '{}'", label.name));
                }
                let l = (self.node_labels.len() - 1) as u16;
                if self.node_label.contains(&l) {
                    return err(format!("nodes still have the label '{}'", label.name));
                }
                self.node_labels.pop();
            }
            Op::AddEdgeLabel(name) => {
                if self.edge_labels.contains(name) {
                    return err(format!("there is an edge label '{name}' already"));
                }
                if self.edge_labels.len() >= u16::MAX as usize {
                    return err("more edge labels than a u16 can number");
                }
                self.edge_labels.push(name.clone());
            }
            Op::PopEdgeLabel(name) => {
                if self.edge_labels.last() != Some(name) {
                    return err(format!("the last edge label is not '{name}'"));
                }
                let l = (self.edge_labels.len() - 1) as u16;
                if self.edge_label.contains(&l) {
                    return err(format!("edges still have the label '{name}'"));
                }
                self.edge_labels.pop();
            }
            Op::AddNodes(batch) => self.add_nodes(batch)?,
            Op::PopNodes(batch) => self.pop_nodes(batch)?,
            Op::AddEdges(batch) => self.add_edges(batch)?,
            Op::PopEdges(batch) => self.pop_edges(batch)?,
            Op::RemoveNodes(nodes) => set_flags(&mut self.node_flags, nodes, true, "node")?,
            Op::RestoreNodes(nodes) => set_flags(&mut self.node_flags, nodes, false, "node")?,
            Op::RemoveEdges(edges) => self.flag_edges(edges, true)?,
            Op::RestoreEdges(edges) => self.flag_edges(edges, false)?,
            Op::SetColumn {
                target,
                index,
                old,
                new,
            } => self.set_column(*target, *index as usize, old.as_ref(), new.as_ref())?,
            Op::SetHighlights { old, new } => {
                if self.highlights != *old {
                    return err("the highlights are not the ones this op replaces");
                }
                self.check_highlights(new)?;
                self.highlights = new.clone();
            }
            Op::SetEncodings { old, new } => {
                if self.encodings != *old {
                    return err("the encodings are not the ones this op replaces");
                }
                self.check_encodings(new)?;
                self.encodings = new.clone();
            }
            Op::SetPositions { old, new } => {
                if self.positions != *old {
                    return err("the positions are not the ones this op replaces");
                }
                if let Some(p) = new {
                    check_positions(p, self.n_nodes())?;
                }
                self.positions = new.clone();
            }
        }
        Ok(())
    }

    fn add_nodes(&mut self, batch: &NodeBatch) -> Result<(), ModelError> {
        let n = self.n_nodes();
        let count = batch.len();
        if batch.keys.len() != count {
            return err(format!("{count} labels, but {} keys", batch.keys.len()));
        }
        if n + count > NULL as usize {
            return err("more nodes than a u32 can number");
        }
        for (i, &label) in batch.labels.iter().enumerate() {
            let Some(node_label) = self.node_labels.get(label as usize) else {
                return err(format!(
                    "new node {i} has the label {label}, but the result has {} node labels",
                    self.node_labels.len()
                ));
            };
            Key::check(node_label.key, batch.keys.get(i)).map_err(|e| {
                ModelError(format!(
                    "new node {i} of the label '{}': {e}",
                    node_label.name
                ))
            })?;
        }
        match (&self.positions, &batch.positions) {
            (Some(_), Some(p)) => check_positions(p, count)?,
            (None, None) => {}
            (Some(_), None) => return err("the result has positions, but the new nodes have none"),
            (None, Some(_)) => return err("the new nodes have positions, but the result has none"),
        }
        check_batch_columns(&self.node_columns, &batch.columns, count, "node")?;

        // The identities must be new, among the nodes and among themselves.
        // The index reads a node's key and label from the result, so the
        // new ones go in first, and come out again if one is refused.
        self.node_label.extend_from_slice(&batch.labels);
        self.node_keys.extend(&batch.keys);
        for i in n..n + count {
            let first = self
                .index
                .insert(&self.node_keys, &self.node_label, i as u32);
            if let Some(first) = first {
                for done in n..i {
                    self.index
                        .remove(&self.node_keys, &self.node_label, done as u32);
                }
                let refused = err(format!(
                    "'{}' of the label '{}' is node {first} already",
                    self.node_keys.get(i),
                    self.node_labels[self.node_label[i] as usize].name
                ));
                self.node_label.truncate(n);
                self.node_keys.truncate(n);
                return refused;
            }
        }

        self.node_flags.resize(n + count, 0);
        extend_columns(&mut self.node_columns, &batch.columns, count);
        if let (Some(p), Some(new)) = (&mut self.positions, &batch.positions) {
            p.x.extend_from_slice(&new.x);
            p.y.extend_from_slice(&new.y);
        }
        Ok(())
    }

    fn pop_nodes(&mut self, batch: &NodeBatch) -> Result<(), ModelError> {
        let count = batch.len();
        let Some(n) = self.n_nodes().checked_sub(count) else {
            return err(format!(
                "{count} nodes to take off, but the result has {}",
                self.n_nodes()
            ));
        };
        if batch.keys.len() != count
            || self.node_label[n..] != batch.labels[..]
            || !self.node_keys.ends_with(&batch.keys)
        {
            return err("the last nodes are not the ones this op takes off");
        }
        if self.node_flags[n..].iter().any(|&f| f != 0) {
            return err("one of the last nodes is removed; restore it first");
        }
        match (&self.positions, &batch.positions) {
            (Some(p), Some(tail)) => {
                if !bits_eq(&p.x[n..], &tail.x) || !bits_eq(&p.y[n..], &tail.y) {
                    return err("the last nodes are not where this op says they are");
                }
            }
            (None, None) => {}
            _ => return err("the result and the op do not agree on whether nodes have positions"),
        }
        check_tail_columns(&self.node_columns, &batch.columns, n, "node")?;
        let named = |nodes: &[u32]| nodes.iter().any(|&v| v as usize >= n);
        if self.names_past(n) {
            return err("an edge still names one of the last nodes");
        }
        if named(&self.highlights.seeds) || self.highlights.paths.iter().any(|p| named(&p.nodes)) {
            return err("a highlight still names one of the last nodes");
        }

        for i in n..n + count {
            self.index
                .remove(&self.node_keys, &self.node_label, i as u32);
        }
        self.node_label.truncate(n);
        self.node_keys.truncate(n);
        self.node_flags.truncate(n);
        pop_columns(&mut self.node_columns, &batch.columns, n);
        if let Some(p) = &mut self.positions {
            p.x.truncate(n);
            p.y.truncate(n);
        }
        Ok(())
    }

    fn add_edges(&mut self, batch: &EdgeBatch) -> Result<(), ModelError> {
        let m = self.n_edges();
        let count = batch.len();
        if batch.dst.len() != count
            || batch.labels.len() != count
            || batch.rows.as_ref().is_some_and(|r| r.len() != count)
        {
            return err(format!(
                "{count} src, but {} dst, {} labels and {} rows",
                batch.dst.len(),
                batch.labels.len(),
                batch.rows.as_ref().map_or(count, Vec::len)
            ));
        }
        if m + count > NULL as usize {
            return err("more edges than a u32 can number");
        }
        let labels = Some(&batch.labels[..]);
        self.check_edges(&batch.src, &batch.dst, labels, self.n_nodes(), m)?;
        check_batch_columns(&self.edge_columns, &batch.columns, count, "edge")?;

        self.src.extend_from_slice(&batch.src);
        self.dst.extend_from_slice(&batch.dst);
        self.edge_label.extend_from_slice(&batch.labels);
        self.edge_flags.extend_zeros(count);
        let known = batch
            .rows
            .as_ref()
            .filter(|rows| rows.iter().any(|&r| r != NO_ROW));
        if self.edge_rows.is_some() || known.is_some() {
            let rows = self.edge_rows.get_or_insert_with(|| vec![NO_ROW; m]);
            match known {
                Some(new) => rows.extend_from_slice(new),
                None => rows.resize(m + count, NO_ROW),
            }
        }
        extend_columns(&mut self.edge_columns, &batch.columns, count);
        self.note_named(m);
        Ok(())
    }

    /// Remove or restore `edges`. While no edge is removed nothing is held
    /// for their flags, and a restore that cannot apply does not change
    /// that.
    fn flag_edges(&mut self, edges: &[u32], remove: bool) -> Result<(), ModelError> {
        let m = self.n_edges();
        if !remove && self.edge_flags.as_slice().is_none() {
            return match edges.first() {
                None => Ok(()),
                Some(&e) if (e as usize) < m => err(format!("edge {e} is not removed")),
                Some(&e) => err(format!("edge {e}, but the result has {m} edges")),
            };
        }
        if edges.is_empty() {
            return Ok(());
        }
        set_flags(self.edge_flags.make_mut(), edges, remove, "edge")
    }

    fn pop_edges(&mut self, batch: &EdgeBatch) -> Result<(), ModelError> {
        let count = batch.len();
        let Some(m) = self.n_edges().checked_sub(count) else {
            return err(format!(
                "{count} edges to take off, but the result has {}",
                self.n_edges()
            ));
        };
        let rows_match = (0..count).all(|i| {
            let row = batch.rows.as_ref().and_then(|r| r.get(i)).copied();
            self.edge_row(m + i) == row.filter(|&r| r != NO_ROW)
        });
        if batch.dst.len() != count
            || self.src[m..] != batch.src[..]
            || self.dst[m..] != batch.dst[..]
            || !self.edge_label.ends_with(m, &batch.labels)
            || !rows_match
        {
            return err("the last edges are not the ones this op takes off");
        }
        if self.edge_flags.any_from(m) {
            return err("one of the last edges is removed; restore it first");
        }
        check_tail_columns(&self.edge_columns, &batch.columns, m, "edge")?;
        let named = |p: &super::Path| p.edges.iter().any(|&e| e as usize >= m);
        if self.highlights.paths.iter().any(named) {
            return err("a path still names one of the last edges");
        }

        self.src.truncate(m);
        self.dst.truncate(m);
        self.forget_named(m);
        self.edge_label.truncate(m);
        self.edge_flags.truncate(m);
        if let Some(rows) = &mut self.edge_rows {
            rows.truncate(m);
            if rows.iter().all(|&r| r == NO_ROW) {
                self.edge_rows = None;
            }
        }
        pop_columns(&mut self.edge_columns, &batch.columns, m);
        Ok(())
    }

    fn set_column(
        &mut self,
        target: Target,
        index: usize,
        old: Option<&Column>,
        new: Option<&Column>,
    ) -> Result<(), ModelError> {
        let noun = target.noun();
        let rows = self.rows(target);
        let columns = self.columns(target);
        let name = match (old, new) {
            (None, None) => return err("neither an old column nor a new one"),
            (Some(old), Some(new)) if old.name != new.name => {
                return err(format!(
                    "it replaces the column '{}' with one named '{}'",
                    old.name, new.name
                ))
            }
            (Some(c), _) | (_, Some(c)) => &c.name,
        };
        match old {
            Some(old) => {
                if columns.get(index) != Some(old) {
                    return err(format!(
                        "{noun} column {index} is not the '{name}' this op replaces"
                    ));
                }
            }
            None => {
                if index > columns.len() {
                    return err(format!(
                        "a new column at {index}, but there are {} {noun} columns",
                        columns.len()
                    ));
                }
                if columns.iter().any(|c| c.name == *name) {
                    return err(format!("there is a {noun} column '{name}' already"));
                }
            }
        }
        if let Some(new) = new {
            new.values
                .check(rows, &format!("the {noun} column '{name}'"))?;
        }

        let columns = self.columns_mut(target);
        match (old, new) {
            (Some(_), Some(new)) => columns[index] = new.clone(),
            (None, Some(new)) => columns.insert(index, new.clone()),
            (Some(_), None) => drop(columns.remove(index)),
            (None, None) => unreachable!(),
        }
        // A highlight or an encoding may name the column: it has to stay,
        // and stay of a type they can use.
        let still_fits = self
            .check_highlights(&self.highlights)
            .and_then(|()| self.check_encodings(&self.encodings));
        if let Err(e) = still_fits {
            let columns = self.columns_mut(target);
            match (old, new) {
                (Some(old), Some(_)) => columns[index] = old.clone(),
                (None, Some(_)) => drop(columns.remove(index)),
                (Some(old), None) => columns.insert(index, old.clone()),
                (None, None) => unreachable!(),
            }
            return Err(e);
        }
        Ok(())
    }
}

fn check_positions(p: &Positions, count: usize) -> Result<(), ModelError> {
    if p.x.len() != count || p.y.len() != count {
        return err(format!(
            "{count} nodes, but {} x and {} y positions",
            p.x.len(),
            p.y.len()
        ));
    }
    Ok(())
}

/// Set or clear [`REMOVED`] on `indexes`, each of which must be in range,
/// listed once, and in the other state.
fn set_flags(
    flags: &mut [u8],
    indexes: &[u32],
    remove: bool,
    noun: &str,
) -> Result<(), ModelError> {
    let mut seen = HashSet::with_capacity(indexes.len());
    for &i in indexes {
        let Some(flag) = flags.get(i as usize) else {
            return err(format!(
                "{noun} {i}, but the result has {} {noun}s",
                flags.len()
            ));
        };
        if (flag & REMOVED != 0) == remove {
            let state = if remove {
                "removed already"
            } else {
                "not removed"
            };
            return err(format!("{noun} {i} is {state}"));
        }
        if !seen.insert(i) {
            return err(format!("{noun} {i} is listed twice"));
        }
    }
    for &i in indexes {
        flags[i as usize] ^= REMOVED;
    }
    Ok(())
}

/// A batch's columns fit the result's: each names a column of the same
/// type, once, with `count` values and codes the dictionary will have.
fn check_batch_columns(
    columns: &[Column],
    batch: &[BatchColumn],
    count: usize,
    noun: &str,
) -> Result<(), ModelError> {
    for (i, b) in batch.iter().enumerate() {
        let what = format!("the {noun} column '{}'", b.name);
        if batch[..i].iter().any(|c| c.name == b.name) {
            return err(format!("the batch has {what} twice"));
        }
        let Some(column) = columns.iter().find(|c| c.name == b.name) else {
            return err(format!("the result does not have {what}; add it first"));
        };
        if b.values.len() != count {
            return err(format!(
                "{what} has {} values for {count} new {noun}s",
                b.values.len()
            ));
        }
        match (&column.values, &b.values) {
            (Values::F32(_), BatchValues::F32(_)) | (Values::U32(_), BatchValues::U32(_)) => {}
            (Values::Dict { values, .. }, BatchValues::Dict { new_values, codes }) => {
                let n_values = values.len() + new_values.len();
                if let Some(code) = codes.iter().find(|&&c| c != NULL && c as usize >= n_values) {
                    return err(format!(
                        "{what} has the code {code}, but only {n_values} values"
                    ));
                }
            }
            (have, _) => {
                return err(format!(
                    "{what} is {}, but the batch's values are {}",
                    have.type_name(),
                    b.values.type_name()
                ))
            }
        }
    }
    Ok(())
}

/// Append a checked batch's values; nulls where it leaves a column out.
fn extend_columns(columns: &mut [Column], batch: &[BatchColumn], count: usize) {
    for column in columns {
        let new = batch.iter().find(|b| b.name == column.name);
        match (&mut column.values, new.map(|b| &b.values)) {
            (Values::F32(v), Some(BatchValues::F32(new))) => v.extend_from_slice(new),
            (Values::U32(v), Some(BatchValues::U32(new))) => v.extend_from_slice(new),
            (
                Values::Dict { values, codes },
                Some(BatchValues::Dict {
                    new_values,
                    codes: new,
                }),
            ) => {
                values.extend(new_values);
                codes.extend_from_slice(new);
            }
            (Values::F32(v), None) => v.resize(v.len() + count, f32::NAN),
            (Values::U32(v), None) | (Values::Dict { codes: v, .. }, None) => {
                v.resize(v.len() + count, NULL)
            }
            _ => unreachable!("check_batch_columns matched the types"),
        }
    }
}

/// The rows from `len` on are the batch's, in every column it names.
fn check_tail_columns(
    columns: &[Column],
    batch: &[BatchColumn],
    len: usize,
    noun: &str,
) -> Result<(), ModelError> {
    for b in batch {
        let column = columns.iter().find(|c| c.name == b.name);
        let same = match (column.map(|c| &c.values), &b.values) {
            (Some(Values::F32(v)), BatchValues::F32(tail)) => bits_eq(&v[len..], tail),
            (Some(Values::U32(v)), BatchValues::U32(tail)) => v[len..] == tail[..],
            (
                Some(Values::Dict { values, codes }),
                BatchValues::Dict {
                    new_values,
                    codes: tail,
                },
            ) => codes[len..] == tail[..] && values.ends_with(new_values),
            _ => false,
        };
        if !same {
            return err(format!(
                "the last {noun}s' values of '{}' are not the ones this op takes off",
                b.name
            ));
        }
    }
    Ok(())
}

/// Cut every column to `len` rows, and the dictionaries by the values the
/// batch had added.
fn pop_columns(columns: &mut [Column], batch: &[BatchColumn], len: usize) {
    for column in columns {
        let tail = batch.iter().find(|b| b.name == column.name);
        match &mut column.values {
            Values::F32(v) => v.truncate(len),
            Values::U32(v) => v.truncate(len),
            Values::Dict { values, codes } => {
                codes.truncate(len);
                if let Some(BatchValues::Dict { new_values, .. }) = tail.map(|b| &b.values) {
                    values.truncate(values.len() - new_values.len());
                }
            }
        }
    }
}
