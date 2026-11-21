//! [`GraphResult`]: the nodes, edges, highlights and provenance of one result.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// The null of a `u32` column and of a dictionary column's codes.
pub const NULL: u32 = u32::MAX;

/// An edge whose row in its edge table the engine does not know.
pub const NO_ROW: u64 = u64::MAX;

/// A broken invariant of the model, or a delta that does not fit the result
/// it was applied to. The message says which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError(pub(crate) String);

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModelError {}

pub(crate) fn err<T>(message: impl Into<String>) -> Result<T, ModelError> {
    Err(ModelError(message.into()))
}

/// A node's key in its vertex table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Int(i64),
    Text(String),
}

impl Key {
    /// Read `text` as a key of type `ty`. An integer key's text is its
    /// canonical decimal form: `"7"`, never `"+7"` or `"07"`.
    pub fn parse(ty: KeyType, text: &str) -> Result<Self, ModelError> {
        match ty {
            KeyType::Text => Ok(Key::Text(text.to_string())),
            KeyType::Int => match text.parse::<i64>() {
                Ok(v) if v.to_string() == text => Ok(Key::Int(v)),
                _ => err(format!("the key '{text}' is not an integer")),
            },
        }
    }

    pub fn key_type(&self) -> KeyType {
        match self {
            Key::Int(_) => KeyType::Int,
            Key::Text(_) => KeyType::Text,
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Int(v) => write!(f, "{v}"),
            Key::Text(v) => f.write_str(v),
        }
    }
}

impl From<i64> for Key {
    fn from(v: i64) -> Self {
        Key::Int(v)
    }
}

impl From<&str> for Key {
    fn from(v: &str) -> Self {
        Key::Text(v.to_string())
    }
}

impl From<String> for Key {
    fn from(v: String) -> Self {
        Key::Text(v)
    }
}

/// The type of a vertex table's key column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyType {
    Int,
    Text,
}

/// A vertex table: its name, and the type of its keys. A result that is not
/// part of a property graph has one label, named `""`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeLabel {
    pub name: String,
    pub key: KeyType,
}

impl NodeLabel {
    pub fn new(name: impl Into<String>, key: KeyType) -> Self {
        Self {
            name: name.into(),
            key,
        }
    }
}

/// What a column is, as the engine's `ursa.role` field metadata says it
/// (design §3). A column of a result is usually a `Metric`, a `Category`, a
/// `Property` or a `Distance`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Node,
    Src,
    Dst,
    Seed,
    Distance,
    PathNodes,
    PathEdges,
    PathElements,
    Metric,
    Category,
    Property,
}

/// Strings stored as the wire format stores them: one run of UTF-8 and the
/// offset of each string's start, plus the end of the last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strings {
    pub(crate) offsets: Vec<u32>,
    pub(crate) bytes: Vec<u8>,
}

impl Default for Strings {
    fn default() -> Self {
        Self {
            offsets: vec![0],
            bytes: Vec::new(),
        }
    }
}

impl Strings {
    pub fn new() -> Self {
        Self::default()
    }

