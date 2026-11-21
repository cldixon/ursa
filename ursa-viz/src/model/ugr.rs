//! UGR, the wire format of a [`GraphResult`] and a [`GraphDelta`]: a JSON
//! header, then aligned little-endian arrays. `ursa-viz/UGR.md` is the
//! byte-level specification; `ursa-viz/ui/ugr.js` reads it in JavaScript.
//!
//! Encoding is canonical: equal values give equal bytes, so decoding what
//! the encoder wrote and encoding it again gives the same buffer.

use std::collections::HashMap;
use std::fmt;

use bytemuck::Pod;
use serde::{Deserialize, Serialize};

use super::delta::{BatchColumn, BatchValues, EdgeBatch, GraphDelta, NodeBatch, Op};
use super::result::{
    Column, Encodings, GraphResult, Highlights, ModelError, NodeLabel, Path, Positions, Provenance,
    Role, Strings, Target, Values, NO_ROW,
};

/// The first four bytes of a UGR buffer; the digit is the format's version.
pub const MAGIC: [u8; 4] = *b"UGR1";

/// The header and every section start at a multiple of this many bytes.
pub const ALIGN: usize = 8;

/// A buffer that is not a UGR this reader can decode. The message says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UgrError(String);

impl fmt::Display for UgrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UGR: {}", self.0)
    }
}

impl std::error::Error for UgrError {}

impl From<ModelError> for UgrError {
    fn from(e: ModelError) -> Self {
        UgrError(e.to_string())
    }
}

fn bad<T>(message: impl Into<String>) -> Result<T, UgrError> {
    Err(UgrError(message.into()))
}

/// What a UGR buffer holds.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Result(GraphResult),
    Delta(GraphDelta),
}

/// Decode a buffer that may be a result or a delta.
pub fn decode(bytes: &[u8]) -> Result<Message, UgrError> {
    let (header, reader) = open(bytes)?;
    match header {
        Header::Result(h) => Ok(Message::Result(read_result(h, &reader)?)),
        Header::Delta(h) => Ok(Message::Delta(read_delta(h, &reader)?)),
    }
}

impl GraphResult {
    /// The result as one UGR buffer.
    pub fn to_bytes(&self) -> Vec<u8> {
        write_result(self)
    }

    /// Decode a result, checking every invariant of the model.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, UgrError> {
        match decode(bytes)? {
            Message::Result(result) => Ok(result),
            Message::Delta(_) => bad("the buffer holds a delta, not a result"),
        }
    }
}

impl GraphDelta {
    /// The delta as one UGR buffer.
    pub fn to_bytes(&self) -> Vec<u8> {
        write_delta(self)
    }

    /// Decode a delta. Whether it fits a result is for
    /// [`GraphResult::apply`] to say.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, UgrError> {
        match decode(bytes)? {
            Message::Delta(delta) => Ok(delta),
            Message::Result(_) => bad("the buffer holds a result, not a delta"),
        }
    }
}

// The header, as JSON.

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Header {
    Result(ResultH),
    Delta(DeltaH),
}

#[derive(Serialize, Deserialize)]
struct ResultH {
    graph: Option<String>,
    nodes: NodesH,
    edges: EdgesH,
    highlights: HighlightsH,
    encodings: Encodings,
    provenance: Provenance,
    body: u64,
    sections: Vec<SectionH>,
}

#[derive(Serialize, Deserialize)]
struct DeltaH {
    provenance: Provenance,
    ops: Vec<OpH>,
    body: u64,
    sections: Vec<SectionH>,
}

#[derive(Serialize, Deserialize)]
struct SectionH {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    /// From the start of the body, in bytes.
    offset: u64,
    /// In elements.
    count: u64,
}

#[derive(Serialize, Deserialize)]
struct NodesH {
    count: u64,
    labels: Vec<NodeLabel>,
    columns: Vec<ColumnH>,
    positions: bool,
}

#[derive(Serialize, Deserialize)]
struct EdgesH {
    count: u64,
    labels: Vec<String>,
    columns: Vec<ColumnH>,
}

#[derive(Serialize, Deserialize)]
struct ColumnH {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    role: Option<Role>,
}

#[derive(Serialize, Deserialize)]
struct BatchColumnH {
    name: String,
    #[serde(rename = "type")]
    ty: String,
}

