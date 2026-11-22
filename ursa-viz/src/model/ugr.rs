//! UGR, the wire format of a [`GraphResult`] and a [`GraphDelta`]: a JSON
//! header, then aligned little-endian arrays. `ursa-viz/UGR.md` is the
//! byte-level specification; `ursa-viz/ui/ugr.js` reads it in JavaScript.
//!
//! Encoding is canonical: equal values give equal bytes, so decoding what
//! the encoder wrote and encoding it again gives the same buffer.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

use bytemuck::Pod;
use serde::{Deserialize, Serialize};
use ursa_core::topology::UndirectedCsr;

use crate::pace::{ready, Pace};

use super::delta::{BatchColumn, BatchValues, EdgeBatch, GraphDelta, NodeBatch, Op};
use super::result::{
    Column, Encodings, GraphResult, Highlights, ModelError, NodeLabel, Path, Positions, Provenance,
    Role, Sparse, Strings, Target, Values, NO_ROW,
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
    Result(Box<GraphResult>),
    Delta(GraphDelta),
}

/// Decode a buffer that may be a result or a delta.
pub fn decode(bytes: &[u8]) -> Result<Message, UgrError> {
    let (header, reader) = open(&bytes)?;
    match header {
        Header::Result(h) => {
            let result = ready(read_result(
                *h,
                &reader,
                Room::default(),
                &mut Pace::never(),
            ));
            Ok(Message::Result(Box::new(result?)))
        }
        Header::Delta(h) => Ok(Message::Delta(read_delta(h, &reader)?)),
    }
}

/// Where a UGR buffer is read from: bytes this program holds, or bytes it
/// can copy from (a page's `ArrayBuffer`, outside a WebAssembly memory).
/// The decoder copies each section once, from the source to where the
/// result keeps it, so a buffer held elsewhere is never held here as well.
pub trait Source {
    /// How many bytes there are.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy the bytes from `at` on into `out`, all of it: the caller has
    /// checked that `at + out.len()` is within [`Source::len`].
    fn read(&self, at: usize, out: &mut [u8]);
}

impl Source for &[u8] {
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }

    fn read(&self, at: usize, out: &mut [u8]) {
        out.copy_from_slice(&self[at..at + out.len()]);
    }
}

/// Room a decoded result is given beyond what it holds: for as many more
/// nodes and edges, so that the deltas a view applies to it first move
/// nothing ([`GraphResult::reserve`], without allocating twice). A count
/// from a header is not trusted with it: room is given only once a section
/// of that many elements is there to read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Room {
    pub nodes: usize,
    pub edges: usize,
}

impl GraphResult {
    /// The result as one UGR buffer.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.encoded().to_vec()
    }

    /// The result in its wire format, for a caller that puts the bytes
    /// somewhere other than a vector ([`Encoded::write`]).
    pub fn encoded(&self) -> Encoded<'_> {
        write_result(self, None)
    }

    /// [`GraphResult::to_bytes`], with the undirected adjacency of the
    /// result's edges in the buffer beside them (UGR's
    /// `edges.adjacency.*`), for a writer that holds it already: a layout
    /// then uses it as it is ([`read_adjacency`]) and sorts nothing.
    ///
    /// `adjacency` must be the adjacency of every edge slot of this result
    /// ([`UndirectedCsr::from_edges`] of its nodes, `src` and `dst`), and a
    /// result with a removed node or edge has none to give, since a layout
    /// reads only what is shown. Neither is checked here.
    pub fn to_bytes_with_adjacency(&self, adjacency: &UndirectedCsr) -> Vec<u8> {
        write_result(self, Some(adjacency)).to_vec()
    }

    /// Decode a result, checking every invariant of the model.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, UgrError> {
        match decode(bytes)? {
            Message::Result(result) => Ok(*result),
            Message::Delta(_) => bad("the buffer holds a delta, not a result"),
        }
    }

    /// [`GraphResult::from_bytes`] for a view of a large result on a page:
    /// read from any [`Source`], a section at a time; with `room` for the
    /// first deltas; and giving the event loop a turn as `pace` says
    /// between the sections and the checks, so that no piece holds the
    /// thread for long. The result is the same.
    ///
    /// `room(nodes, edges)` is asked once the header has said how many
    /// there are.
    pub async fn read_paced(
        source: &dyn Source,
        room: &dyn Fn(usize, usize) -> Room,
        pace: &mut Pace,
    ) -> Result<Self, UgrError> {
        let (header, reader) = open(source)?;
        match header {
            Header::Result(h) => {
                let room = room(h.nodes.count as usize, h.edges.count as usize);
                read_result(*h, &reader, room, pace).await
            }
            Header::Delta(_) => bad("the buffer holds a delta, not a result"),
        }
    }
}