    /// From the wire's two arrays: `len + 1` offsets, rising from 0 to
    /// `bytes.len()`, each on a character boundary of valid UTF-8.
    pub fn from_parts(offsets: Vec<u32>, bytes: Vec<u8>) -> Result<Self, ModelError> {
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return err("the text is not UTF-8");
        };
        if offsets.first() != Some(&0) || offsets.last().map(|&o| o as usize) != Some(bytes.len()) {
            return err("the text's offsets do not run from 0 to its length");
        }
        for pair in offsets.windows(2) {
            if pair[0] > pair[1] || !text.is_char_boundary(pair[1] as usize) {
                return err("the text's offsets do not rise, or split a character");
            }
        }
        Ok(Self { offsets, bytes })
    }

    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Panics past 4 GiB of text, which the format's `u32` offsets cannot
    /// address.
    pub fn push(&mut self, text: &str) {
        self.bytes.extend_from_slice(text.as_bytes());
        let end = u32::try_from(self.bytes.len()).expect("more than 4 GiB of text");
        self.offsets.push(end);
    }

    pub fn get(&self, i: usize) -> &str {
        let range = self.offsets[i] as usize..self.offsets[i + 1] as usize;
        // Checked when built: `push` takes a `&str`, `from_parts` validates.
        std::str::from_utf8(&self.bytes[range]).expect("Strings holds UTF-8")
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        (0..self.len()).map(|i| self.get(i))
    }

    pub fn truncate(&mut self, len: usize) {
        if len < self.len() {
            self.offsets.truncate(len + 1);
            self.bytes.truncate(self.offsets[len] as usize);
        }
    }

    pub(crate) fn extend(&mut self, other: &Strings) {
        for text in other.iter() {
            self.push(text);
        }
    }

    /// Whether the last `tail.len()` strings are `tail`'s.
    pub(crate) fn ends_with(&self, tail: &Strings) -> bool {
        let Some(start) = self.len().checked_sub(tail.len()) else {
            return false;
        };
        self.bytes[self.offsets[start] as usize..] == tail.bytes[..]
            && (0..tail.len()).all(|i| self.get(start + i).len() == tail.get(i).len())
    }

    /// The offsets and the UTF-8, as the wire format writes them.
    pub fn parts(&self) -> (&[u32], &[u8]) {
        (&self.offsets, &self.bytes)
    }
}

impl<S: AsRef<str>> FromIterator<S> for Strings {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        let mut out = Strings::new();
        for text in iter {
            out.push(text.as_ref());
        }
        out
    }
}

/// A column's values, one per node or per edge.
///
/// Nulls: NaN in `F32`, [`NULL`] in `U32` and in a dictionary's codes.
/// Two columns are equal when their bits are, so a NaN equals itself.
#[derive(Debug, Clone)]
pub enum Values {
    F32(Vec<f32>),
    U32(Vec<u32>),
    /// Text, as codes into the distinct values.
    Dict {
        values: Strings,
        codes: Vec<u32>,
    },
}

pub(crate) fn bits_eq(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

impl PartialEq for Values {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Values::F32(a), Values::F32(b)) => bits_eq(a, b),
            (Values::U32(a), Values::U32(b)) => a == b,
            (
                Values::Dict { values, codes },
                Values::Dict {
                    values: v,
                    codes: c,
                },
            ) => values == v && codes == c,
            _ => false,
        }
    }
}

impl Values {
    /// A dictionary column from text, `None` for null. The values are in
    /// order of first appearance.
    pub fn dict<'a>(texts: impl IntoIterator<Item = Option<&'a str>>) -> Self {
        let mut values = Strings::new();
        let mut seen: HashMap<&str, u32> = HashMap::new();
        let mut codes = Vec::new();
        for text in texts {
            codes.push(match text {
                None => NULL,
                Some(text) => *seen.entry(text).or_insert_with(|| {
                    values.push(text);
                    values.len() as u32 - 1
                }),
            });
        }
        Values::Dict { values, codes }
    }

    pub fn len(&self) -> usize {
        match self {
            Values::F32(v) => v.len(),
            Values::U32(v) => v.len(),
            Values::Dict { codes, .. } => codes.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `"f32"`, `"u32"` or `"dict"`: the wire format's name of the type.
    pub fn type_name(&self) -> &'static str {
        match self {
            Values::F32(_) => "f32",
            Values::U32(_) => "u32",
            Values::Dict { .. } => "dict",
        }
    }

    /// Row `i` of a dictionary column as text; `None` for a null and for
    /// the other types.
    pub fn text(&self, i: usize) -> Option<&str> {
        match self {
            Values::Dict { values, codes } if codes[i] != NULL => {
                Some(values.get(codes[i] as usize))
            }
            _ => None,
        }
    }

    pub(crate) fn check(&self, len: usize, what: &str) -> Result<(), ModelError> {
        if self.len() != len {
            return err(format!("{what} has {} values, for {len} rows", self.len()));
        }
        if let Values::Dict { values, codes } = self {
            check_codes(codes, values.len(), what)?;
        }
        Ok(())
    }
}