#[derive(Serialize, Deserialize)]
struct HighlightsH {
    paths: u64,
    seeds: u64,
    distance: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct NodeBatchH {
    count: u64,
    columns: Vec<BatchColumnH>,
    positions: bool,
}

#[derive(Serialize, Deserialize)]
struct EdgeBatchH {
    count: u64,
    columns: Vec<BatchColumnH>,
    rows: bool,
}

#[derive(Serialize, Deserialize)]
struct LabelH {
    name: String,
}

#[derive(Serialize, Deserialize)]
struct IndexH {
    count: u64,
}

/// The names are [`Op::name`]'s.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum OpH {
    AddNodeLabel(NodeLabel),
    PopNodeLabel(NodeLabel),
    AddEdgeLabel(LabelH),
    PopEdgeLabel(LabelH),
    AddNodes(NodeBatchH),
    PopNodes(NodeBatchH),
    AddEdges(EdgeBatchH),
    PopEdges(EdgeBatchH),
    RemoveNodes(IndexH),
    RestoreNodes(IndexH),
    RemoveEdges(IndexH),
    RestoreEdges(IndexH),
    SetColumn {
        target: Target,
        index: u32,
        old: Option<ColumnH>,
        new: Option<ColumnH>,
    },
    SetHighlights {
        old: HighlightsH,
        new: HighlightsH,
    },
    SetEncodings {
        old: Encodings,
        new: Encodings,
    },
    SetPositions {
        old: bool,
        new: bool,
    },
}

// The sections' element types.

/// An element of a section: plain data, stored little-endian.
trait Le: Pod {
    const NAME: &'static str;
    /// Swap the bytes: to and from little-endian on a big-endian host.
    fn swap(self) -> Self;
}

macro_rules! le {
    ($($t:ty, $name:literal, $swap:expr;)*) => {$(
        impl Le for $t {
            const NAME: &'static str = $name;
            fn swap(self) -> Self {
                let swap: fn($t) -> $t = $swap;
                swap(self)
            }
        }
    )*};
}

le! {
    u8, "u8", |v| v;
    u16, "u16", u16::swap_bytes;
    u32, "u32", u32::swap_bytes;
    u64, "u64", u64::swap_bytes;
    f32, "f32", |v| f32::from_bits(v.to_bits().swap_bytes());
}

// Writing.

#[derive(Default)]
struct Writer {
    sections: Vec<SectionH>,
    body: Vec<u8>,
}

fn pad(bytes: &mut Vec<u8>) {
    bytes.resize(bytes.len().next_multiple_of(ALIGN), 0);
}

impl Writer {
    fn put<T: Le>(&mut self, name: impl Into<String>, data: &[T]) {
        self.sections.push(SectionH {
            name: name.into(),
            ty: T::NAME.to_string(),
            offset: self.body.len() as u64,
            count: data.len() as u64,
        });
        if cfg!(target_endian = "little") {
            self.body.extend_from_slice(bytemuck::cast_slice(data));
        } else {
            for &v in data {
                self.body.extend_from_slice(bytemuck::bytes_of(&v.swap()));
            }
        }
        pad(&mut self.body);
    }

    fn strings(&mut self, name: &str, strings: &Strings) {
        let (offsets, bytes) = strings.parts();
        self.put(format!("{name}.offsets"), offsets);
        self.put(format!("{name}.bytes"), bytes);
    }

    /// Labels and flags are left out when every one is 0, which they are
    /// for most results.
    fn sparse<T: Le + PartialEq + Default>(&mut self, name: String, data: &[T]) {
        if data.iter().any(|v| *v != T::default()) {
            self.put(name, data);
        }
    }

    fn values(&mut self, name: String, values: &Values) {
        match values {
            Values::F32(v) => self.put(name, v),
            Values::U32(v) => self.put(name, v),
            Values::Dict { values, codes } => {
                self.strings(&format!("{name}.values"), values);
                self.put(name, codes);
            }
        }
    }

    fn columns(&mut self, prefix: &str, columns: &[Column]) -> Vec<ColumnH> {
        let headers = columns.iter().map(column_header).collect();
        for (i, column) in columns.iter().enumerate() {
            self.values(format!("{prefix}.columns.{i}"), &column.values);
        }
        headers
    }