/// The undirected adjacency a result's buffer carries beside its edges
/// ([`GraphResult::to_bytes_with_adjacency`]), checked
/// ([`UndirectedCsr::from_parts`]); `None` if it carries none, as most do.
pub fn read_adjacency(bytes: &[u8]) -> Result<Option<UndirectedCsr>, UgrError> {
    let (header, reader) = open(&bytes)?;
    let Header::Result(h) = header else {
        return bad("the buffer holds a delta, not a result");
    };
    let (Some(offsets), Some(targets)) = (
        reader.opt::<u64>(ADJACENCY_OFFSETS)?,
        reader.opt::<u32>(ADJACENCY_TARGETS)?,
    ) else {
        return Ok(None);
    };
    UndirectedCsr::from_parts(h.nodes.count as usize, offsets, targets)
        .map(Some)
        .map_err(UgrError)
}

/// The sections of a result's undirected adjacency: `nodes + 1` offsets,
/// and each node's neighbours.
const ADJACENCY_OFFSETS: &str = "edges.adjacency.offsets";
const ADJACENCY_TARGETS: &str = "edges.adjacency.targets";

impl GraphDelta {
    /// The delta as one UGR buffer.
    pub fn to_bytes(&self) -> Vec<u8> {
        write_delta(self).to_vec()
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
    Result(Box<ResultH>),
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

/// The sections of a buffer being written: each borrowed from the value it
/// is a part of, so that nothing is copied until the buffer is
/// ([`Encoded`]).
#[derive(Default)]
struct Writer<'a> {
    sections: Vec<SectionH>,
    /// Each section's bytes, in order.
    parts: Vec<Cow<'a, [u8]>>,
    /// The body's length so far, padding included.
    body: usize,
}

/// A value in its wire format, not yet in one piece: the header, and the
/// sections where the value holds them. [`Encoded::to_vec`] is the buffer;
/// [`Encoded::write`] puts it somewhere else (a page's array) a section at
/// a time, so that a large result is never held a second time on the way.
pub struct Encoded<'a> {
    /// The magic, the header's length and the header, padded.
    head: Vec<u8>,
    /// Where each section starts in the body, and its bytes.
    parts: Vec<(usize, Cow<'a, [u8]>)>,
    body: usize,
}

impl Encoded<'_> {
    /// The buffer's length in bytes.
    pub fn len(&self) -> usize {
        self.head.len() + self.body
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Give `put` each run of the buffer's bytes and where it starts. The
    /// bytes between runs (padding to a multiple of 8) are zero, and are
    /// not given: the destination starts as `len()` zeros.
    pub fn write(&self, mut put: impl FnMut(usize, &[u8])) {
        put(0, &self.head);
        for (offset, bytes) in &self.parts {
            if !bytes.is_empty() {
                put(self.head.len() + offset, bytes);
            }
        }
    }

    /// The buffer.
    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.len()];
        self.write(|at, bytes| out[at..at + bytes.len()].copy_from_slice(bytes));
        out
    }
}

fn pad(bytes: &mut Vec<u8>) {
    bytes.resize(bytes.len().next_multiple_of(ALIGN), 0);
}

impl<'a> Writer<'a> {
    fn put<T: Le>(&mut self, name: impl Into<String>, data: &'a [T]) {
        let bytes = if cfg!(target_endian = "little") {
            Cow::Borrowed(bytemuck::cast_slice(data))
        } else {
            let swapped: Vec<T> = data.iter().map(|v| v.swap()).collect();
            Cow::Owned(bytemuck::cast_slice(&swapped).to_vec())
        };
        self.part(name.into(), T::NAME, data.len(), bytes);
    }

    /// [`Writer::put`] for a section made for the buffer, which no value
    /// holds: small ones (a result's path offsets).
    fn put_owned<T: Le>(&mut self, name: impl Into<String>, data: Vec<T>) {
        let swapped: Vec<T> = if cfg!(target_endian = "little") {
            data
        } else {
            data.iter().map(|v| v.swap()).collect()
        };
        let bytes = Cow::Owned(bytemuck::cast_slice(&swapped).to_vec());
        self.part(name.into(), T::NAME, swapped.len(), bytes);
    }

    fn part(&mut self, name: String, ty: &str, count: usize, bytes: Cow<'a, [u8]>) {
        self.sections.push(SectionH {
            name,
            ty: ty.to_string(),
            offset: self.body as u64,
            count: count as u64,
        });
        self.body = (self.body + bytes.len()).next_multiple_of(ALIGN);
        self.parts.push(bytes);
    }