fn check_codes(codes: &[u32], n_values: usize, what: &str) -> Result<(), ModelError> {
    match codes.iter().find(|&&c| c != NULL && c as usize >= n_values) {
        Some(code) => err(format!(
            "{what} has the code {code}, but only {n_values} values"
        )),
        None => Ok(()),
    }
}

/// A named column of the nodes or of the edges: a property or a metric.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub role: Option<Role>,
    pub values: Values,
}

impl Column {
    pub fn new(name: impl Into<String>, role: Option<Role>, values: Values) -> Self {
        Self {
            name: name.into(),
            role,
            values,
        }
    }
}

/// Whether a column belongs to the nodes or to the edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Nodes,
    Edges,
}

/// A place per node. Equal when the bits are.
#[derive(Debug, Clone, Default)]
pub struct Positions {
    pub x: Vec<f32>,
    pub y: Vec<f32>,
}

impl PartialEq for Positions {
    fn eq(&self, other: &Self) -> bool {
        bits_eq(&self.x, &other.x) && bits_eq(&self.y, &other.y)
    }
}

/// A path: its nodes in order, and the edge between each two. `edges` is
/// empty when the result names only the nodes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Path {
    pub nodes: Vec<u32>,
    pub edges: Vec<u32>,
}

/// What a result points at: paths, seeds, and each node's distance from the
/// seeds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Highlights {
    pub paths: Vec<Path>,
    pub seeds: Vec<u32>,
    /// The `u32` node column that holds the distances ([`NULL`]: not
    /// reached). A column, so that it grows with the nodes like any other.
    pub distance: Option<String>,
}

impl Highlights {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.seeds.is_empty() && self.distance.is_none()
    }
}

/// The layout a result asks for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    #[default]
    Force,
    Rings,
    Layers,
}

/// How columns are drawn. Each names a node column.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Encodings {
    /// An `f32` or `u32` column.
    pub size_by: Option<String>,
    pub color_by: Option<String>,
    pub group_by: Option<String>,
    pub layout: Layout,
}

/// Where a result, or a delta, came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// The SQL that produced it.
    pub sql: Option<String>,
    /// The result → graph rule that fired (`"path"`, `"rings"`, `"edges"`, …).
    pub rule: Option<String>,
    /// The columns of the SQL's result that the rule read, and what each
    /// was read as. Left out of the wire format when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<ColumnRole>,
    /// What a status line adds to the rule: what was left out, and what else
    /// the result could have been drawn as. Left out when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A column of a result, and what a rule read it as: the engine's
/// `ursa.role` (`"node"`, `"src"`, `"path_nodes"`, `"step"`, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnRole {
    pub column: String,
    pub role: String,
}

impl ColumnRole {
    pub fn new(column: impl Into<String>, role: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            role: role.into(),
        }
    }
}

/// Flag bit: the node or edge was removed. It keeps its slot and its data,
/// so every index stays valid and restoring it needs nothing else.
pub const REMOVED: u8 = 1;

/// Nodes, edges, highlights, and where they came from: everything drawn.
///
/// See the [module docs](super) for the invariants. A result is built and
/// changed only by [`apply`](GraphResult::apply)ing deltas, and read through
/// the accessors here.
#[derive(Debug, Clone, Default)]
pub struct GraphResult {
    pub(crate) graph: Option<String>,
    pub(crate) node_labels: Vec<NodeLabel>,
    pub(crate) edge_labels: Vec<String>,

    pub(crate) node_keys: Strings,
    pub(crate) node_label: Vec<u16>,
    pub(crate) node_flags: Vec<u8>,
    pub(crate) node_columns: Vec<Column>,
    pub(crate) positions: Option<Positions>,

    pub(crate) src: Vec<u32>,
    pub(crate) dst: Vec<u32>,
    pub(crate) edge_label: Vec<u16>,
    /// `None` when no edge's row is known; else one per edge, [`NO_ROW`]
    /// for the unknown, and never all of them unknown.
    pub(crate) edge_rows: Option<Vec<u64>>,
    pub(crate) edge_flags: Vec<u8>,
    pub(crate) edge_columns: Vec<Column>,