    fn batch_columns(&mut self, prefix: &str, columns: &[BatchColumn]) -> Vec<BatchColumnH> {
        let mut headers = Vec::new();
        for (i, column) in columns.iter().enumerate() {
            let name = format!("{prefix}.columns.{i}");
            match &column.values {
                BatchValues::F32(v) => self.put(name, v),
                BatchValues::U32(v) => self.put(name, v),
                BatchValues::Dict { new_values, codes } => {
                    self.strings(&format!("{name}.values"), new_values);
                    self.put(name, codes);
                }
            }
            headers.push(BatchColumnH {
                name: column.name.clone(),
                ty: column.values.type_name().to_string(),
            });
        }
        headers
    }

    fn positions(&mut self, prefix: &str, positions: Option<&Positions>) -> bool {
        if let Some(p) = positions {
            self.put(format!("{prefix}.x"), &p.x);
            self.put(format!("{prefix}.y"), &p.y);
        }
        positions.is_some()
    }

    fn highlights(&mut self, prefix: &str, h: &Highlights) -> HighlightsH {
        let mut node_offsets = vec![0u32];
        let mut edge_offsets = vec![0u32];
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for path in &h.paths {
            nodes.extend_from_slice(&path.nodes);
            edges.extend_from_slice(&path.edges);
            node_offsets.push(u32::try_from(nodes.len()).expect("under 4G path nodes"));
            edge_offsets.push(edges.len() as u32);
        }
        self.put(format!("{prefix}.paths.offsets"), &node_offsets);
        self.put(format!("{prefix}.paths.nodes"), &nodes);
        self.put(format!("{prefix}.paths.edge_offsets"), &edge_offsets);
        self.put(format!("{prefix}.paths.edges"), &edges);
        self.put(format!("{prefix}.seeds"), &h.seeds);
        HighlightsH {
            paths: h.paths.len() as u64,
            seeds: h.seeds.len() as u64,
            distance: h.distance.clone(),
        }
    }

    fn node_batch(&mut self, prefix: &str, batch: &NodeBatch) -> NodeBatchH {
        self.strings(&format!("{prefix}.keys"), &batch.keys);
        self.sparse(format!("{prefix}.labels"), &batch.labels);
        NodeBatchH {
            count: batch.len() as u64,
            columns: self.batch_columns(prefix, &batch.columns),
            positions: self.positions(prefix, batch.positions.as_ref()),
        }
    }

    fn edge_batch(&mut self, prefix: &str, batch: &EdgeBatch) -> EdgeBatchH {
        self.put(format!("{prefix}.src"), &batch.src);
        self.put(format!("{prefix}.dst"), &batch.dst);
        self.sparse(format!("{prefix}.labels"), &batch.labels);
        if let Some(rows) = &batch.rows {
            self.put(format!("{prefix}.rows"), rows);
        }
        EdgeBatchH {
            count: batch.len() as u64,
            columns: self.batch_columns(prefix, &batch.columns),
            rows: batch.rows.is_some(),
        }
    }

    fn index(&mut self, prefix: &str, index: &[u32]) -> IndexH {
        self.put(format!("{prefix}.index"), index);
        IndexH {
            count: index.len() as u64,
        }
    }

    fn finish(self, header: impl FnOnce(u64, Vec<SectionH>) -> Header) -> Vec<u8> {
        let header = header(self.body.len() as u64, self.sections);
        let json = serde_json::to_vec(&header).expect("a header is plain JSON");
        let mut out = Vec::with_capacity(2 * ALIGN + json.len() + self.body.len());
        out.extend_from_slice(&MAGIC);
        let len = u32::try_from(json.len()).expect("a header under 4 GiB");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&json);
        pad(&mut out);
        out.extend_from_slice(&self.body);
        out
    }
}

fn column_header(column: &Column) -> ColumnH {
    ColumnH {
        name: column.name.clone(),
        ty: column.values.type_name().to_string(),
        role: column.role,
    }
}

fn write_result(r: &GraphResult) -> Vec<u8> {
    let mut w = Writer::default();
    w.strings("nodes.keys", &r.node_keys);
    w.sparse("nodes.labels".into(), &r.node_label);
    w.sparse("nodes.flags".into(), &r.node_flags);
    let nodes = NodesH {
        count: r.n_nodes() as u64,
        labels: r.node_labels.clone(),
        columns: w.columns("nodes", &r.node_columns),
        positions: w.positions("nodes", r.positions.as_ref()),
    };
    w.put("edges.src", &r.src);
    w.put("edges.dst", &r.dst);
    w.sparse("edges.labels".into(), &r.edge_label);
    if let Some(rows) = &r.edge_rows {
        w.put("edges.rows", rows);
    }
    w.sparse("edges.flags".into(), &r.edge_flags);
    let edges = EdgesH {
        count: r.n_edges() as u64,
        labels: r.edge_labels.clone(),
        columns: w.columns("edges", &r.edge_columns),
    };
    let highlights = w.highlights("highlights", &r.highlights);
    w.finish(|body, sections| {
        Header::Result(ResultH {
            graph: r.graph.clone(),
            nodes,
            edges,
            highlights,
            encodings: r.encodings.clone(),
            provenance: r.provenance.clone(),
            body,
            sections,
        })
    })
}

