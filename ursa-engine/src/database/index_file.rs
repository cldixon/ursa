//! One stored graph index: `club.ursa.d/indexes/<hash>.idx`.
//!
//! A file holds one built index, the CSR topology and the id map, under the
//! full key it was built for (the edge plan's fingerprint and the identity of
//! every source file), followed by optional **chunks**: parts of the index
//! that are built later, or that only some queries need, such as the
//! in-adjacency. Version 2, all integers little-endian.
//!
//! The **main part** comes first:
//!
//! ```text
//! offset  size  field
//!      0     8  magic            "URSAIDX\0"
//!      8     4  version          2
//!     12     4  flags            bit 0: string ids
//!     16     8  n_nodes
//!     24     8  n_edges
//!     32     8  key_len          bytes of the key that follows the header
//!     40     4  n_sections
//!     44     4  reserved (zero)
//!     48     8  main_len         bytes of the main part, trailer included
//!     56     8  n_dropped        edge rows left out (see below); zero before
//!                                #311, and for a graph with no vertex table
//!     64     …  key              JSON: {"fingerprint": …, "files": [{…}, …]}
//!      …     …  section table    at the next multiple of 8; 24 bytes a section:
//!                                kind u32, element width u32, offset u64, count u64
//!      …     …  sections         each at a multiple of 64 from the file's start
//!      …    16  trailer          at the next multiple of 8: main_len u64,
//!                                CRC-32 of every byte before the trailer u32,
//!                                "XDIU"
//! ```
//!
//! Its sections, in this order: the out-adjacency's offsets (`u64`,
//! `n_nodes + 1`), targets (`u32`, `n_edges`) and edge ids (`u32`, `n_edges`);
//! then the ids, either `i64` values (`n_nodes`) or, for string ids,
//! Arrow-style `i32` offsets (`n_nodes + 1`, from 0) and UTF-8 bytes.
//!
//! A graph with a vertex table leaves out the edge rows whose source or
//! destination is not one of its keys (#311). When it left some out, the
//! header counts them (`n_dropped`) and one more section (kind 11, bytes)
//! says which rows of the edge plan were kept: one bit per row,
//! `n_edges + n_dropped` of them, least significant bit first, as an Arrow
//! boolean buffer. Its key includes the vertex table's, so no file written
//! before #311 is ever read for such a graph; a file of a graph without one
//! is read as before, with nothing left out.
//!
//! **Chunks** follow the main part, one after another, each appended in
//! place once what it holds exists (see `IndexStore`):
//!
//! ```text
//!      0    16  header           "CHNK", kind u32, end u64 (the chunk's end
//!                                offset in the file)
//!     16     …  sections         each at a multiple of 64 from the file's start
//!      …     …  name             bytes naming what the chunk holds (may be empty)
//!      …     …  section table    at the next multiple of 8, as in the main part
//!      …    48  trailer          kind u32, n_sections u32, start u64,
//!                                name offset u64, name length u64,
//!                                data CRC u32 (the header and sections),
//!                                meta CRC u32 (name, table and the 36 bytes
//!                                of the trailer before it), "KNHC", zero u32
//! ```
//!
//! Chunk kind 1 is the in-adjacency (no name; its offsets, targets and edge
//! ids, as the out-adjacency's). Chunk kind 2 is a set of evaluated weights:
//! its name is the weights' key (the weighted edge plan's fingerprint and the
//! identity of the files it reads), and it holds one `f64` section (kind 10,
//! `n_edges` values in edge-row order) per weight expression, in order. A reader of the main part never reads the
//! chunks' arrays: it lists the chunks (their headers, names and tables, a
//! few small reads), and reads a chunk's sections only when they are asked
//! for, checking the main part's trailer and the chunk's data CRC first. The
//! in-adjacency is read this way the first time an algorithm asks for it
//! (`Topology::with_incoming_loader`).
//!
//! **Appending.** A chunk is written after the last chunk that checks out,
//! so a chunk cut short by a crash is overwritten by the next append, and a
//! reader stops listing at the first chunk that does not check out.
//! Appending never changes the bytes of the main part or of an earlier
//! chunk, so a reader in another process (`--readonly`) never sees them
//! change.
//!
//! **Why raw arrays, not Arrow IPC.** The arrays have different lengths and
//! no schema: an IPC stream would need one record batch per array, or padding
//! them into one table, and its metadata says nothing a section table does
//! not. Raw little-endian arrays at 64-byte-aligned offsets are what a later
//! memory-mapped reader wants (each section is a ready `&[u32]`/`&[u64]` on a
//! little-endian machine), need no dependency, and the string ids are already
//! in Arrow's layout, so they become a `StringArray` without a copy once
//! mapped. The main part is read once, front to back, decoding each section
//! into its array as it goes ([`read_file`]).
//!
//! **Corruption.** The main part is read only if its length matches the
//! trailer, the CRC matches, the version is 2, the key matches the key asked
//! for, every section lies within it, and the arrays form a valid topology and
//! id map (`ursa_core::Topology::from_parts`, `IdMap::from_user_ids`).
//! Anything else is an error, which the store treats as a miss. A chunk that
//! does not check out is ignored, and what it held is rebuilt when needed.
//!
//! **Version 1** files (the in-adjacency, if any, inside the main part) are
//! not read: they are a miss like any other unreadable file, so the index is
//! built again once and stored as version 2 in their place.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use datafusion::arrow::array::{Array, Int64Array, StringArray};
use datafusion::arrow::buffer::{BooleanBuffer, Buffer, OffsetBuffer, ScalarBuffer};
use serde_json::{json, Value};
use ursa_core::{Adjacency, IdMap, Topology};

use crate::graph::FileIdentity;

/// The first bytes of every index file.
pub const MAGIC: [u8; 8] = *b"URSAIDX\0";
/// The last bytes of every index file.
const END_MAGIC: [u8; 4] = *b"XDIU";
/// The index file version this build reads and writes.
pub const VERSION: u32 = 2;
/// The first bytes of every chunk.
const CHUNK_MAGIC: [u8; 4] = *b"CHNK";
/// The last bytes but four of every chunk's trailer.
const CHUNK_END_MAGIC: [u8; 4] = *b"KNHC";
const CHUNK_HEADER_LEN: u64 = 16;
const CHUNK_TRAILER_LEN: u64 = 48;

/// The chunk kind of an in-adjacency.
pub const CHUNK_INCOMING: u32 = 1;
/// The chunk kind of a set of evaluated weights.
pub const CHUNK_WEIGHTS: u32 = 2;

const HEADER_LEN: u64 = 64;
const TRAILER_LEN: u64 = 16;
const SECTION_ENTRY_LEN: u64 = 24;
const SECTION_ALIGN: u64 = 64;

const FLAG_STRING_IDS: u32 = 1;

const OUT_OFFSETS: u32 = 1;
const OUT_TARGETS: u32 = 2;
const OUT_EDGE_IDS: u32 = 3;
const IN_OFFSETS: u32 = 4;
const IN_TARGETS: u32 = 5;
const IN_EDGE_IDS: u32 = 6;
const IDS_INT64: u32 = 7;
const IDS_STR_OFFSETS: u32 = 8;
const IDS_STR_BYTES: u32 = 9;
const WEIGHTS_F64: u32 = 10;
const KEPT_ROWS: u32 = 11;