    fn strings(&mut self, name: &str, strings: &'a Strings) {
        let (offsets, bytes) = strings.parts();
        self.put(format!("{name}.offsets"), offsets);
        self.put(format!("{name}.bytes"), bytes);
    }

    /// Labels and flags are left out when every one is 0, which they are
    /// for most results.
    fn sparse<T: Le + PartialEq + Default>(&mut self, name: String, data: &'a [T]) {
        if data.iter().any(|v| *v != T::default()) {
            self.put(name, data);
        }
    }

    fn values(&mut self, name: String, values: &'a Values) {
        match values {
            Values::F32(v) => self.put(name, v),
            Values::U32(v) => self.put(name, v),
            Values::Dict { values, codes } => {
                self.strings(&format!("{name}.values"), values);
                self.put(name, codes);
            }
        }
    }

    /// The old or the new column of a `SetColumn`, if there is one.
    fn side(&mut self, name: String, column: &'a Option<Column>) -> Option<ColumnH> {
        column.as_ref().map(|c| {
            self.values(name, &c.values);
            column_header(c)
        })
    }

    fn columns(&mut self, prefix: &str, columns: &'a [Column]) -> Vec<ColumnH> {
        let headers = columns.iter().map(column_header).collect();
        for (i, column) in columns.iter().enumerate() {
            self.values(format!("{prefix}.columns.{i}"), &column.values);
        }
        headers
    }

    fn batch_columns(&mut self, prefix: &str, columns: &'a [BatchColumn]) -> Vec<BatchColumnH> {
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

    fn positions(&mut self, prefix: &str, positions: Option<&'a Positions>) -> bool {
        if let Some(p) = positions {
            self.put(format!("{prefix}.x"), &p.x);
            self.put(format!("{prefix}.y"), &p.y);
        }
        positions.is_some()
    }

    fn highlights(&mut self, prefix: &str, h: &'a Highlights) -> HighlightsH {
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
        self.put_owned(format!("{prefix}.paths.offsets"), node_offsets);
        self.put_owned(format!("{prefix}.paths.nodes"), nodes);
        self.put_owned(format!("{prefix}.paths.edge_offsets"), edge_offsets);
        self.put_owned(format!("{prefix}.paths.edges"), edges);
        self.put(format!("{prefix}.seeds"), &h.seeds);
        HighlightsH {
            paths: h.paths.len() as u64,
            seeds: h.seeds.len() as u64,
            distance: h.distance.clone(),
        }
    }

    fn node_batch(&mut self, prefix: &str, batch: &'a NodeBatch) -> NodeBatchH {
        self.strings(&format!("{prefix}.keys"), &batch.keys);
        self.sparse(format!("{prefix}.labels"), &batch.labels);
        NodeBatchH {
            count: batch.len() as u64,
            columns: self.batch_columns(prefix, &batch.columns),
            positions: self.positions(prefix, batch.positions.as_ref()),
        }
    }

    fn edge_batch(&mut self, prefix: &str, batch: &'a EdgeBatch) -> EdgeBatchH {
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

    fn index(&mut self, prefix: &str, index: &'a [u32]) -> IndexH {
        self.put(format!("{prefix}.index"), index);
        IndexH {
            count: index.len() as u64,
        }
    }

    fn finish(self, header: impl FnOnce(u64, Vec<SectionH>) -> Header) -> Encoded<'a> {
        let offsets: Vec<usize> = self.sections.iter().map(|s| s.offset as usize).collect();
        let header = header(self.body as u64, self.sections);
        let json = serde_json::to_vec(&header).expect("a header is plain JSON");
        let mut head = Vec::with_capacity(2 * ALIGN + json.len());
        head.extend_from_slice(&MAGIC);
        let len = u32::try_from(json.len()).expect("a header under 4 GiB");
        head.extend_from_slice(&len.to_le_bytes());
        head.extend_from_slice(&json);
        pad(&mut head);
        Encoded {
            head,
            parts: offsets.into_iter().zip(self.parts).collect(),
            body: self.body,
        }
    }
}

fn column_header(column: &Column) -> ColumnH {
    ColumnH {
        name: column.name.clone(),
        ty: column.values.type_name().to_string(),
        role: column.role,
    }
}

fn write_result<'a>(r: &'a GraphResult, adjacency: Option<&'a UndirectedCsr>) -> Encoded<'a> {
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
    if let Some(labels) = r.edge_label.as_slice() {
        w.sparse("edges.labels".into(), labels);
    }
    if let Some(rows) = &r.edge_rows {
        w.put("edges.rows", rows);
    }
    if let Some(flags) = r.edge_flags.as_slice() {
        w.sparse("edges.flags".into(), flags);
    }
    if let Some(adjacency) = adjacency {
        w.put(ADJACENCY_OFFSETS, &adjacency.offsets);
        w.put(ADJACENCY_TARGETS, &adjacency.targets);
    }
    let edges = EdgesH {
        count: r.n_edges() as u64,
        labels: r.edge_labels.clone(),
        columns: w.columns("edges", &r.edge_columns),
    };
    let highlights = w.highlights("highlights", &r.highlights);
    w.finish(|body, sections| {
        Header::Result(Box::new(ResultH {
            graph: r.graph.clone(),
            nodes,
            edges,
            highlights,
            encodings: r.encodings.clone(),
            provenance: r.provenance.clone(),
            body,
            sections,
        }))
    })
}

fn write_delta(delta: &GraphDelta) -> Encoded<'_> {
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
            } => OpH::SetColumn {
                target: *target,
                index: *index,
                old: w.side(format!("{p}.old"), old),
                new: w.side(format!("{p}.new"), new),
            },
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
    source: &'a dyn Source,
    /// Where the body starts in `source`, and how long it is.
    start: usize,
    body: usize,
    sections: HashMap<String, SectionH>,
}