fn write_delta(delta: &GraphDelta) -> Vec<u8> {
    let mut w = Writer::default();
    let mut ops = Vec::with_capacity(delta.ops.len());
    for (i, op) in delta.ops.iter().enumerate() {
        let p = format!("ops.{i}");
        let label = |name: &String| LabelH { name: name.clone() };
        ops.push(match op {
            Op::AddNodeLabel(l) => OpH::AddNodeLabel(l.clone()),
            Op::PopNodeLabel(l) => OpH::PopNodeLabel(l.clone()),
            Op::AddEdgeLabel(l) => OpH::AddEdgeLabel(label(l)),
            Op::PopEdgeLabel(l) => OpH::PopEdgeLabel(label(l)),
            Op::AddNodes(b) => OpH::AddNodes(w.node_batch(&p, b)),
            Op::PopNodes(b) => OpH::PopNodes(w.node_batch(&p, b)),
            Op::AddEdges(b) => OpH::AddEdges(w.edge_batch(&p, b)),
            Op::PopEdges(b) => OpH::PopEdges(w.edge_batch(&p, b)),
            Op::RemoveNodes(v) => OpH::RemoveNodes(w.index(&p, v)),
            Op::RestoreNodes(v) => OpH::RestoreNodes(w.index(&p, v)),
            Op::RemoveEdges(v) => OpH::RemoveEdges(w.index(&p, v)),
            Op::RestoreEdges(v) => OpH::RestoreEdges(w.index(&p, v)),
            Op::SetColumn {
                target,
                index,
                old,
                new,
            } => {
                let mut side = |name: &str, column: &Option<Column>| {
                    column.as_ref().map(|c| {
                        w.values(format!("{p}.{name}"), &c.values);
                        column_header(c)
                    })
                };
                OpH::SetColumn {
                    target: *target,
                    index: *index,
                    old: side("old", old),
                    new: side("new", new),
                }
            }
            Op::SetHighlights { old, new } => OpH::SetHighlights {
                old: w.highlights(&format!("{p}.old"), old),
                new: w.highlights(&format!("{p}.new"), new),
            },
            Op::SetEncodings { old, new } => OpH::SetEncodings {
                old: old.clone(),
                new: new.clone(),
            },
            Op::SetPositions { old, new } => OpH::SetPositions {
                old: w.positions(&format!("{p}.old"), old.as_ref()),
                new: w.positions(&format!("{p}.new"), new.as_ref()),
            },
        });
    }
    w.finish(|body, sections| {
        Header::Delta(DeltaH {
            provenance: delta.provenance.clone(),
            ops,
            body,
            sections,
        })
    })
}

// Reading.

struct Reader<'a> {
    body: &'a [u8],
    sections: HashMap<String, SectionH>,
}