/// What an index is stored under: the edge plan's fingerprint and the
/// identity of every file it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexKey {
    pub fingerprint: String,
    pub files: Vec<FileIdentity>,
}

impl IndexKey {
    pub fn new(fingerprint: String, files: Vec<FileIdentity>) -> Self {
        Self { fingerprint, files }
    }

    /// The key as JSON, as the file header and the manifest hold it.
    pub fn to_json(&self) -> Value {
        let files: Vec<Value> = self
            .files
            .iter()
            .map(|f| {
                json!({
                    "store": f.store,
                    "location": f.location,
                    "size": f.size,
                    "modified": f.modified,
                    "e_tag": f.e_tag,
                })
            })
            .collect();
        json!({ "fingerprint": self.fingerprint, "files": files })
    }

    /// The key from its JSON, or `None` if it is not one.
    pub fn from_json(value: &Value) -> Option<Self> {
        let fingerprint = value.get("fingerprint")?.as_str()?.to_string();
        let files = value
            .get("files")?
            .as_array()?
            .iter()
            .map(|f| {
                Some(FileIdentity {
                    store: f.get("store")?.as_str()?.to_string(),
                    location: f.get("location")?.as_str()?.to_string(),
                    size: f.get("size")?.as_u64()?,
                    modified: f.get("modified")?.as_i64()?,
                    e_tag: match f.get("e_tag")? {
                        Value::Null => None,
                        Value::String(s) => Some(s.clone()),
                        _ => return None,
                    },
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self { fingerprint, files })
    }

    /// A stable 64-bit hash of the key (FNV-1a over its JSON), which names
    /// its file. The file's header holds the full key, so a collision is
    /// only a miss.
    pub fn hash(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.to_json().to_string().bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }
}

/// One array of the file: borrowed from the topology being written, or a
/// part of the id column (which the id map hands out as an `Arc`).
enum Data<'a> {
    U64(&'a [u64]),
    F64(&'a [f64]),
    U32(&'a [u32]),
    /// The values of an `Int64Array`.
    Ints(Int64Array),
    /// The offsets of a `StringArray`, written less their first value so
    /// they start at 0.
    StrOffsets(StringArray),
    /// The bytes of a `StringArray` its offsets span.
    StrBytes(StringArray),
    /// Bytes, as they are.
    Bytes(Buffer),
}

impl Data<'_> {
    fn width(&self) -> u64 {
        match self {
            Data::U64(_) | Data::F64(_) | Data::Ints(_) => 8,
            Data::U32(_) | Data::StrOffsets(_) => 4,
            Data::StrBytes(_) | Data::Bytes(_) => 1,
        }
    }

    fn count(&self) -> u64 {
        (match self {
            Data::U64(v) => v.len(),
            Data::F64(v) => v.len(),
            Data::U32(v) => v.len(),
            Data::Ints(a) => a.len(),
            Data::StrOffsets(a) => a.len() + 1,
            Data::StrBytes(a) => str_bytes(a).len(),
            Data::Bytes(b) => b.len(),
        }) as u64
    }
}

/// The bytes of `strings` its offsets span.
fn str_bytes(strings: &StringArray) -> &[u8] {
    let offsets = strings.value_offsets();
    let (first, last) = (offsets[0] as usize, offsets[offsets.len() - 1] as usize);
    &strings.value_data()[first..last]
}

fn align(n: u64, to: u64) -> u64 {
    n.div_ceil(to) * to
}

/// An index laid out for writing: its header fields, key, sections and where
/// each goes.
pub struct Encoded<'a> {
    flags: u32,
    n_nodes: u64,
    n_edges: u64,
    n_dropped: u64,
    key: Vec<u8>,
    sections: Vec<(u32, Data<'a>, u64)>,
    table_offset: u64,
    trailer_offset: u64,
}

impl<'a> Encoded<'a> {
    /// Lay out the main part of `topology` and `ids` under `key` (not the
    /// in-adjacency: see [`EncodedChunk::incoming`]). Nothing is written yet.
    pub fn new(key: &IndexKey, topology: &'a Topology, ids: &'a IdMap) -> io::Result<Self> {
        Self::with_dropped(key, topology, ids, 0, None)
    }

    /// [`Self::new`], for an index that left out `n_dropped` edge rows,
    /// `kept` saying which it kept (see the module docs).
    pub fn with_dropped(
        key: &IndexKey,
        topology: &'a Topology,
        ids: &'a IdMap,
        n_dropped: u64,
        kept: Option<&BooleanBuffer>,
    ) -> io::Result<Self> {
        let id_array = ids.user_id_array();
        let mut flags = 0;
        let out = topology.out();
        let mut sections: Vec<(u32, Data<'a>)> = vec![
            (OUT_OFFSETS, Data::U64(&out.offsets)),
            (OUT_TARGETS, Data::U32(&out.targets)),
            (OUT_EDGE_IDS, Data::U32(&out.edge_ids)),
        ];
        // Cloning an Arrow array clones `Arc`s to its buffers, not the ids.
        if let Some(ints) = id_array.as_any().downcast_ref::<Int64Array>() {
            sections.push((IDS_INT64, Data::Ints(ints.clone())));
        } else if let Some(strings) = id_array.as_any().downcast_ref::<StringArray>() {
            flags |= FLAG_STRING_IDS;
            sections.push((IDS_STR_OFFSETS, Data::StrOffsets(strings.clone())));
            sections.push((IDS_STR_BYTES, Data::StrBytes(strings.clone())));
        } else {
            return Err(io::Error::other(format!(
                "cannot store node ids of type {}",
                id_array.data_type()
            )));
        }
        let n_dropped = match kept {
            Some(kept) if n_dropped > 0 => {
                sections.push((KEPT_ROWS, Data::Bytes(kept.sliced())));
                n_dropped
            }
            _ => 0,
        };

        let key = key.to_json().to_string().into_bytes();
        let table_offset = align(HEADER_LEN + key.len() as u64, 8);
        let mut at = table_offset + SECTION_ENTRY_LEN * sections.len() as u64;
        let sections = sections
            .into_iter()
            .map(|(kind, data)| {
                at = align(at, SECTION_ALIGN);
                let offset = at;
                at += data.width() * data.count();
                (kind, data, offset)
            })
            .collect();
        Ok(Self {
            flags,
            n_nodes: topology.n_nodes() as u64,
            n_edges: topology.n_edges() as u64,
            n_dropped,
            key,
            sections,
            table_offset,
            trailer_offset: align(at, 8),
        })
    }

    /// The size of the main part, in bytes.
    pub fn file_size(&self) -> u64 {
        self.trailer_offset + TRAILER_LEN
    }

    /// Write the main part to `out`. Returns its identity.
    pub fn write_to(&self, out: impl Write) -> io::Result<MainPart> {
        let mut sink = Sink {
            out,
            crc: crc32fast::Hasher::new(),
            at: 0,
        };
        let mut header = Vec::with_capacity(HEADER_LEN as usize);
        header.extend_from_slice(&MAGIC);
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&self.flags.to_le_bytes());
        header.extend_from_slice(&self.n_nodes.to_le_bytes());
        header.extend_from_slice(&self.n_edges.to_le_bytes());
        header.extend_from_slice(&(self.key.len() as u64).to_le_bytes());
        header.extend_from_slice(&(self.sections.len() as u32).to_le_bytes());
        header.extend_from_slice(&[0; 4]);
        header.extend_from_slice(&self.file_size().to_le_bytes());
        header.extend_from_slice(&self.n_dropped.to_le_bytes());
        header.resize(HEADER_LEN as usize, 0);
        sink.put(&header)?;
        sink.put(&self.key)?;
        sink.pad_to(self.table_offset)?;
        for (kind, data, offset) in &self.sections {
            sink.put(&section_entry(*kind, data, *offset))?;
        }
        for (_, data, offset) in &self.sections {
            sink.pad_to(*offset)?;
            sink.put_data(data)?;
        }
        sink.pad_to(self.trailer_offset)?;
        let crc = sink.crc.clone().finalize();
        let mut trailer = Vec::with_capacity(TRAILER_LEN as usize);
        trailer.extend_from_slice(&self.file_size().to_le_bytes());
        trailer.extend_from_slice(&crc.to_le_bytes());
        trailer.extend_from_slice(&END_MAGIC);
        sink.put(&trailer)?;
        sink.out.flush()?;
        Ok(MainPart {
            len: self.file_size(),
            crc,
        })
    }
}

/// The identity of a file's main part: its length and CRC. A chunk is read
/// from, or appended to, a file only while its main part is still this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainPart {
    pub len: u64,
    pub crc: u32,
}

/// A chunk laid out for writing at a given offset of a file.
pub struct EncodedChunk<'a> {
    kind: u32,
    name: Vec<u8>,
    start: u64,
    sections: Vec<(u32, Data<'a>, u64)>,
    name_offset: u64,
    table_offset: u64,
}

impl<'a> EncodedChunk<'a> {
    /// The chunk of the in-adjacency `incoming`, to be written at `start`.
    pub fn incoming(incoming: &'a Adjacency, start: u64) -> Self {
        Self::new(
            CHUNK_INCOMING,
            Vec::new(),
            vec![
                (IN_OFFSETS, Data::U64(&incoming.offsets)),
                (IN_TARGETS, Data::U32(&incoming.targets)),
                (IN_EDGE_IDS, Data::U32(&incoming.edge_ids)),
            ],
            start,
        )
    }

    /// The chunk of the weight columns `columns` (one value per edge row
    /// each), stored under `key`, to be written at `start`.
    pub fn weights(key: &str, columns: &[&'a [f64]], start: u64) -> Self {
        Self::new(
            CHUNK_WEIGHTS,
            key.as_bytes().to_vec(),
            columns
                .iter()
                .map(|column| (WEIGHTS_F64, Data::F64(column)))
                .collect(),
            start,
        )
    }

    fn new(kind: u32, name: Vec<u8>, sections: Vec<(u32, Data<'a>)>, start: u64) -> Self {
        let mut at = start + CHUNK_HEADER_LEN;
        let sections = sections
            .into_iter()
            .map(|(kind, data)| {
                at = align(at, SECTION_ALIGN);
                let offset = at;
                at += data.width() * data.count();
                (kind, data, offset)
            })
            .collect();
        let name_offset = at;
        let table_offset = align(name_offset + name.len() as u64, 8);
        Self {
            kind,
            name,
            start,
            sections,
            name_offset,
            table_offset,
        }
    }

    fn trailer_offset(&self) -> u64 {
        self.table_offset + SECTION_ENTRY_LEN * self.sections.len() as u64
    }

    /// The offset just past the chunk: the file's length once it is written.
    pub fn end(&self) -> u64 {
        self.trailer_offset() + CHUNK_TRAILER_LEN
    }

    /// The chunk's size, in bytes.
    pub fn size(&self) -> u64 {
        self.end() - self.start
    }

    /// Write the chunk to `out`, which is at its start.
    pub fn write_to(&self, out: impl Write) -> io::Result<()> {
        let mut sink = Sink {
            out,
            crc: crc32fast::Hasher::new(),
            at: self.start,
        };
        let mut header = Vec::with_capacity(CHUNK_HEADER_LEN as usize);
        header.extend_from_slice(&CHUNK_MAGIC);
        header.extend_from_slice(&self.kind.to_le_bytes());
        header.extend_from_slice(&self.end().to_le_bytes());
        sink.put(&header)?;
        for (_, data, offset) in &self.sections {
            sink.pad_to(*offset)?;
            sink.put_data(data)?;
        }
        sink.pad_to(self.name_offset)?;
        let data_crc = std::mem::replace(&mut sink.crc, crc32fast::Hasher::new()).finalize();
        sink.put(&self.name)?;
        sink.pad_to(self.table_offset)?;
        for (kind, data, offset) in &self.sections {
            sink.put(&section_entry(*kind, data, *offset))?;
        }
        let mut trailer = Vec::with_capacity(CHUNK_TRAILER_LEN as usize);
        trailer.extend_from_slice(&self.kind.to_le_bytes());
        trailer.extend_from_slice(&(self.sections.len() as u32).to_le_bytes());
        trailer.extend_from_slice(&self.start.to_le_bytes());
        trailer.extend_from_slice(&self.name_offset.to_le_bytes());
        trailer.extend_from_slice(&(self.name.len() as u64).to_le_bytes());
        trailer.extend_from_slice(&data_crc.to_le_bytes());
        sink.crc.update(&trailer);
        let meta_crc = sink.crc.clone().finalize();
        trailer.extend_from_slice(&meta_crc.to_le_bytes());
        trailer.extend_from_slice(&CHUNK_END_MAGIC);
        trailer.extend_from_slice(&[0; 4]);
        sink.out.write_all(&trailer)?;
        sink.out.flush()
    }
}

/// A section table entry.
fn section_entry(kind: u32, data: &Data<'_>, offset: u64) -> Vec<u8> {
    let mut entry = Vec::with_capacity(SECTION_ENTRY_LEN as usize);
    entry.extend_from_slice(&kind.to_le_bytes());
    entry.extend_from_slice(&(data.width() as u32).to_le_bytes());
    entry.extend_from_slice(&offset.to_le_bytes());
    entry.extend_from_slice(&data.count().to_le_bytes());
    entry
}

/// A writer that keeps a running CRC and position.
struct Sink<W> {
    out: W,
    crc: crc32fast::Hasher,
    at: u64,
}

impl<W: Write> Sink<W> {
    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.crc.update(bytes);
        self.at += bytes.len() as u64;
        self.out.write_all(bytes)
    }

    fn pad_to(&mut self, offset: u64) -> io::Result<()> {
        debug_assert!(offset >= self.at);
        let zeros = [0u8; SECTION_ALIGN as usize];
        while self.at < offset {
            let n = ((offset - self.at) as usize).min(zeros.len());
            self.put(&zeros[..n])?;
        }
        Ok(())
    }

    /// Write one section's array.
    fn put_data(&mut self, data: &Data<'_>) -> io::Result<()> {
        match data {
            Data::U64(v) => self.put_all(v, |x| x.to_le_bytes()),
            Data::F64(v) => self.put_all(v, |x| x.to_le_bytes()),
            Data::U32(v) => self.put_all(v, |x| x.to_le_bytes()),
            Data::Ints(a) => self.put_all(a.values(), |x| x.to_le_bytes()),
            Data::StrOffsets(a) => {
                let offsets = a.value_offsets();
                let base = offsets[0];
                self.put_all(offsets, |x| (x - base).to_le_bytes())
            }
            Data::StrBytes(a) => self.put(str_bytes(a)),
            Data::Bytes(b) => self.put(b.as_slice()),
        }
    }

    /// Write `values` little-endian, a block at a time.
    fn put_all<T: Copy, const N: usize>(
        &mut self,
        values: &[T],
        le: impl Fn(T) -> [u8; N],
    ) -> io::Result<()> {
        let mut block = Vec::with_capacity(64 * 1024);
        for chunk in values.chunks(64 * 1024 / N) {
            block.clear();
            for &v in chunk {
                block.extend_from_slice(&le(v));
            }
            self.put(&block)?;
        }
        Ok(())
    }
}

/// Why a file could not be read as an index.
pub type DecodeError = String;

/// The edge rows an index left out, and which it kept: nothing left out and
/// `None` for a graph with no vertex table (see the module docs).
#[derive(Debug, Clone, Default)]
pub struct Dropped {
    pub n_dropped: u64,
    pub kept: Option<BooleanBuffer>,
}

/// An index read from its file.
pub struct Stored {
    /// With a loader for the in-adjacency, if the file has its chunk.
    pub topology: Topology,
    pub ids: IdMap,
    /// The edge rows left out, and which were kept (see the module docs).
    pub dropped: Dropped,
    pub main: MainPart,
    /// The chunks that check out, in file order.
    pub chunks: Vec<ChunkInfo>,
}

/// Whether `chunks` has one of `kind` named `name`.
pub fn has_chunk(chunks: &[ChunkInfo], kind: u32, name: &[u8]) -> bool {
    chunks.iter().any(|c| c.kind == kind && c.name == name)
}

/// How many in-adjacencies have been read from index files, in this process.
static INCOMING_READS: AtomicU64 = AtomicU64::new(0);

/// How many in-adjacencies this process has read from index files: for
/// tests that an algorithm read one, or did not.
pub fn incoming_reads() -> u64 {
    INCOMING_READS.load(Ordering::Relaxed)
}

/// Read the index file at `path`, which must be stored under `key`.
///
/// The main part is read once, front to back, straight into the arrays the
/// index is made of, so a load holds the index and a small buffer, not the
/// file's bytes as well. The key is checked before the sections are read, so
/// a file for another key costs only its header; the checksum covers every
/// byte of the main part and is checked before any array is used. The chunks
/// are only listed; the in-adjacency's is read when the topology is first
/// asked for it.
pub fn read_file(path: &Path, key: &IndexKey) -> Result<Stored, DecodeError> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let (topology, ids, main, dropped) =
        decode_from(BufReader::with_capacity(BLOCK, &file), len, key)?;
    let (chunks, _) = list_chunks(&mut file, main.len, len);
    let topology = match chunks.iter().find(|c| c.kind == CHUNK_INCOMING) {
        Some(chunk) => {
            let (path, chunk) = (path.to_path_buf(), chunk.clone());
            let (n_nodes, n_edges) = (topology.n_nodes(), topology.n_edges());
            topology.with_incoming_loader(Box::new(move || {
                let incoming = read_incoming(&path, main, &chunk, n_nodes, n_edges).ok()?;
                INCOMING_READS.fetch_add(1, Ordering::Relaxed);
                Some(incoming)
            }))
        }
        None => topology,
    };
    Ok(Stored {
        topology,
        ids,
        dropped,
        main,
        chunks,
    })
}

/// Read the main part of the index in `bytes`, which must be stored under
/// `key`. Chunks are not read.
pub fn decode(bytes: &[u8], key: &IndexKey) -> Result<(Topology, IdMap), DecodeError> {
    decode_from(bytes, bytes.len() as u64, key).map(|(topology, ids, _, _)| (topology, ids))
}

/// One chunk of a file, as listed: where it is and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkInfo {
    pub kind: u32,
    pub name: Vec<u8>,
    pub start: u64,
    pub end: u64,
    /// The end of its header and sections, which the data CRC covers.
    data_end: u64,
    data_crc: u32,
    /// (kind, width, offset, count), in file order.
    sections: Vec<(u32, u64, u64, u64)>,
}

/// The chunks of a file of `len` bytes whose main part is `main_len` bytes,
/// in file order, as far as they check out; and the offset where the next
/// chunk goes (the end of the last chunk that checks out). Only the chunks'
/// headers, names, tables and trailers are read.
pub fn list_chunks<R: Read + Seek>(file: &mut R, main_len: u64, len: u64) -> (Vec<ChunkInfo>, u64) {
    let mut chunks = Vec::new();
    let mut at = main_len;
    while let Some(chunk) = chunk_at(file, at, len) {
        at = chunk.end;
        chunks.push(chunk);
    }
    (chunks, at)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

/// The chunk at `start` of a file of `len` bytes, if one is there whole and
/// its header, name, table and trailer check out.
fn chunk_at<R: Read + Seek>(file: &mut R, start: u64, len: u64) -> Option<ChunkInfo> {
    let read = |file: &mut R, at: u64, n: u64| -> Option<Vec<u8>> {
        file.seek(SeekFrom::Start(at)).ok()?;
        let mut buf = vec![0; usize::try_from(n).ok()?];
        file.read_exact(&mut buf).ok()?;
        Some(buf)
    };
    if len.saturating_sub(start) < CHUNK_HEADER_LEN + CHUNK_TRAILER_LEN {
        return None;
    }
    let header = read(file, start, CHUNK_HEADER_LEN)?;
    let end = u64_at(&header, 8);
    if header[..4] != CHUNK_MAGIC || end > len || end < start + CHUNK_HEADER_LEN + CHUNK_TRAILER_LEN
    {
        return None;
    }
    let kind = u32_at(&header, 4);
    let trailer_offset = end - CHUNK_TRAILER_LEN;
    let trailer = read(file, trailer_offset, CHUNK_TRAILER_LEN)?;
    let n_sections = u32_at(&trailer, 4) as u64;
    let name_offset = u64_at(&trailer, 16);
    let name_len = u64_at(&trailer, 24);
    let table_offset = align(name_offset.checked_add(name_len)?, 8);
    if trailer[40..44] != CHUNK_END_MAGIC
        || u32_at(&trailer, 0) != kind
        || u64_at(&trailer, 8) != start
        || name_offset < start + CHUNK_HEADER_LEN
        || n_sections
            .checked_mul(SECTION_ENTRY_LEN)?
            .checked_add(table_offset)?
            != trailer_offset
    {
        return None;
    }
    let meta = read(file, name_offset, trailer_offset - name_offset)?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(&meta);
    crc.update(&trailer[..36]);
    if crc.finalize() != u32_at(&trailer, 36) {
        return None;
    }
    let name = meta[..name_len as usize].to_vec();
    let table = &meta[(table_offset - name_offset) as usize..];
    let mut sections = Vec::new();
    for entry in table.chunks_exact(SECTION_ENTRY_LEN as usize) {
        let (kind, width) = (u32_at(entry, 0), u32_at(entry, 4) as u64);
        let (offset, count) = (u64_at(entry, 8), u64_at(entry, 16));
        width
            .checked_mul(count)
            .and_then(|n| n.checked_add(offset))
            .filter(|&e| offset >= start + CHUNK_HEADER_LEN && e <= name_offset)?;
        sections.push((kind, width, offset, count));
    }
    sections.sort_by_key(|&(_, _, offset, _)| offset);
    if sections
        .windows(2)
        .any(|w| w[0].2 + w[0].1 * w[0].3 > w[1].2)
    {
        return None;
    }
    Some(ChunkInfo {
        kind,
        name,
        start,
        end,
        data_end: name_offset,
        data_crc: u32_at(&trailer, 32),
        sections,
    })
}

/// Check that the file `file` still has the main part `main`.
fn check_main(file: &mut File, main: MainPart) -> Result<(), DecodeError> {
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    if len < main.len || main.len < HEADER_LEN + TRAILER_LEN {
        return Err("the file's main part has changed".into());
    }
    let mut trailer = [0u8; TRAILER_LEN as usize];
    file.seek(SeekFrom::Start(main.len - TRAILER_LEN))
        .and_then(|_| file.read_exact(&mut trailer))
        .map_err(|e| e.to_string())?;
    if u64_at(&trailer, 0) != main.len || u32_at(&trailer, 8) != main.crc {
        return Err("the file's main part has changed".into());
    }
    Ok(())
}

/// Read the sections of `chunk` from the file at `path`, whose main part
/// must still be `main`. Every byte the data CRC covers is read and checked.
pub fn read_chunk(
    path: &Path,
    main: MainPart,
    chunk: &ChunkInfo,
) -> Result<Vec<(u32, Section)>, DecodeError> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    check_main(&mut file, main)?;
    if file.metadata().map_err(|e| e.to_string())?.len() < chunk.end {
        return Err("the file is shorter than the chunk".into());
    }
    file.seek(SeekFrom::Start(chunk.start))
        .map_err(|e| e.to_string())?;
    let mut src = Source {
        inner: BufReader::with_capacity(BLOCK, file),
        crc: crc32fast::Hasher::new(),
        at: chunk.start,
    };
    let mut sections = Vec::with_capacity(chunk.sections.len());
    for &(kind, width, offset, count) in &chunk.sections {
        src.skip_to(offset)?;
        sections.push((kind, src.section(kind, width, offset, count)?));
    }
    src.skip_to(chunk.data_end)?;
    if src.crc.finalize() != chunk.data_crc {
        return Err("the chunk's checksum does not match".into());
    }
    Ok(sections)
}

/// Read the in-adjacency in `chunk` of the file at `path`, for a topology
/// of `n_nodes` nodes and `n_edges` edges.
fn read_incoming(
    path: &Path,
    main: MainPart,
    chunk: &ChunkInfo,
    n_nodes: usize,
    n_edges: usize,
) -> Result<Adjacency, DecodeError> {
    let mut sections = read_chunk(path, main, chunk)?;
    let mut take = |kind: u32| {
        let i = sections
            .iter()
            .position(|(k, _)| *k == kind)
            .ok_or_else(|| format!("section {kind} is missing"))?;
        Ok::<_, DecodeError>(sections.swap_remove(i).1)
    };
    match (take(IN_OFFSETS)?, take(IN_TARGETS)?, take(IN_EDGE_IDS)?) {
        (Section::U64(offsets), Section::U32(targets), Section::U32(edge_ids))
            if offsets.len() == n_nodes + 1
                && targets.len() == n_edges
                && edge_ids.len() == n_edges =>
        {
            Ok(Adjacency {
                offsets,
                targets,
                edge_ids,
            })
        }
        _ => Err("the in-adjacency is malformed".into()),
    }
}

/// Read the weight columns in `chunk` of the file at `path`, for a topology
/// of `n_edges` edges: one per section, in order.
pub fn read_weights(
    path: &Path,
    main: MainPart,
    chunk: &ChunkInfo,
    n_edges: usize,
) -> Result<Vec<Vec<f64>>, DecodeError> {
    read_chunk(path, main, chunk)?
        .into_iter()
        .map(|(_, section)| match section {
            Section::F64(values) if values.len() == n_edges => Ok(values),
            _ => Err("the weights are malformed".to_string()),
        })
        .collect()
}

/// Append the chunk `encode` lays out (at the offset it is given) to the
/// file at `path`, whose main part must still be `main`, after its last
/// chunk that checks out; unless the file already has a chunk of `kind`
/// named `name`. Returns the file's new length, or `None` if the chunk was
/// there already.
pub fn append_chunk<'a>(
    path: &Path,
    main: MainPart,
    kind: u32,
    name: &[u8],
    encode: impl FnOnce(u64) -> EncodedChunk<'a>,
) -> io::Result<Option<u64>> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    check_main(&mut file, main).map_err(io::Error::other)?;
    let len = file.metadata()?.len();
    let (chunks, at) = list_chunks(&mut file, main.len, len);
    if has_chunk(&chunks, kind, name) {
        return Ok(None);
    }
    let chunk = encode(at);
    debug_assert_eq!((chunk.kind, chunk.name.as_slice()), (kind, name));
    // What follows the last chunk that checks out (one cut short by a
    // crash) is dropped first, so the file never ends in bytes that do not.
    if len > at {
        file.set_len(at)?;
    }
    file.seek(SeekFrom::Start(at))?;
    let mut out = BufWriter::with_capacity(1 << 20, &file);
    chunk.write_to(&mut out)?;
    out.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    Ok(Some(chunk.end()))
}