    pub(crate) highlights: Highlights,
    pub(crate) encodings: Encodings,
    pub(crate) provenance: Provenance,

    /// `(label, key)` → node index. Derived from the keys and labels.
    pub(crate) index: HashMap<(u16, Key), u32>,
}

impl PartialEq for GraphResult {
    fn eq(&self, other: &Self) -> bool {
        // All but `index`, which the rest determines.
        self.graph == other.graph
            && self.node_labels == other.node_labels
            && self.edge_labels == other.edge_labels
            && self.node_keys == other.node_keys
            && self.node_label == other.node_label
            && self.node_flags == other.node_flags
            && self.node_columns == other.node_columns
            && self.positions == other.positions
            && self.src == other.src
            && self.dst == other.dst
            && self.edge_label == other.edge_label
            && self.edge_rows == other.edge_rows
            && self.edge_flags == other.edge_flags
            && self.edge_columns == other.edge_columns
            && self.highlights == other.highlights
            && self.encodings == other.encodings
            && self.provenance == other.provenance
    }
}

impl GraphResult {
    /// An empty result: no labels, nodes or edges. `graph` is the property
    /// graph it is part of, if any.
    pub fn new(graph: Option<String>, provenance: Provenance) -> Self {
        Self {
            graph,
            provenance,
            ..Self::default()
        }
    }

    /// The property graph this is part of.
    pub fn graph(&self) -> Option<&str> {
        self.graph.as_deref()
    }

    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    pub fn encodings(&self) -> &Encodings {
        &self.encodings
    }

    pub fn highlights(&self) -> &Highlights {
        &self.highlights
    }

    // Nodes.

    /// How many node slots there are, removed ones included.
    pub fn n_nodes(&self) -> usize {
        self.node_label.len()
    }

    pub fn node_labels(&self) -> &[NodeLabel] {
        &self.node_labels
    }

    /// Each node's label, as an index into [`node_labels`](Self::node_labels).
    pub fn node_label(&self) -> &[u16] {
        &self.node_label
    }

    /// Every node's key as text.
    pub fn node_keys(&self) -> &Strings {
        &self.node_keys
    }

    /// Node `i`'s key, typed as its label's keys are.
    pub fn node_key(&self, i: usize) -> Key {
        let text = self.node_keys.get(i);
        match self.node_labels[self.node_label[i] as usize].key {
            KeyType::Text => Key::Text(text.to_string()),
            // Checked when the node was added.
            KeyType::Int => Key::Int(text.parse().expect("an integer key")),
        }
    }

    /// The node with this identity, removed or not.
    pub fn node(&self, label: u16, key: &Key) -> Option<u32> {
        // The map's key is a pair, so the lookup needs one; a key is small.
        self.index.get(&(label, key.clone())).copied()
    }

    /// Each node's flags ([`REMOVED`]).
    pub fn node_flags(&self) -> &[u8] {
        &self.node_flags
    }

    pub fn node_is_removed(&self, i: usize) -> bool {
        self.node_flags[i] & REMOVED != 0
    }

    pub fn node_columns(&self) -> &[Column] {
        &self.node_columns
    }

    pub fn node_column(&self, name: &str) -> Option<&Column> {
        self.node_columns.iter().find(|c| c.name == name)
    }

    pub fn positions(&self) -> Option<&Positions> {
        self.positions.as_ref()
    }

    // Edges.

    /// How many edge slots there are, removed ones included.
    pub fn n_edges(&self) -> usize {
        self.src.len()
    }

    pub fn edge_labels(&self) -> &[String] {
        &self.edge_labels
    }

    pub fn src(&self) -> &[u32] {
        &self.src
    }

    pub fn dst(&self) -> &[u32] {
        &self.dst
    }

    /// Each edge's label, as an index into [`edge_labels`](Self::edge_labels).
    pub fn edge_label(&self) -> &[u16] {
        &self.edge_label
    }