/// Split a buffer into its header and its body, checking the framing.
fn open(bytes: &[u8]) -> Result<(Header, Reader<'_>), UgrError> {
    if bytes.len() < 8 {
        return bad(format!(
            "the buffer is truncated: {} bytes, and the magic and the header's length take 8",
            bytes.len()
        ));
    }
    if bytes[..4] != MAGIC {
        return match &bytes[..4] {
            [b'U', b'G', b'R', version] => bad(format!(
                "version '{}', and this reader reads version 1",
                *version as char
            )),
            _ => bad("not a UGR buffer: it does not start with \"UGR1\""),
        };
    }
    let header_len = u32::from_le_bytes(bytes[4..8].try_into().expect("four bytes")) as usize;
    let Some(json) = bytes.get(8..8 + header_len) else {
        return bad(format!(
            "the buffer is truncated: its header is {header_len} bytes, and {} follow",
            bytes.len() - 8
        ));
    };
    let mut header: Header =
        serde_json::from_slice(json).map_err(|e| UgrError(format!("the header: {e}")))?;
    let (body_len, sections) = match &mut header {
        Header::Result(h) => (h.body, std::mem::take(&mut h.sections)),
        Header::Delta(h) => (h.body, std::mem::take(&mut h.sections)),
    };
    let start = (8 + header_len).next_multiple_of(ALIGN);
    let body = bytes.get(start..).unwrap_or(&[]);
    if (body.len() as u64) < body_len {
        return bad(format!(
            "the buffer is truncated: its body is {body_len} bytes, and {} follow the header",
            body.len()
        ));
    }
    if body.len() as u64 > body_len {
        return bad(format!(
            "{} bytes follow the body",
            body.len() as u64 - body_len
        ));
    }
    let mut by_name = HashMap::with_capacity(sections.len());
    for section in sections {
        if let Some(first) = by_name.insert(section.name.clone(), section) {
            return bad(format!("two sections are named '{}'", first.name));
        }
    }
    Ok((
        header,
        Reader {
            body,
            sections: by_name,
        },
    ))
}