/// The most a read asks for at once, in bytes.
const BLOCK: usize = 1 << 20;

/// A reader that keeps a running CRC and position.
struct Source<R> {
    inner: R,
    crc: crc32fast::Hasher,
    at: u64,
}

impl<R: Read> Source<R> {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), DecodeError> {
        self.inner.read_exact(buf).map_err(|e| match e.kind() {
            io::ErrorKind::UnexpectedEof => "the file is truncated".to_string(),
            _ => e.to_string(),
        })?;
        self.crc.update(buf);
        self.at += buf.len() as u64;
        Ok(())
    }

    fn bytes(&mut self, n: usize) -> Result<Vec<u8>, DecodeError> {
        let mut buf = vec![0; n];
        self.fill(&mut buf)?;
        Ok(buf)
    }

    /// Read and discard up to `offset`: padding, or a section this build
    /// does not read.
    fn skip_to(&mut self, offset: u64) -> Result<(), DecodeError> {
        let mut buf = vec![0; BLOCK.min(offset.saturating_sub(self.at) as usize)];
        while self.at < offset {
            let n = ((offset - self.at) as usize).min(buf.len());
            self.fill(&mut buf[..n])?;
        }
        Ok(())
    }

    /// `count` little-endian values, a block at a time.
    fn values<T, const N: usize>(
        &mut self,
        count: usize,
        le: fn([u8; N]) -> T,
    ) -> Result<Vec<T>, DecodeError> {
        let mut out = Vec::with_capacity(count);
        let mut buf = vec![0; BLOCK.min(count * N)];
        while out.len() < count {
            let n = ((count - out.len()) * N).min(buf.len());
            self.fill(&mut buf[..n])?;
            out.extend(read(&buf[..n], le));
        }
        Ok(out)
    }
}