    /// Edge `i`'s row in its edge table, where the engine knew it. With its
    /// label, that is the edge's identity; without, the identity is its
    /// endpoints and its place among their parallel edges.
    pub fn edge_row(&self, i: usize) -> Option<u64> {
        let row = self.edge_rows.as_ref().map_or(NO_ROW, |rows| rows[i]);
        (row != NO_ROW).then_some(row)
    }

    /// Every edge's row ([`NO_ROW`] for the unknown), or `None` when none
    /// is known.
    pub fn edge_rows(&self) -> Option<&[u64]> {
        self.edge_rows.as_deref()
    }

    /// Each edge's flags ([`REMOVED`]).
    pub fn edge_flags(&self) -> &[u8] {
        &self.edge_flags
    }

    /// Whether edge `i` is to be drawn: it is not removed, and neither end
    /// is. So removing a node hides its edges, and restoring it shows them.
    pub fn edge_is_shown(&self, i: usize) -> bool {
        self.edge_flags[i] & REMOVED == 0
            && !self.node_is_removed(self.src[i] as usize)
            && !self.node_is_removed(self.dst[i] as usize)
    }

    pub fn edge_columns(&self) -> &[Column] {
        &self.edge_columns
    }

    pub fn edge_column(&self, name: &str) -> Option<&Column> {
        self.edge_columns.iter().find(|c| c.name == name)
    }

    pub fn columns(&self, target: Target) -> &[Column] {
        match target {
            Target::Nodes => &self.node_columns,
            Target::Edges => &self.edge_columns,
        }
    }

    pub(crate) fn columns_mut(&mut self, target: Target) -> &mut Vec<Column> {
        match target {
            Target::Nodes => &mut self.node_columns,
            Target::Edges => &mut self.edge_columns,
        }
    }

    pub(crate) fn rows(&self, target: Target) -> usize {
        match target {
            Target::Nodes => self.n_nodes(),
            Target::Edges => self.n_edges(),
        }
    }

    // Invariants.

    /// Check every invariant, and rebuild the key index. `apply` keeps them,
    /// so this is for a result made another way: the wire format's decoder
    /// calls it.
    pub fn validate(&mut self) -> Result<(), ModelError> {
        let n = self.n_nodes();
        let m = self.n_edges();
        if self.node_labels.len() > u16::MAX as usize || self.edge_labels.len() > u16::MAX as usize
        {
            return err("more labels than a u16 can number");
        }
        if n > NULL as usize || m > NULL as usize {
            return err("more nodes or edges than a u32 can number");
        }
        if self.node_keys.len() != n || self.node_flags.len() != n {
            return err(format!(
                "{n} nodes, but {} keys and {} flags",
                self.node_keys.len(),
                self.node_flags.len()
            ));
        }
        self.index.clear();
        self.index.reserve(n);
        for i in 0..n {
            let label = self.node_label[i];
            let Some(node_label) = self.node_labels.get(label as usize) else {
                return err(format!(
                    "node {i} has the label {label}, but the result has {} node labels",
                    self.node_labels.len()
                ));
            };
            let key = Key::parse(node_label.key, self.node_keys.get(i))
                .map_err(|e| ModelError(format!("node {i}: {e}")))?;
            if let Some(first) = self.index.insert((label, key), i as u32) {
                return err(format!(
                    "nodes {first} and {i} are both '{}' of the label '{}'",
                    self.node_keys.get(i),
                    node_label.name
                ));
            }
        }
        if let Some(p) = &self.positions {
            if p.x.len() != n || p.y.len() != n {
                return err(format!(
                    "{n} nodes, but {} x and {} y positions",
                    p.x.len(),
                    p.y.len()
                ));
            }
        }

        if self.dst.len() != m || self.edge_label.len() != m || self.edge_flags.len() != m {
            return err(format!(
                "{m} edges, but {} dst, {} labels and {} flags",
                self.dst.len(),
                self.edge_label.len(),
                self.edge_flags.len()
            ));
        }
        self.check_edges(&self.src, &self.dst, &self.edge_label, n, 0)?;
        if let Some(rows) = &self.edge_rows {
            if rows.len() != m {
                return err(format!("{m} edges, but {} rows", rows.len()));
            }
            if rows.iter().all(|&r| r == NO_ROW) {
                return err("the edges' rows are present, but none is known");
            }
        }

        for target in [Target::Nodes, Target::Edges] {
            let columns = self.columns(target);
            for (i, column) in columns.iter().enumerate() {
                let what = format!("the {} column '{}'", target.noun(), column.name);
                column.values.check(self.rows(target), &what)?;
                if columns[..i].iter().any(|c| c.name == column.name) {
                    return err(format!("there are two of {what}"));
                }
            }
        }
        self.check_highlights(&self.highlights)?;
        self.check_encodings(&self.encodings)
    }