impl Reader<'_> {
    /// A section's elements, or `None` if the buffer has no such section.
    fn opt<T: Le>(&self, name: &str) -> Result<Option<Vec<T>>, UgrError> {
        let Some(section) = self.sections.get(name) else {
            return Ok(None);
        };
        if section.ty != T::NAME {
            return bad(format!(
                "the section '{name}' is {}, not {}",
                section.ty,
                T::NAME
            ));
        }
        if section.offset % ALIGN as u64 != 0 {
            return bad(format!(
                "the section '{name}' starts at {}, not a multiple of {ALIGN}",
                section.offset
            ));
        }
        let end = section
            .count
            .checked_mul(size_of::<T>() as u64)
            .and_then(|len| section.offset.checked_add(len));
        let bytes = match end {
            Some(end) if end <= self.body.len() as u64 => {
                &self.body[section.offset as usize..end as usize]
            }
            _ => {
                return bad(format!(
                    "the section '{name}' ({} {} at {}) runs past the body's {} bytes",
                    section.count,
                    section.ty,
                    section.offset,
                    self.body.len()
                ))
            }
        };
        // A copy, since `bytes` may sit at any address; the cast is of the
        // new, aligned vector.
        let mut out = vec![T::zeroed(); section.count as usize];
        bytemuck::cast_slice_mut::<T, u8>(&mut out).copy_from_slice(bytes);
        if cfg!(target_endian = "big") {
            for v in &mut out {
                *v = v.swap();
            }
        }
        Ok(Some(out))
    }

    fn get<T: Le>(&self, name: &str) -> Result<Vec<T>, UgrError> {
        match self.opt(name)? {
            Some(data) => Ok(data),
            None => bad(format!("the section '{name}' is missing")),
        }
    }

    /// A section that must have `count` elements.
    fn exact<T: Le>(&self, name: &str, count: u64) -> Result<Vec<T>, UgrError> {
        let data = self.get::<T>(name)?;
        check_count(name, data.len(), count)?;
        Ok(data)
    }

    /// A section left out when every element is 0.
    fn sparse<T: Le>(&self, name: &str, count: usize) -> Result<Vec<T>, UgrError> {
        match self.opt::<T>(name)? {
            Some(data) => {
                check_count(name, data.len(), count as u64)?;
                Ok(data)
            }
            None => Ok(vec![T::zeroed(); count]),
        }
    }

    fn strings(&self, name: &str) -> Result<Strings, UgrError> {
        let offsets = self.get(&format!("{name}.offsets"))?;
        let bytes = self.get(&format!("{name}.bytes"))?;
        Strings::from_parts(offsets, bytes).map_err(|e| UgrError(format!("'{name}': {e}")))
    }

    fn values(&self, name: &str, ty: &str) -> Result<Values, UgrError> {
        match ty {
            "f32" => Ok(Values::F32(self.get(name)?)),
            "u32" => Ok(Values::U32(self.get(name)?)),
            "dict" => Ok(Values::Dict {
                values: self.strings(&format!("{name}.values"))?,
                codes: self.get(name)?,
            }),
            other => bad(format!("'{name}' has the unknown column type '{other}'")),
        }
    }

    fn column(&self, name: &str, h: ColumnH) -> Result<Column, UgrError> {
        Ok(Column {
            values: self.values(name, &h.ty)?,
            name: h.name,
            role: h.role,
        })
    }

    fn columns(&self, prefix: &str, headers: Vec<ColumnH>) -> Result<Vec<Column>, UgrError> {
        let column = |(i, h)| self.column(&format!("{prefix}.columns.{i}"), h);
        headers.into_iter().enumerate().map(column).collect()
    }

    fn batch_columns(
        &self,
        prefix: &str,
        headers: Vec<BatchColumnH>,
    ) -> Result<Vec<BatchColumn>, UgrError> {
        let column = |(i, h): (usize, BatchColumnH)| {
            let values = match self.values(&format!("{prefix}.columns.{i}"), &h.ty)? {
                Values::F32(v) => BatchValues::F32(v),
                Values::U32(v) => BatchValues::U32(v),
                Values::Dict { values, codes } => BatchValues::Dict {
                    new_values: values,
                    codes,
                },
            };
            Ok(BatchColumn {
                name: h.name,
                values,
            })
        };
        headers.into_iter().enumerate().map(column).collect()
    }

    fn positions(&self, prefix: &str, present: bool) -> Result<Option<Positions>, UgrError> {
        if !present {
            return Ok(None);
        }
        Ok(Some(Positions {
            x: self.get(&format!("{prefix}.x"))?,
            y: self.get(&format!("{prefix}.y"))?,
        }))
    }

    fn highlights(&self, prefix: &str, h: HighlightsH) -> Result<Highlights, UgrError> {
        let name = format!("{prefix}.paths");
        let node_offsets: Vec<u32> = self.get(&format!("{name}.offsets"))?;
        let edge_offsets: Vec<u32> = self.get(&format!("{name}.edge_offsets"))?;
        let nodes: Vec<u32> = self.get(&format!("{name}.nodes"))?;
        let edges: Vec<u32> = self.get(&format!("{name}.edges"))?;
        if h.paths.checked_add(1) != Some(node_offsets.len() as u64)
            || edge_offsets.len() != node_offsets.len()
        {
            return bad(format!(
                "'{name}' has {} and {} offsets for {} paths",
                node_offsets.len(),
                edge_offsets.len(),
                h.paths
            ));
        }
        let slice = |offsets: &[u32], data: &[u32], p: usize| {
            let range = offsets[p] as usize..offsets[p + 1] as usize;
            match data.get(range) {
                Some(part) => Ok(part.to_vec()),
                None => bad(format!(
                    "'{name}': the offsets of path {p} are out of range"
                )),
            }
        };
        let ends_ok = |offsets: &[u32], data: &[u32]| {
            offsets.first() == Some(&0) && offsets.last() == Some(&(data.len() as u32))
        };
        if !ends_ok(&node_offsets, &nodes) || !ends_ok(&edge_offsets, &edges) {
            return bad(format!(
                "'{name}': the offsets do not run from 0 to the length"
            ));
        }
        let mut paths = Vec::new();
        for p in 0..h.paths as usize {
            paths.push(Path {
                nodes: slice(&node_offsets, &nodes, p)?,
                edges: slice(&edge_offsets, &edges, p)?,
            });
        }
        Ok(Highlights {
            paths,
            seeds: self.exact(&format!("{prefix}.seeds"), h.seeds)?,
            distance: h.distance,
        })
    }

    fn node_batch(&self, prefix: &str, h: NodeBatchH) -> Result<NodeBatch, UgrError> {
        let keys = self.strings(&format!("{prefix}.keys"))?;
        check_count(&format!("{prefix}.keys"), keys.len(), h.count)?;
        Ok(NodeBatch {
            labels: self.sparse(&format!("{prefix}.labels"), keys.len())?,
            keys,
            columns: self.batch_columns(prefix, h.columns)?,
            positions: self.positions(prefix, h.positions)?,
        })
    }

    fn edge_batch(&self, prefix: &str, h: EdgeBatchH) -> Result<EdgeBatch, UgrError> {
        let src: Vec<u32> = self.exact(&format!("{prefix}.src"), h.count)?;
        let rows = match h.rows {
            true => Some(self.exact(&format!("{prefix}.rows"), h.count)?),
            false => None,
        };
        Ok(EdgeBatch {
            dst: self.exact(&format!("{prefix}.dst"), h.count)?,
            labels: self.sparse(&format!("{prefix}.labels"), src.len())?,
            src,
            rows,
            columns: self.batch_columns(prefix, h.columns)?,
        })
    }

    fn index(&self, prefix: &str, h: IndexH) -> Result<Vec<u32>, UgrError> {
        self.exact(&format!("{prefix}.index"), h.count)
    }
}