impl<R: Read> Source<R> {
    /// The section of `kind` at `offset` (where the source is): `count`
    /// values of `width` bytes. A section this build does not read, or a
    /// known one of the wrong width, is skipped and comes back empty.
    fn section(
        &mut self,
        kind: u32,
        width: u64,
        offset: u64,
        count: u64,
    ) -> Result<Section, DecodeError> {
        let n = usize::try_from(count).map_err(|e| e.to_string())?;
        Ok(match (kind, width) {
            (OUT_OFFSETS | IN_OFFSETS, 8) => Section::U64(self.values(n, u64::from_le_bytes)?),
            (OUT_TARGETS | OUT_EDGE_IDS | IN_TARGETS | IN_EDGE_IDS, 4) => {
                Section::U32(self.values(n, u32::from_le_bytes)?)
            }
            (IDS_INT64, 8) => Section::I64(self.values(n, i64::from_le_bytes)?),
            (WEIGHTS_F64, 8) => Section::F64(self.values(n, f64::from_le_bytes)?),
            (IDS_STR_OFFSETS, 4) => Section::I32(self.values(n, i32::from_le_bytes)?),
            (IDS_STR_BYTES | KEPT_ROWS, 1) => Section::Bytes(self.bytes(n)?),
            _ => {
                self.skip_to(offset + width * count)?;
                Section::Bytes(Vec::new())
            }
        })
    }
}