/// Split a buffer into its header and its body, checking the framing.
fn open(source: &dyn Source) -> Result<(Header, Reader<'_>), UgrError> {
    let len = source.len();
    if len < 8 {
        return bad(format!(
            "the buffer is truncated: {len} bytes, and the magic and the header's length take 8"
        ));
    }
    let mut frame = [0u8; 8];
    source.read(0, &mut frame);
    if frame[..4] != MAGIC {
        return match &frame[..4] {
            [b'U', b'G', b'R', version] => bad(format!(
                "version '{}', and this reader reads version 1",
                *version as char
            )),
            _ => bad("not a UGR buffer: it does not start with \"UGR1\""),
        };
    }
    let header_len = u32::from_le_bytes(frame[4..8].try_into().expect("four bytes")) as usize;
    if header_len > len - 8 {
        return bad(format!(
            "the buffer is truncated: its header is {header_len} bytes, and {} follow",
            len - 8
        ));
    }
    let mut json = vec![0u8; header_len];
    source.read(8, &mut json);
    let mut header: Header =
        serde_json::from_slice(&json).map_err(|e| UgrError(format!("the header: {e}")))?;
    drop(json);
    let (body_len, sections) = match &mut header {
        Header::Result(h) => (h.body, std::mem::take(&mut h.sections)),
        Header::Delta(h) => (h.body, std::mem::take(&mut h.sections)),
    };
    let start = (8 + header_len).next_multiple_of(ALIGN).min(len);
    let body = len - start;
    if (body as u64) < body_len {
        return bad(format!(
            "the buffer is truncated: its body is {body_len} bytes, and {body} follow the header"
        ));
    }
    if body as u64 > body_len {
        return bad(format!("{} bytes follow the body", body as u64 - body_len));
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
            source,
            start,
            body,
            sections: by_name,
        },
    ))
}