fn check_count(name: &str, have: usize, want: u64) -> Result<(), UgrError> {
    if have as u64 != want {
        return bad(format!(
            "'{name}' has {have} elements, and the header says {want}"
        ));
    }
    Ok(())
}

fn read_result(h: ResultH, r: &Reader<'_>) -> Result<GraphResult, UgrError> {
    let node_keys = r.strings("nodes.keys")?;
    check_count("nodes.keys", node_keys.len(), h.nodes.count)?;
    let n = node_keys.len();
    let src: Vec<u32> = r.exact("edges.src", h.edges.count)?;
    let m = src.len();
    let edge_rows: Option<Vec<u64>> = r.opt("edges.rows")?;
    if edge_rows
        .as_ref()
        .is_some_and(|rows| rows.iter().all(|&row| row == NO_ROW))
    {
        return bad("'edges.rows' is present, but no row is known");
    }
    let mut result = GraphResult {
        graph: h.graph,
        node_labels: h.nodes.labels,
        edge_labels: h.edges.labels,
        node_keys,
        node_label: r.sparse("nodes.labels", n)?,
        node_flags: r.sparse("nodes.flags", n)?,
        node_columns: r.columns("nodes", h.nodes.columns)?,
        positions: r.positions("nodes", h.nodes.positions)?,
        src,
        dst: r.exact("edges.dst", h.edges.count)?,
        edge_label: r.sparse("edges.labels", m)?,
        edge_rows,
        edge_flags: r.sparse("edges.flags", m)?,
        edge_columns: r.columns("edges", h.edges.columns)?,
        highlights: r.highlights("highlights", h.highlights)?,
        encodings: h.encodings,
        provenance: h.provenance,
        index: HashMap::new(),
    };
    result.validate()?;
    Ok(result)
}

fn read_delta(h: DeltaH, r: &Reader<'_>) -> Result<GraphDelta, UgrError> {
    let mut ops = Vec::with_capacity(h.ops.len());
    for (i, op) in h.ops.into_iter().enumerate() {
        let p = format!("ops.{i}");
        ops.push(match op {
            OpH::AddNodeLabel(l) => Op::AddNodeLabel(l),
            OpH::PopNodeLabel(l) => Op::PopNodeLabel(l),
            OpH::AddEdgeLabel(l) => Op::AddEdgeLabel(l.name),
            OpH::PopEdgeLabel(l) => Op::PopEdgeLabel(l.name),
            OpH::AddNodes(b) => Op::AddNodes(r.node_batch(&p, b)?),
            OpH::PopNodes(b) => Op::PopNodes(r.node_batch(&p, b)?),
            OpH::AddEdges(b) => Op::AddEdges(r.edge_batch(&p, b)?),
            OpH::PopEdges(b) => Op::PopEdges(r.edge_batch(&p, b)?),
            OpH::RemoveNodes(v) => Op::RemoveNodes(r.index(&p, v)?),
            OpH::RestoreNodes(v) => Op::RestoreNodes(r.index(&p, v)?),
            OpH::RemoveEdges(v) => Op::RemoveEdges(r.index(&p, v)?),
            OpH::RestoreEdges(v) => Op::RestoreEdges(r.index(&p, v)?),
            OpH::SetColumn {
                target,
                index,
                old,
                new,
            } => {
                let side = |name: &str, column: Option<ColumnH>| {
                    column
                        .map(|c| r.column(&format!("{p}.{name}"), c))
                        .transpose()
                };
                Op::SetColumn {
                    target,
                    index,
                    old: side("old", old)?,
                    new: side("new", new)?,
                }
            }
            OpH::SetHighlights { old, new } => Op::SetHighlights {
                old: r.highlights(&format!("{p}.old"), old)?,
                new: r.highlights(&format!("{p}.new"), new)?,
            },
            OpH::SetEncodings { old, new } => Op::SetEncodings { old, new },
            OpH::SetPositions { old, new } => Op::SetPositions {
                old: r.positions(&format!("{p}.old"), old)?,
                new: r.positions(&format!("{p}.new"), new)?,
            },
        });
    }
    Ok(GraphDelta {
        ops,
        provenance: h.provenance,
    })
}