/// One section, read.
pub enum Section {
    U64(Vec<u64>),
    F64(Vec<f64>),
    U32(Vec<u32>),
    I64(Vec<i64>),
    I32(Vec<i32>),
    Bytes(Vec<u8>),
}

/// Read the index from `reader`, a file of `len` bytes stored under `key`.
fn decode_from(
    reader: impl Read,
    len: u64,
    key: &IndexKey,
) -> Result<(Topology, IdMap, MainPart, Dropped), DecodeError> {
    if len < HEADER_LEN + TRAILER_LEN {
        return Err(format!("{len} bytes is too short for an index file"));
    }
    let mut src = Source {
        inner: reader,
        crc: crc32fast::Hasher::new(),
        at: 0,
    };
    let header = src.bytes(HEADER_LEN as usize)?;
    if header[..8] != MAGIC {
        return Err("not an Ursa index file".into());
    }
    let version = u32::from_le_bytes(header[8..12].try_into().expect("4 bytes"));
    if version != VERSION {
        return Err(format!(
            "index format version {version}; this build reads {VERSION}"
        ));
    }
    let flags = u32::from_le_bytes(header[12..16].try_into().expect("4 bytes"));
    let n_nodes = usize::try_from(u64_at(&header, 16)).map_err(|e| e.to_string())?;
    let n_edges = usize::try_from(u64_at(&header, 24)).map_err(|e| e.to_string())?;
    let key_len = u64_at(&header, 32);
    let n_sections = u32::from_le_bytes(header[40..44].try_into().expect("4 bytes")) as u64;
    let main_len = u64_at(&header, 48);
    let n_dropped = u64_at(&header, 56);
    if main_len > len || main_len < HEADER_LEN + TRAILER_LEN {
        return Err("the file is truncated".into());
    }

    let body = main_len - TRAILER_LEN;
    let key_end = HEADER_LEN
        .checked_add(key_len)
        .filter(|&end| end <= body)
        .ok_or("the key runs past the end of the file")?;
    let stored = src.bytes((key_end - HEADER_LEN) as usize)?;
    let stored: Value =
        serde_json::from_slice(&stored).map_err(|e| format!("the key is not JSON: {e}"))?;
    if IndexKey::from_json(&stored).as_ref() != Some(key) {
        return Err("the file holds an index for another key".into());
    }

    let table = align(key_end, 8);
    let table_end = n_sections
        .checked_mul(SECTION_ENTRY_LEN)
        .and_then(|n| n.checked_add(table))
        .filter(|&end| end <= body)
        .ok_or("the section table runs past the end of the file")?;
    src.skip_to(table)?;
    let entries = src.bytes((table_end - table) as usize)?;
    // (kind, width, offset, count), in file order. Sections are read in one
    // pass, so they may not overlap.
    let mut layout: Vec<(u32, u64, u64, u64)> = Vec::new();
    for entry in entries.chunks_exact(SECTION_ENTRY_LEN as usize) {
        let kind = u32::from_le_bytes(entry[0..4].try_into().expect("4 bytes"));
        let width = u32::from_le_bytes(entry[4..8].try_into().expect("4 bytes")) as u64;
        let offset = u64_at(entry, 8);
        let count = u64_at(entry, 16);
        width
            .checked_mul(count)
            .and_then(|n| n.checked_add(offset))
            .filter(|&end| offset >= table_end && end <= body)
            .ok_or("a section runs past the end of the file")?;
        layout.push((kind, width, offset, count));
    }
    layout.sort_by_key(|&(_, _, offset, _)| offset);
    if layout.windows(2).any(|w| w[0].2 + w[0].1 * w[0].3 > w[1].2) {
        return Err("the sections overlap".into());
    }

    let mut sections: Vec<(u32, u64, u64, Section)> = Vec::new();
    for (kind, width, offset, count) in layout {
        src.skip_to(offset)?;
        // A section this build does not read, or a known one of the wrong
        // width (malformed, below), comes back empty.
        let data = src.section(kind, width, offset, count)?;
        sections.push((kind, width, count, data));
    }
    src.skip_to(body)?;
    let crc = src.crc.clone().finalize();
    let mut trailer = [0u8; TRAILER_LEN as usize];
    src.fill(&mut trailer)?;
    if trailer[12..] != END_MAGIC || u64_at(&trailer, 0) != main_len {
        return Err("the file is truncated".into());
    }
    if u32::from_le_bytes(trailer[8..12].try_into().expect("4 bytes")) != crc {
        return Err("the checksum does not match".into());
    }

    let main = MainPart { len: main_len, crc };
    let mut take = |kind: u32, width: u64, count: Option<usize>| -> Result<Section, DecodeError> {
        let mut found = sections.iter().filter(|(k, _, _, _)| *k == kind);
        let (_, w, c, _) = found
            .next()
            .ok_or_else(|| format!("section {kind} is missing"))?;
        if found.next().is_some() || *w != width {
            return Err(format!("section {kind} is malformed"));
        }
        if count.is_some_and(|count| *c != count as u64) {
            return Err(format!("section {kind} has the wrong length"));
        }
        let i = sections
            .iter()
            .position(|(k, _, _, _)| *k == kind)
            .expect("found above");
        Ok(sections.swap_remove(i).3)
    };
    let malformed = |kind: u32| format!("section {kind} is malformed");
    let mut adjacency = |kinds: [u32; 3]| -> Result<Adjacency, DecodeError> {
        match (
            take(kinds[0], 8, Some(n_nodes + 1))?,
            take(kinds[1], 4, Some(n_edges))?,
            take(kinds[2], 4, Some(n_edges))?,
        ) {
            (Section::U64(offsets), Section::U32(targets), Section::U32(edge_ids)) => {
                Ok(Adjacency {
                    offsets,
                    targets,
                    edge_ids,
                })
            }
            _ => Err(malformed(kinds[0])),
        }
    };
    let out = adjacency([OUT_OFFSETS, OUT_TARGETS, OUT_EDGE_IDS])?;
    let topology = Topology::from_parts(n_nodes, out, None)?;

    let ids: Arc<dyn Array> = if flags & FLAG_STRING_IDS == 0 {
        let Section::I64(values) = take(IDS_INT64, 8, Some(n_nodes))? else {
            return Err(malformed(IDS_INT64));
        };
        Arc::new(Int64Array::from(values))
    } else {
        let (Section::I32(offsets), Section::Bytes(data)) = (
            take(IDS_STR_OFFSETS, 4, Some(n_nodes + 1))?,
            take(IDS_STR_BYTES, 1, None)?,
        ) else {
            return Err(malformed(IDS_STR_OFFSETS));
        };
        if offsets[0] != 0
            || offsets.windows(2).any(|w| w[0] > w[1])
            || offsets[n_nodes] as usize != data.len()
        {
            return Err("the string ids' offsets are malformed".into());
        }
        let offsets = OffsetBuffer::new(ScalarBuffer::from(offsets));
        Arc::new(
            StringArray::try_new(offsets, Buffer::from_vec(data), None)
                .map_err(|e| format!("the string ids are malformed: {e}"))?,
        )
    };
    let ids = IdMap::from_user_ids(ids.as_ref()).map_err(|e| e.to_string())?;
    if ids.len() != n_nodes {
        return Err("the id map does not match the topology".into());
    }
    let dropped = match n_dropped {
        0 => Dropped::default(),
        n_dropped => {
            let rows = usize::try_from(n_dropped)
                .ok()
                .and_then(|d| d.checked_add(n_edges))
                .ok_or("the kept rows are malformed")?;
            let Section::Bytes(bits) = take(KEPT_ROWS, 1, Some(rows.div_ceil(8)))? else {
                return Err(malformed(KEPT_ROWS));
            };
            let kept = BooleanBuffer::new(Buffer::from_vec(bits), 0, rows);
            if kept.count_set_bits() != n_edges {
                return Err("the kept rows do not match the edges".into());
            }
            Dropped {
                n_dropped,
                kept: Some(kept),
            }
        }
    };
    Ok((topology, ids, main, dropped))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

/// The little-endian values in `bytes`.
fn read<T, const N: usize>(
    bytes: &[u8],
    le: fn([u8; N]) -> T,
) -> impl Iterator<Item = T> + use<'_, T, N> {
    bytes
        .chunks_exact(N)
        .map(move |c| le(c.try_into().expect("N bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::arrow::array::ArrayRef;

    fn key() -> IndexKey {
        IndexKey::new(
            "Projection: src, dst\n  TableScan: ties".into(),
            vec![FileIdentity {
                store: "file:///".into(),
                location: "data/edges.parquet".into(),
                size: 1234,
                modified: 1_700_000_000_000_000_000,
                e_tag: Some("\"abc\"".into()),
            }],
        )
    }

    fn built(src: ArrayRef, dst: ArrayRef) -> (Arc<Topology>, Arc<IdMap>) {
        ursa_io::build_topology_batches(&[(src.as_ref(), dst.as_ref())]).unwrap()
    }

    fn encode(key: &IndexKey, topology: &Topology, ids: &IdMap) -> Vec<u8> {
        let encoded = Encoded::new(key, topology, ids).unwrap();
        let mut bytes = Vec::new();
        encoded.write_to(&mut bytes).unwrap();
        assert_eq!(bytes.len() as u64, encoded.file_size());
        bytes
    }

    fn same(a: &Topology, b: &Topology) {
        assert_eq!(a.n_nodes(), b.n_nodes());
        assert_eq!(a.out().offsets, b.out().offsets);
        assert_eq!(a.out().targets, b.out().targets);
        assert_eq!(a.out().edge_ids, b.out().edge_ids);
    }

    #[test]
    fn int_and_string_indexes_round_trip() {
        let ints = |v: &[i64]| Arc::new(Int64Array::from(v.to_vec())) as ArrayRef;
        let (topology, ids) = built(ints(&[10, 20, 10, 30]), ints(&[20, 30, 30, 10]));
        let bytes = encode(&key(), &topology, &ids);
        assert_eq!(&bytes[..8], b"URSAIDX\0");
        let (back, back_ids) = decode(&bytes, &key()).unwrap();
        same(&topology, &back);
        assert!(back.incoming_if_built().is_none());
        assert_eq!(
            back_ids.user_id_array().as_ref(),
            ids.user_id_array().as_ref()
        );

        // The in-adjacency is never part of the main part.
        topology.incoming();
        assert_eq!(encode(&key(), &topology, &ids), bytes);

        let strs = |v: &[&str]| Arc::new(StringArray::from(v.to_vec())) as ArrayRef;
        let (topology, ids) = built(strs(&["a", "bé", "a"]), strs(&["bé", "c", "c"]));
        let bytes = encode(&key(), &topology, &ids);
        let (back, back_ids) = decode(&bytes, &key()).unwrap();
        same(&topology, &back);
        assert_eq!(
            back_ids.user_id_array().as_ref(),
            ids.user_id_array().as_ref()
        );

        // Sections are 64-byte aligned.
        let n_sections = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        let table = align(HEADER_LEN + u64_at(&bytes, 32), 8) as usize;
        for i in 0..n_sections {
            assert_eq!(u64_at(&bytes, table + 24 * i + 8) % 64, 0);
        }
    }

    #[test]
    fn the_rows_a_vertex_table_left_out_round_trip() {
        let ints = |v: &[i64]| Arc::new(Int64Array::from(v.to_vec())) as ArrayRef;
        let (topology, ids) = built(ints(&[1, 2]), ints(&[2, 3]));
        // Rows 0 and 3 of four were kept, rows 1 and 2 left out.
        let kept = BooleanBuffer::from(vec![true, false, false, true]);
        let encoded = Encoded::with_dropped(&key(), &topology, &ids, 2, Some(&kept)).unwrap();
        let mut bytes = Vec::new();
        encoded.write_to(&mut bytes).unwrap();
        let (back, _, _, dropped) = decode_from(&bytes[..], bytes.len() as u64, &key()).unwrap();
        same(&topology, &back);
        assert_eq!(dropped.n_dropped, 2);
        assert_eq!(dropped.kept, Some(kept));
        // A file with nothing left out has no such section, as before #311.
        let plain = encode(&key(), &topology, &ids);
        let (_, _, _, none) = decode_from(&plain[..], plain.len() as u64, &key()).unwrap();
        assert_eq!((none.n_dropped, none.kept), (0, None));
    }

    #[test]
    fn an_empty_index_round_trips() {
        let (topology, ids) =
            ursa_io::topology::empty_topology(&datafusion::arrow::datatypes::DataType::Utf8);
        let (back, back_ids) = decode(&encode(&key(), &topology, &ids), &key()).unwrap();
        assert_eq!((back.n_nodes(), back.n_edges()), (0, 0));
        assert!(back_ids.is_empty());
    }

    #[test]
    fn corruption_truncation_and_another_key_are_errors() {
        let ints = |v: &[i64]| Arc::new(Int64Array::from(v.to_vec())) as ArrayRef;
        let (topology, ids) = built(ints(&[1, 2, 3]), ints(&[2, 3, 1]));
        let bytes = encode(&key(), &topology, &ids);
        for cut in [0, 10, 64, bytes.len() / 2, bytes.len() - 1] {
            assert!(decode(&bytes[..cut], &key()).is_err(), "cut at {cut}");
        }
        for at in [9, 70, bytes.len() / 2, bytes.len() - 20] {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x40;
            assert!(decode(&flipped, &key()).is_err(), "flipped at {at}");
        }
        let mut other = key();
        other.files[0].size += 1;
        assert_eq!(
            decode(&bytes, &other).unwrap_err(),
            "the file holds an index for another key"
        );
        assert!(decode(
            b"not an index at all, just some text..........................................",
            &key()
        )
        .is_err());
    }

    /// Write the main part of `topology` to `path`; its identity.
    fn write_main(path: &Path, topology: &Topology, ids: &IdMap) -> MainPart {
        let encoded = Encoded::new(&key(), topology, ids).unwrap();
        let mut file = File::create(path).unwrap();
        encoded.write_to(&mut file).unwrap()
    }

    #[test]
    fn the_in_adjacency_is_appended_and_read_on_first_demand() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        let ints = |v: &[i64]| Arc::new(Int64Array::from(v.to_vec())) as ArrayRef;
        let (topology, ids) = built(
            ints(&[10, 20, 10, 30, 20, 40]),
            ints(&[20, 30, 30, 10, 40, 10]),
        );
        let main = write_main(&path, &topology, &ids);
        assert_eq!(main.len, std::fs::metadata(&path).unwrap().len());

        // No chunk: the transpose is built, nothing is read.
        let reads = incoming_reads();
        let stored = read_file(&path, &key()).unwrap();
        assert!(stored.chunks.is_empty());
        assert_eq!(
            stored.topology.incoming().targets,
            topology.incoming().targets
        );
        assert_eq!(incoming_reads(), reads);

        // Appended once; a second append finds it there.
        let incoming = topology.incoming().clone();
        let end = append_chunk(&path, main, CHUNK_INCOMING, b"", |at| {
            EncodedChunk::incoming(&incoming, at)
        })
        .unwrap()
        .unwrap();
        assert_eq!(end, std::fs::metadata(&path).unwrap().len());
        let again = append_chunk(&path, main, CHUNK_INCOMING, b"", |at| {
            EncodedChunk::incoming(&incoming, at)
        });
        assert_eq!(again.unwrap(), None);

        let stored = read_file(&path, &key()).unwrap();
        assert_eq!(stored.chunks.len(), 1);
        assert_eq!(stored.main, main);
        same(&topology, &stored.topology);
        // Listed, not read, until asked for.
        assert!(stored.topology.incoming_if_built().is_none());
        let back = stored.topology.incoming();
        assert!(incoming_reads() > reads);
        assert_eq!(
            (&back.offsets, &back.targets, &back.edge_ids),
            (&incoming.offsets, &incoming.targets, &incoming.edge_ids)
        );
        // The main part reads the same with or without the chunk.
        let bytes = std::fs::read(&path).unwrap();
        let (main_only, _) = decode(&bytes, &key()).unwrap();
        same(&topology, &main_only);
    }

    #[test]
    fn a_damaged_chunk_is_ignored_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        let ints = |v: &[i64]| Arc::new(Int64Array::from(v.to_vec())) as ArrayRef;
        let (topology, ids) = built(ints(&[1, 2, 3, 1]), ints(&[2, 3, 1, 3]));
        let main = write_main(&path, &topology, &ids);
        let incoming = topology.incoming().clone();
        let end = append_chunk(&path, main, CHUNK_INCOMING, b"", |at| {
            EncodedChunk::incoming(&incoming, at)
        })
        .unwrap()
        .unwrap();
        let good = std::fs::read(&path).unwrap();

        // A flipped byte in the chunk's arrays: listed, but its data CRC
        // fails, so the transpose is built instead, the same.
        let mut bytes = good.clone();
        bytes[main.len as usize + 64] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let stored = read_file(&path, &key()).unwrap();
        assert_eq!(stored.chunks.len(), 1);
        assert!(read_chunk(&path, main, &stored.chunks[0]).is_err());
        assert_eq!(stored.topology.incoming().targets, incoming.targets);
        assert_eq!(stored.topology.incoming().edge_ids, incoming.edge_ids);

        // A chunk cut short (a crash mid-append): not listed, and the next
        // append writes over it.
        std::fs::write(&path, &good[..good.len() - 10]).unwrap();
        assert!(read_file(&path, &key()).unwrap().chunks.is_empty());
        let again = append_chunk(&path, main, CHUNK_INCOMING, b"", |at| {
            EncodedChunk::incoming(&incoming, at)
        })
        .unwrap();
        assert_eq!(again, Some(end));
        assert_eq!(std::fs::read(&path).unwrap(), good);

        // Another main part: nothing is appended or read.
        let other = MainPart {
            crc: main.crc ^ 1,
            ..main
        };
        assert!(append_chunk(&path, other, CHUNK_INCOMING, b"", |at| {
            EncodedChunk::incoming(&incoming, at)
        })
        .is_err());
        let chunk = &read_file(&path, &key()).unwrap().chunks[0];
        assert!(read_chunk(&path, other, chunk).is_err());
        assert!(read_chunk(&path, main, chunk).is_ok());
    }

    #[test]
    fn weight_chunks_round_trip_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        let ints = |v: &[i64]| Arc::new(Int64Array::from(v.to_vec())) as ArrayRef;
        let (topology, ids) = built(ints(&[1, 2, 3]), ints(&[2, 3, 1]));
        let main = write_main(&path, &topology, &ids);
        let (a, b) = (vec![0.5, 1.0, f64::MAX], vec![2.0, 0.0, 1e-300]);
        for (key, columns) in [("w1", vec![&a[..]]), ("w2", vec![&a[..], &b[..]])] {
            let appended = append_chunk(&path, main, CHUNK_WEIGHTS, key.as_bytes(), |at| {
                EncodedChunk::weights(key, &columns, at)
            });
            assert!(appended.unwrap().is_some());
        }
        let stored = read_file(&path, &key()).unwrap();
        assert_eq!(stored.chunks.len(), 2);
        let chunk = |name: &[u8]| stored.chunks.iter().find(|c| c.name == name).unwrap();
        assert_eq!(
            read_weights(&path, main, chunk(b"w1"), 3).unwrap(),
            std::slice::from_ref(&a)
        );
        assert_eq!(read_weights(&path, main, chunk(b"w2"), 3).unwrap(), [a, b]);
        // The wrong edge count is an error, never a short column.
        assert!(read_weights(&path, main, chunk(b"w1"), 4).is_err());
    }

    #[test]
    fn a_version_1_file_is_not_read() {
        let ints = |v: &[i64]| Arc::new(Int64Array::from(v.to_vec())) as ArrayRef;
        let (topology, ids) = built(ints(&[1, 2]), ints(&[2, 3]));
        let mut bytes = encode(&key(), &topology, &ids);
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            decode(&bytes, &key()).unwrap_err(),
            "index format version 1; this build reads 2"
        );
    }

    #[test]
    fn keys_round_trip_through_json() {
        let k = key();
        assert_eq!(IndexKey::from_json(&k.to_json()), Some(k.clone()));
        let mut no_tag = k.clone();
        no_tag.files[0].e_tag = None;
        assert_eq!(IndexKey::from_json(&no_tag.to_json()), Some(no_tag.clone()));
        assert_ne!(no_tag.hash(), k.hash());
    }
}