impl Reader<'_> {
    /// A section's elements, or `None` if the buffer has no such section.
    fn opt<T: Le>(&self, name: &str) -> Result<Option<Vec<T>>, UgrError> {
        self.opt_with(name, 0)
    }

    /// [`Reader::opt`], in a vector with room for `extra` more elements.
    fn opt_with<T: Le>(&self, name: &str, extra: usize) -> Result<Option<Vec<T>>, UgrError> {
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
        if !end.is_some_and(|end| end <= self.body as u64) {
            return bad(format!(
                "the section '{name}' ({} {} at {}) runs past the body's {} bytes",
                section.count, section.ty, section.offset, self.body
            ));
        }
        // The one copy of the section: from the source, which may sit at
        // any address or in no memory of ours, into the aligned vector the
        // result keeps.
        let count = section.count as usize;
        // Never more room than the section is long: `extra` may come from a
        // count the header only claims.
        let mut out = Vec::with_capacity(count + extra.min(count));
        out.resize(count, T::zeroed());
        self.source.read(
            self.start + section.offset as usize,
            bytemuck::cast_slice_mut::<T, u8>(&mut out),
        );
        if cfg!(target_endian = "big") {
            for v in &mut out {
                *v = v.swap();
            }
        }
        Ok(Some(out))
    }

    fn get<T: Le>(&self, name: &str) -> Result<Vec<T>, UgrError> {
        self.get_with(name, 0)
    }

    fn get_with<T: Le>(&self, name: &str, extra: usize) -> Result<Vec<T>, UgrError> {
        match self.opt_with(name, extra)? {
            Some(data) => Ok(data),
            None => bad(format!("the section '{name}' is missing")),
        }
    }

    /// A section that must have `count` elements.
    fn exact<T: Le>(&self, name: &str, count: u64) -> Result<Vec<T>, UgrError> {
        self.exact_with(name, count, 0)
    }

    fn exact_with<T: Le>(&self, name: &str, count: u64, extra: usize) -> Result<Vec<T>, UgrError> {
        let data = self.get_with::<T>(name, extra)?;
        check_count(name, data.len(), count)?;
        Ok(data)
    }

    /// A section left out when every element is 0.
    fn sparse<T: Le>(&self, name: &str, count: usize) -> Result<Vec<T>, UgrError> {
        self.sparse_with(name, count, 0)
    }

    fn sparse_with<T: Le>(
        &self,
        name: &str,
        count: usize,
        extra: usize,
    ) -> Result<Vec<T>, UgrError> {
        match self.opt_with::<T>(name, extra)? {
            Some(data) => {
                check_count(name, data.len(), count as u64)?;
                Ok(data)
            }
            None => {
                let mut zeros = Vec::with_capacity(count + extra.min(count));
                zeros.resize(count, T::zeroed());
                Ok(zeros)
            }
        }
    }

    /// [`Reader::sparse`], holding nothing for a section left out.
    fn lazy<T: Le + Default + PartialEq>(
        &self,
        name: &str,
        count: usize,
    ) -> Result<Sparse<T>, UgrError> {
        match self.opt::<T>(name)? {
            Some(data) => {
                check_count(name, data.len(), count as u64)?;
                Ok(Sparse::from_vec(data))
            }
            None => Ok(Sparse::zeros(count)),
        }
    }

    fn strings(&self, name: &str) -> Result<Strings, UgrError> {
        self.strings_with(name, 0)
    }

    /// [`Reader::strings`], with room for `extra` more of them.
    fn strings_with(&self, name: &str, extra: usize) -> Result<Strings, UgrError> {
        let offsets = self.get_with(&format!("{name}.offsets"), extra)?;
        let bytes = self.get_with(&format!("{name}.bytes"), 8 * extra)?;
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

/// A result from its header and its sections, each copied once into a
/// vector with `room`, with a turn of the event loop as `pace` says after
/// each of the large ones, and then while the result is checked.
async fn read_result(
    h: ResultH,
    r: &Reader<'_>,
    room: Room,
    pace: &mut Pace,
) -> Result<GraphResult, UgrError> {
    let node_keys = r.strings_with("nodes.keys", room.nodes)?;
    check_count("nodes.keys", node_keys.len(), h.nodes.count)?;
    let n = node_keys.len();
    let node_label = r.sparse_with("nodes.labels", n, room.nodes)?;
    let node_flags = r.sparse_with("nodes.flags", n, room.nodes)?;
    pace.breathe().await;
    let node_columns = r.columns("nodes", h.nodes.columns)?;
    let positions = r.positions("nodes", h.nodes.positions)?;
    pace.breathe().await;
    let src: Vec<u32> = r.exact_with("edges.src", h.edges.count, room.edges)?;
    let m = src.len();
    pace.breathe().await;
    let dst: Vec<u32> = r.exact_with("edges.dst", h.edges.count, room.edges)?;
    pace.breathe().await;
    let edge_rows: Option<Vec<u64>> = r.opt_with("edges.rows", room.edges)?;
    if edge_rows
        .as_ref()
        .is_some_and(|rows| rows.iter().all(|&row| row == NO_ROW))
    {
        return bad("'edges.rows' is present, but no row is known");
    }
    pace.breathe().await;
    let edge_label = r.lazy("edges.labels", m)?;
    let edge_flags = r.lazy("edges.flags", m)?;
    pace.breathe().await;
    let edge_columns = r.columns("edges", h.edges.columns)?;
    pace.breathe().await;
    let mut result = GraphResult {
        graph: h.graph,
        node_labels: h.nodes.labels,
        edge_labels: h.edges.labels,
        node_keys,
        node_label,
        node_flags,
        node_columns,
        positions,
        src,
        dst,
        edge_label,
        edge_rows,
        edge_flags,
        edge_columns,
        highlights: r.highlights("highlights", h.highlights)?,
        encodings: h.encodings,
        provenance: h.provenance,
        index: Default::default(),
        named: Vec::new(),
    };
    result.validate_paced(pace).await?;
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