    /// The edges `src[i] → dst[i]` name nodes below `n_nodes` and labels the
    /// result has. `first` is the index of the first, for the message.
    pub(crate) fn check_edges(
        &self,
        src: &[u32],
        dst: &[u32],
        label: &[u16],
        n_nodes: usize,
        first: usize,
    ) -> Result<(), ModelError> {
        for (i, ((&s, &d), &l)) in src.iter().zip(dst).zip(label).enumerate() {
            for node in [s, d] {
                if node as usize >= n_nodes {
                    return err(format!(
                        "edge {} names node {node}, but the result has {n_nodes} nodes",
                        first + i
                    ));
                }
            }
            if l as usize >= self.edge_labels.len() {
                return err(format!(
                    "edge {} has the label {l}, but the result has {} edge labels",
                    first + i,
                    self.edge_labels.len()
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn check_highlights(&self, h: &Highlights) -> Result<(), ModelError> {
        let n = self.n_nodes();
        if let Some(&seed) = h.seeds.iter().find(|&&s| s as usize >= n) {
            return err(format!(
                "a seed is node {seed}, but the result has {n} nodes"
            ));
        }
        for (p, path) in h.paths.iter().enumerate() {
            if path.nodes.is_empty() {
                return err(format!("path {p} has no nodes"));
            }
            if let Some(&node) = path.nodes.iter().find(|&&v| v as usize >= n) {
                return err(format!(
                    "path {p} names node {node}, but the result has {n} nodes"
                ));
            }
            if path.edges.is_empty() {
                continue;
            }
            if path.edges.len() != path.nodes.len() - 1 {
                return err(format!(
                    "path {p} has {} nodes and {} edges; it needs one edge fewer than nodes, or none",
                    path.nodes.len(),
                    path.edges.len()
                ));
            }
            for (step, &e) in path.edges.iter().enumerate() {
                let (a, b) = (path.nodes[step], path.nodes[step + 1]);
                let Some((&s, &d)) = self.src.get(e as usize).zip(self.dst.get(e as usize)) else {
                    return err(format!(
                        "path {p} names edge {e}, but the result has {} edges",
                        self.n_edges()
                    ));
                };
                if (s, d) != (a, b) && (s, d) != (b, a) {
                    return err(format!(
                        "path {p}: edge {e} joins nodes {s} and {d}, not {a} and {b}"
                    ));
                }
            }
        }
        if let Some(name) = &h.distance {
            match self.node_column(name).map(|c| &c.values) {
                Some(Values::U32(_)) => {}
                Some(_) => return err(format!("the distance column '{name}' is not a u32 column")),
                None => {
                    return err(format!(
                        "there is no node column '{name}' for the distances"
                    ))
                }
            }
        }
        Ok(())
    }

    pub(crate) fn check_encodings(&self, e: &Encodings) -> Result<(), ModelError> {
        for (what, name) in [
            ("size", &e.size_by),
            ("colour", &e.color_by),
            ("group", &e.group_by),
        ] {
            let Some(name) = name else { continue };
            match self.node_column(name).map(|c| &c.values) {
                None => return err(format!("there is no node column '{name}' to {what} by")),
                Some(Values::Dict { .. }) if what == "size" => {
                    return err(format!(
                        "the column '{name}' is text, so it cannot size the nodes"
                    ))
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

impl Target {
    pub(crate) fn noun(self) -> &'static str {
        match self {
            Target::Nodes => "node",
            Target::Edges => "edge",
        }
    }
}
