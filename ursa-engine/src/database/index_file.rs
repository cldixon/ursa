//! One stored graph index: `club.ursa.d/indexes/<hash>.idx`.
//!
//! A file holds one built index, the CSR topology and the id map, under the
//! full key it was built for (the edge plan's fingerprint and the identity of
//! every source file), followed by optional **chunks**: parts of the index
//! that are built later, or that only some queries need, such as the
//! in-adjacency. Version 5, all integers little-endian.
//!
//! The **main part** comes first:
//!
//! ```text
//! offset  size  field
//!      0     8  magic            "URSAIDX\0"
//!      8     4  version          5
//!     12     4  flags            bit 0: string ids
//!     16     8  n_nodes
//!     24     8  n_edges
//!     32     8  key_len          bytes of the key that follows the header
//!     40     4  n_sections
//!     44     4  reserved (zero)
//!     48     8  main_len         bytes of the main part, trailer included
//!     56     8  n_dropped        edge rows left out (see below); zero before
//!                                #311, and for a graph with no vertex table
//!     64     …  key              JSON: {"version": 2, "fingerprint": …, "files": [{…}, …]}
//!      …     …  section table    at the next multiple of 8; 32 bytes a section:
//!                                kind u32, element width u32, offset u64, count u64,
//!                                CRC-32 of the section's bytes u32, zero u32
//!      …     …  sections         each at a multiple of 64 from the file's start
//!      …    16  trailer          at the next multiple of 8: main_len u64,
//!                                CRC-32 of the header, key and section table
//!                                (every byte before the first section) u32,
//!                                "XDIU"
//! ```
//!
//! Every byte between the table and the trailer that is not in a section
//! (alignment padding) is zero. The trailer's CRC covers the section CRCs, so
//! it still identifies every byte of the main part (see [`MainPart`]), while
//! each section can be checked on its own: a reader that maps the file checks
//! the small sections when it loads and the edge arrays only when something
//! reads them whole (see `super::mapped`).
//!
//! Its sections, in this order: the out-adjacency's offsets (`u64`,
//! `n_nodes + 1`), targets (`u32`, `n_edges`) and edge ids (`u32`, `n_edges`);
//! then the ids, either `i64` values (`n_nodes`) or, for string ids,
//! Arrow-style `i32` offsets (`n_nodes + 1`, from 0) and UTF-8 bytes; then
//! the ids' **lookup table** (kind 12, `u32`): the open-addressing table
//! that finds an id's node, `n_nodes + n_nodes / 2 + 1` slots, each a node or
//! `u32::MAX` for an empty one. Its layout and the hashes it places ids by
//! are fixed (`ursa_core::id_map`, `StoredTable`). With it a process that
//! has just mapped the file finds a seed's node by reading a slot or two and
//! comparing the id there, where before it hashed every id of the graph into
//! a table of its own first (#421).
//!
//! An index whose nodes were relabelled for locality (#172,
//! `ursa_core::NodeOrder`) has one more section, the nodes' **original ids**
//! (kind 13, `u32`, `n_nodes`): the first-seen number of each node, which
//! component labels and the seeded community kernels are defined on. The id
//! column is in the relabelled order. Without the section the nodes are in
//! their original order.
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
//! **Version 4** (0.12) is version 5 without the original ids: its nodes
//! were never relabelled, and they keep their numbering when it is read. It is
//! still read, and mapped as version 5 is.
//!
//! **Version 3** (0.11) is version 4 without the lookup table. It is still
//! read, and mapped as version 5 is; the table is then built in memory, the
//! first time a query looks an id up (one that runs an algorithm over the
//! whole graph and names no node never does). **Version 2** (0.10) is the
//! same layout with 24-byte table entries (no section CRCs) and one CRC over
//! every byte before the trailer. It is still read, whole and checked (see
//! [`read_file`]), and its lookup table built in the same way. A writer
//! stores any of them again as version 5 (`IndexStore`).
//!
//! **Why raw arrays, not Arrow IPC.** The arrays have different lengths and
//! no schema: an IPC stream would need one record batch per array, or padding
//! them into one table, and its metadata says nothing a section table does
//! not. Raw little-endian arrays at 64-byte-aligned offsets are what a later
//! memory-mapped reader wants (each section is a ready `&[u32]`/`&[u64]` on a
//! little-endian machine), need no dependency, and the string ids are already
//! in Arrow's layout, so they become a `StringArray` without a copy once
//! mapped. Where the platform allows, [`read_file`] maps the file and the
//! arrays are borrowed from the map, never copied (`super::mapped`);
//! elsewhere the main part is read into memory in parallel and the arrays are
//! borrowed from that.
//!
//! **Corruption.** The main part is read only if its length matches the
//! trailer, the trailer's CRC matches, the version is 2, 3 or 4, the key matches
//! the key asked for, every section lies within it, the padding is zero, the
//! CRC of every section but the edge arrays matches (all of them, for a file
//! that is not mapped), and the arrays form a valid topology and id map
//! (`ursa_core::Topology::from_checksummed_parts`, which checks the offsets;
//! `IdMap::from_stored_ids`, which takes the ids and their table as they
//! lie, the table's CRC vouching that it is the one written for them; a
//! version 2 file, whose sections have no CRC of their own, is checked in
//! full by `Topology::from_parts`). An id stored twice, which no writer
//! stores, is found when a version 2 or 3 file's table is built, and is then
//! the error of the query that looked an id up. Anything else is
//! an error, which the store treats as a miss. The edge arrays' CRCs of a
//! mapped file are checked before a whole-graph algorithm runs
//! (`super::mapped::Unverified`). A chunk that does not check out is ignored,
//! and what it held is rebuilt when needed.
//!
//! **Version 1** files (the in-adjacency, if any, inside the main part) are
//! not read: they are a miss like any other unreadable file, so the index is
//! built again once and stored as version 5 in their place.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use datafusion::arrow::array::{Array, Int64Array, StringArray};
use datafusion::arrow::buffer::{BooleanBuffer, Buffer, OffsetBuffer, ScalarBuffer};
use datafusion::arrow::datatypes::ArrowNativeType;
use serde_json::{json, Value};
use ursa_core::id_map::lookup_table_len;
use ursa_core::{Adjacency, EdgeIds, IdMap, Packed, Topology};

use super::mapped::{Region, Unverified};
use crate::graph::FileIdentity;

/// The first bytes of every index file.
pub const MAGIC: [u8; 8] = *b"URSAIDX\0";
/// The last bytes of every index file.
const END_MAGIC: [u8; 4] = *b"XDIU";
/// The index file version this build writes. It reads this one and
/// [`VERSION_5`] to [`VERSION_2`].
pub const VERSION: u32 = 6;
/// The version 0.13 wrote: version 6 with the out-adjacency's edge ids
/// always stored, and no compressed layout.
pub const VERSION_5: u32 = 5;
/// The version 0.12 wrote: version 5 without the original node ids (its
/// nodes are never relabelled, #172).
pub const VERSION_4: u32 = 4;
/// The version 0.11 wrote: version 4 without the ids' lookup table.
pub const VERSION_3: u32 = 3;
/// The version 0.10 wrote: no section CRCs, one CRC over the main part.
pub const VERSION_2: u32 = 2;
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
/// A main part's section table entry (version 3).
const SECTION_ENTRY_LEN: u64 = 32;
/// A chunk's section table entry, and a version 2 main part's: no CRC.
const CHUNK_ENTRY_LEN: u64 = 24;
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
const ID_TABLE: u32 = 12;
const ORIGINAL_IDS: u32 = 13;
const OUT_BYTE_OFFSETS: u32 = 14;
const OUT_BYTES: u32 = 15;
const IN_BYTE_OFFSETS: u32 = 16;
const IN_BYTES: u32 = 17;

/// The version of the key's form. Version 2 (#327) is the portable key:
/// paths inside the database's directory relative to it, and only the file
/// options that change what is read (`crate::graph::index`). A key of
/// another version is not read, so an index stored under an older key is a
/// miss, never a wrong hit.
pub const KEY_VERSION: u64 = 2;

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

    /// The key as JSON, as the file header and the manifest hold it:
    /// `{"version": 2, "fingerprint": …, "files": […]}`.
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
        json!({ "version": KEY_VERSION, "fingerprint": self.fingerprint, "files": files })
    }

    /// The key from its JSON, or `None` if it is not one.
    pub fn from_json(value: &Value) -> Option<Self> {
        if value.get("version")?.as_u64()? != KEY_VERSION {
            return None;
        }
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
    /// The lookup table of an id map.
    IdTable(IdTable<'a>),
    /// An adjacency's edge ids: read as they are when held, else built for
    /// the write (once, kept until the write is done) and not kept on the
    /// adjacency, so storing an index does not make it hold them (#170).
    EdgeIds(&'a EdgeIds, OnceLock<ScalarBuffer<u32>>),
}

impl<'a> Data<'a> {
    /// The edge ids `ids`, as a section.
    fn edge_ids(ids: &'a EdgeIds) -> Self {
        Data::EdgeIds(ids, OnceLock::new())
    }
}

/// The edge ids of a [`Data::EdgeIds`] section.
fn ids_of<'d>(ids: &'d EdgeIds, built: &'d OnceLock<ScalarBuffer<u32>>) -> &'d [u32] {
    built.get_or_init(|| ids.detached())
}

/// The lookup table of `ids`, built when the file is written (not when it is
/// laid out, which only needs its length, and happens in the query that built
/// the index).
struct IdTable<'a> {
    ids: &'a IdMap,
    slots: OnceLock<Result<ScalarBuffer<u32>, String>>,
}

impl IdTable<'_> {
    fn build(&self) -> Result<&ScalarBuffer<u32>, String> {
        self.slots
            .get_or_init(|| self.ids.lookup_table().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(Clone::clone)
    }

    /// The table, which [`Encoded::write_to`] has built by now.
    fn slots(&self) -> &[u32] {
        self.build()
            .expect("write_to builds the lookup table before it is written")
    }
}

impl Data<'_> {
    fn width(&self) -> u64 {
        match self {
            Data::U64(_) | Data::F64(_) | Data::Ints(_) => 8,
            Data::U32(_) | Data::StrOffsets(_) | Data::IdTable(_) | Data::EdgeIds(..) => 4,
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
            Data::IdTable(t) => return lookup_table_len(t.ids.len()),
            Data::EdgeIds(ids, _) => ids.len(),
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
    /// [`VERSION`]; or [`VERSION_4`], [`VERSION_3`] or [`VERSION_2`], for a
    /// test of reading what 0.12, 0.11 or 0.10 wrote.
    version: u32,
    flags: u32,
    n_nodes: u64,
    n_edges: u64,
    n_dropped: u64,
    key: Vec<u8>,
    sections: Vec<(u32, Data<'a>, u64)>,
    table_offset: u64,
    trailer_offset: u64,
    /// The out-adjacency's edge ids, which an older version always stores.
    out_ids: Option<&'a EdgeIds>,
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
        let mut sections: Vec<(u32, Data<'a>)> = adjacency_sections(
            out,
            [
                OUT_OFFSETS,
                OUT_TARGETS,
                OUT_BYTE_OFFSETS,
                OUT_BYTES,
                OUT_EDGE_IDS,
            ],
        );
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
        sections.push((
            ID_TABLE,
            Data::IdTable(IdTable {
                ids,
                slots: OnceLock::new(),
            }),
        ));
        // The original id of each node, if the build relabelled them (#172).
        if let Some(original) = topology.original_ids() {
            sections.push((ORIGINAL_IDS, Data::U32(original)));
        }
        let n_dropped = match kept {
            Some(kept) if n_dropped > 0 => {
                sections.push((KEPT_ROWS, Data::Bytes(kept.sliced())));
                n_dropped
            }
            _ => 0,
        };

        let key = key.to_json().to_string().into_bytes();
        let (n_nodes, n_edges) = (topology.n_nodes() as u64, topology.n_edges() as u64);
        let mut encoded =
            Self::laid_out(VERSION, flags, n_nodes, n_edges, n_dropped, key, sections);
        encoded.out_ids = Some(&out.edge_ids);
        Ok(encoded)
    }

    /// Place the sections after the header, key and table of a file of
    /// `version`.
    fn laid_out(
        version: u32,
        flags: u32,
        n_nodes: u64,
        n_edges: u64,
        n_dropped: u64,
        key: Vec<u8>,
        sections: Vec<(u32, Data<'a>)>,
    ) -> Self {
        let entry_len = match version {
            VERSION_2 => CHUNK_ENTRY_LEN,
            _ => SECTION_ENTRY_LEN,
        };
        let table_offset = align(HEADER_LEN + key.len() as u64, 8);
        let mut at = table_offset + entry_len * sections.len() as u64;
        let sections = sections
            .into_iter()
            .map(|(kind, data)| {
                at = align(at, SECTION_ALIGN);
                let offset = at;
                at += data.width() * data.count();
                (kind, data, offset)
            })
            .collect();
        Self {
            version,
            flags,
            n_nodes,
            n_edges,
            n_dropped,
            key,
            sections,
            table_offset,
            trailer_offset: align(at, 8),
            out_ids: None,
        }
    }

    /// This index laid out as version 2, as 0.10 wrote it: for tests of
    /// reading such a file.
    #[doc(hidden)]
    pub fn as_version_2(self) -> Self {
        self.in_version(VERSION_2)
    }

    /// This index laid out as version 3, as 0.11 wrote it: for tests of
    /// reading such a file.
    #[doc(hidden)]
    pub fn as_version_3(self) -> Self {
        self.in_version(VERSION_3)
    }

    /// This index laid out as version 4, as 0.12 wrote it (no original
    /// node ids): for tests of reading such a file. Its nodes must not have
    /// been relabelled, as 0.12 never relabelled them.
    #[doc(hidden)]
    pub fn as_version_4(self) -> Self {
        self.in_version(VERSION_4)
    }

    /// This index laid out as version 5, as 0.13 wrote it (its edge ids
    /// always stored): for tests of reading such a file. It must be in the
    /// plain layout, as 0.13 had no other.
    #[doc(hidden)]
    pub fn as_version_5(self) -> Self {
        self.in_version(VERSION_5)
    }

    /// This index laid out as an older `version`: its out-adjacency's edge
    /// ids always stored, no compressed layout, before version 5 no original
    /// node ids and, before version 4, no lookup table.
    fn in_version(self, version: u32) -> Self {
        let Self {
            flags,
            n_nodes,
            n_edges,
            n_dropped,
            key,
            sections,
            out_ids,
            ..
        } = self;
        assert!(
            !sections.iter().any(|(k, _, _)| *k == OUT_BYTES),
            "version {version} has no compressed layout"
        );
        let mut sections: Vec<(u32, Data<'a>)> = sections
            .into_iter()
            .filter(|(k, _, _)| version == VERSION_5 || *k != ORIGINAL_IDS)
            .filter(|(k, _, _)| version >= VERSION_4 || *k != ID_TABLE)
            .map(|(k, d, _)| (k, d))
            .collect();
        if !sections.iter().any(|(k, _)| *k == OUT_EDGE_IDS) {
            let ids = out_ids.expect("the out-adjacency's edge ids");
            sections.insert(2, (OUT_EDGE_IDS, Data::edge_ids(ids)));
        }
        let mut encoded =
            Self::laid_out(version, flags, n_nodes, n_edges, n_dropped, key, sections);
        encoded.out_ids = out_ids;
        encoded
    }

    /// The size of the main part, in bytes.
    pub fn file_size(&self) -> u64 {
        self.trailer_offset + TRAILER_LEN
    }

    /// Write the main part to `out`. Returns its identity.
    pub fn write_to(&self, out: impl Write) -> io::Result<MainPart> {
        let v2 = self.version == VERSION_2;
        for (_, data, _) in &self.sections {
            if let Data::IdTable(table) = data {
                table.build().map_err(io::Error::other)?;
            }
        }
        // Each section's CRC goes in the table, before the section: one pass
        // over the arrays first (in parallel), then one to write them.
        let crcs: Vec<Option<u32>> = self
            .sections
            .iter()
            .map(|(_, data, _)| (!v2).then(|| data_crc(data)))
            .collect();
        let mut sink = Sink {
            out,
            crc: crc32fast::Hasher::new(),
            at: 0,
            hash: true,
        };
        let mut header = Vec::with_capacity(HEADER_LEN as usize);
        header.extend_from_slice(&MAGIC);
        header.extend_from_slice(&self.version.to_le_bytes());
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
        for ((kind, data, offset), crc) in self.sections.iter().zip(&crcs) {
            sink.put(&section_entry(*kind, data, *offset, *crc))?;
        }
        // The trailer CRC (but version 2's) covers the header, key and
        // table; the sections have their own.
        let meta_crc = sink.crc.clone().finalize();
        sink.hash = v2;
        for (_, data, offset) in &self.sections {
            sink.pad_to(*offset)?;
            sink.put_data(data)?;
        }
        sink.pad_to(self.trailer_offset)?;
        let crc = match v2 {
            true => sink.crc.clone().finalize(),
            false => meta_crc,
        };
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

/// The sections of the adjacency `adj`, of the kinds `[offsets, targets,
/// byte offsets, bytes, edge ids]`: its slot offsets; its targets, or in the
/// compressed layout (#173) its byte offsets and bytes; and its edge ids,
/// unless they are the identity (#155), which a reader rebuilds for free.
fn adjacency_sections(adj: &Adjacency, kinds: [u32; 5]) -> Vec<(u32, Data<'_>)> {
    let [offsets, targets, byte_offsets, bytes, edge_ids] = kinds;
    let mut sections = vec![(offsets, Data::U64(&adj.offsets))];
    match &adj.packed {
        None => sections.push((targets, Data::U32(&adj.targets))),
        Some(p) => {
            sections.push((byte_offsets, Data::U64(&p.byte_offsets)));
            sections.push((bytes, Data::Bytes(p.bytes.inner().clone())));
        }
    }
    // Ids held (an older file's, read back) are checked: up to the first
    // that is not its slot, which for most is the first few.
    let ids = &adj.edge_ids;
    let identity = ids.is_identity()
        || (ids.is_built() && ids.iter().enumerate().all(|(k, &e)| k as u32 == e));
    if !identity {
        sections.push((edge_ids, Data::edge_ids(ids)));
    }
    sections
}

/// The identity of a file's main part: its length and CRC. A chunk is read
/// from, or appended to, a file only while its main part is still this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
            adjacency_sections(
                incoming,
                [
                    IN_OFFSETS,
                    IN_TARGETS,
                    IN_BYTE_OFFSETS,
                    IN_BYTES,
                    IN_EDGE_IDS,
                ],
            ),
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
        self.table_offset + CHUNK_ENTRY_LEN * self.sections.len() as u64
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
            hash: true,
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
            sink.put(&section_entry(*kind, data, *offset, None))?;
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

/// A section table entry: with the section's CRC in a main part (but
/// version 2's), without in a chunk.
fn section_entry(kind: u32, data: &Data<'_>, offset: u64, crc: Option<u32>) -> Vec<u8> {
    let mut entry = Vec::with_capacity(SECTION_ENTRY_LEN as usize);
    entry.extend_from_slice(&kind.to_le_bytes());
    entry.extend_from_slice(&(data.width() as u32).to_le_bytes());
    entry.extend_from_slice(&offset.to_le_bytes());
    entry.extend_from_slice(&data.count().to_le_bytes());
    if let Some(crc) = crc {
        entry.extend_from_slice(&crc.to_le_bytes());
        entry.extend_from_slice(&[0; 4]);
    }
    entry
}

/// The CRC-32 of the bytes `data` is written as.
fn data_crc(data: &Data<'_>) -> u32 {
    /// The bytes of `values`, which are plain numbers.
    fn bytes_of<T: Copy>(values: &[T]) -> &[u8] {
        // SAFETY: `T` is a primitive number here (u64, u32, i64, f64): every
        // byte of it is initialized, and the view spans the slice exactly.
        unsafe { std::slice::from_raw_parts(values.as_ptr().cast(), size_of_val(values)) }
    }
    // On a little-endian machine an array is written as it is in memory, so
    // its CRC is computed in place, in parallel.
    if cfg!(target_endian = "little") {
        let bytes = match data {
            Data::U64(v) => Some(bytes_of(v)),
            Data::F64(v) => Some(bytes_of(v)),
            Data::U32(v) => Some(bytes_of(v)),
            Data::Ints(a) => Some(bytes_of(a.values())),
            Data::StrBytes(a) => Some(str_bytes(a)),
            Data::Bytes(b) => Some(b.as_slice()),
            Data::IdTable(t) => Some(bytes_of(t.slots())),
            Data::EdgeIds(ids, built) => Some(bytes_of(ids_of(ids, built))),
            Data::StrOffsets(_) => None,
        };
        if let Some(bytes) = bytes {
            return super::mapped::crc(bytes);
        }
    }
    let mut sink = Sink {
        out: io::sink(),
        crc: crc32fast::Hasher::new(),
        at: 0,
        hash: true,
    };
    sink.put_data(data)
        .expect("writing to io::sink does not fail");
    sink.crc.finalize()
}

/// A writer that keeps a running CRC (while `hash` is set) and position.
struct Sink<W> {
    out: W,
    crc: crc32fast::Hasher,
    at: u64,
    hash: bool,
}

impl<W: Write> Sink<W> {
    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.hash {
            self.crc.update(bytes);
        }
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
            Data::IdTable(t) => self.put_all(t.slots(), |x| x.to_le_bytes()),
            Data::EdgeIds(ids, built) => self.put_all(ids_of(ids, built), |x| x.to_le_bytes()),
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
    /// The file's version: [`VERSION`], or [`VERSION_4`], [`VERSION_3`] or
    /// [`VERSION_2`] for a file 0.12, 0.11 or 0.10 wrote.
    pub version: u32,
    /// The checksums of a mapped file that are checked only when something
    /// reads the arrays whole; `None` when every one was checked as it loaded.
    pub unverified: Option<Arc<Unverified>>,
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

thread_local! {
    /// How many in-adjacencies this thread has read. Tests run in parallel
    /// in one process, so a test that must see *no* read counts its own
    /// thread's: the loader runs on the thread that asks for the
    /// in-adjacency.
    static THREAD_INCOMING_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Count one in-adjacency read, for the process and for this thread.
fn count_incoming_read() {
    INCOMING_READS.fetch_add(1, Ordering::Relaxed);
    THREAD_INCOMING_READS.with(|n| n.set(n.get() + 1));
}

/// How many in-adjacencies this thread has read from index files.
pub fn thread_incoming_reads() -> u64 {
    THREAD_INCOMING_READS.with(std::cell::Cell::get)
}

/// Read the index file at `path`, which must be stored under `key`.
///
/// The header, key and section table are read first, so a file for another
/// key costs only those. Then, for a version 3 or 4 file on a platform that
/// maps files (`super::mapped`), the file is mapped, up to the end of its
/// last chunk that checks out: the arrays are borrowed from the map, the
/// small sections (offsets, ids, the ids' lookup table) are checked now, and
/// the edge arrays' checksums are deferred to [`Stored::unverified`]. The
/// ids are not hashed: a version 4 file's lookup table is used as it lies,
/// and an older file's is built when a query first looks an id up
/// (`IdMap::from_stored_ids`). Otherwise the main part is read
/// whole, in parallel, and every checksum is checked before any array is
/// used. The chunks are only listed; the in-adjacency's is read when the
/// topology is first asked for it.
pub fn read_file(path: &Path, key: &IndexKey) -> Result<Stored, DecodeError> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let head = read_head(&mut file, len, key)?;
    let (chunks, end) = list_chunks(&mut file, head.main_len, len);
    let mapped = head.version != VERSION_2 && super::mapped::MAPS;
    let data = match mapped {
        true => super::mapped::map(&file, end)?,
        false => super::mapped::read_whole(&file, head.main_len)?,
    };
    let (topology, ids, main, dropped, deferred) = parse(&head, &data, mapped)?;
    let unverified = mapped.then(|| Arc::new(Unverified::new(data.clone(), deferred)));
    let topology = match (
        chunks.iter().find(|c| c.kind == CHUNK_INCOMING),
        &unverified,
    ) {
        (Some(chunk), Some(checks)) => {
            // Borrowed from the map, its data CRC deferred as the edge
            // arrays' are.
            let (chunk, checks) = (chunk.clone(), Arc::clone(checks));
            let (n_nodes, n_edges) = (topology.n_nodes(), topology.n_edges());
            topology.with_checksummed_incoming_loader(Box::new(move || {
                let incoming = mapped_incoming(&data, &chunk, n_nodes, n_edges).ok()?;
                checks
                    .defer(Region {
                        what: "in-adjacency",
                        range: usize::try_from(chunk.start).ok()?
                            ..usize::try_from(chunk.data_end).ok()?,
                        crc: chunk.data_crc,
                    })
                    .ok()?;
                count_incoming_read();
                Some(incoming)
            }))
        }
        (Some(chunk), None) => {
            let (path, chunk) = (path.to_path_buf(), chunk.clone());
            let (n_nodes, n_edges) = (topology.n_nodes(), topology.n_edges());
            topology.with_incoming_loader(Box::new(move || {
                let incoming = read_incoming(&path, main, &chunk, n_nodes, n_edges).ok()?;
                count_incoming_read();
                Some(incoming)
            }))
        }
        (None, _) => topology,
    };
    Ok(Stored {
        topology,
        ids,
        dropped,
        main,
        chunks,
        version: head.version,
        unverified,
    })
}

/// Read the main part of the index in `bytes`, which must be stored under
/// `key`, checking every checksum. Chunks are not read.
pub fn decode(bytes: &[u8], key: &IndexKey) -> Result<(Topology, IdMap), DecodeError> {
    decode_main(bytes, key).map(|(topology, ids, _, _)| (topology, ids))
}

/// [`decode`], with the main part's identity and the rows left out.
fn decode_main(
    bytes: &[u8],
    key: &IndexKey,
) -> Result<(Topology, IdMap, MainPart, Dropped), DecodeError> {
    let head = read_head(&mut io::Cursor::new(bytes), bytes.len() as u64, key)?;
    // Copied to memory aligned for 64-bit values, as a read file is.
    let main = &bytes[..head.main_len as usize];
    let mut words = vec![0u64; main.len().div_ceil(8)];
    for (word, chunk) in words.iter_mut().zip(main.chunks(8)) {
        let mut le = [0u8; 8];
        le[..chunk.len()].copy_from_slice(chunk);
        *word = u64::from_ne_bytes(le);
    }
    let data = Buffer::from_vec(words).slice_with_length(0, main.len());
    parse(&head, &data, false)
        .map(|(topology, ids, main, dropped, _)| (topology, ids, main, dropped))
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
            .checked_mul(CHUNK_ENTRY_LEN)?
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
    for entry in table.chunks_exact(CHUNK_ENTRY_LEN as usize) {
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
    let malformed = || DecodeError::from("the in-adjacency is malformed");
    let offsets = match take(IN_OFFSETS)? {
        Section::U64(offsets) if offsets.len() == n_nodes + 1 => offsets,
        _ => return Err(malformed()),
    };
    let edge_ids = match take(IN_EDGE_IDS) {
        Ok(Section::U32(ids)) if ids.len() == n_edges => EdgeIds::from(ids),
        Err(_) => EdgeIds::identity(n_edges),
        Ok(_) => return Err(malformed()),
    };
    let (targets, packed) = match take(IN_TARGETS) {
        Ok(Section::U32(targets)) if targets.len() == n_edges => (targets, None),
        Ok(_) => return Err(malformed()),
        Err(_) => match (take(IN_BYTE_OFFSETS)?, take(IN_BYTES)?) {
            (Section::U64(byte_offsets), Section::Bytes(bytes))
                if byte_offsets.len() == n_nodes + 1 =>
            {
                let packed = Packed {
                    byte_offsets: byte_offsets.into(),
                    bytes: bytes.into(),
                };
                (Vec::new(), Some(packed))
            }
            _ => return Err(malformed()),
        },
    };
    Ok(Adjacency {
        offsets: offsets.into(),
        targets: targets.into(),
        edge_ids,
        packed,
    })
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
            (OUT_OFFSETS | IN_OFFSETS | OUT_BYTE_OFFSETS | IN_BYTE_OFFSETS, 8) => {
                Section::U64(self.values(n, u64::from_le_bytes)?)
            }
            (OUT_TARGETS | OUT_EDGE_IDS | IN_TARGETS | IN_EDGE_IDS, 4) => {
                Section::U32(self.values(n, u32::from_le_bytes)?)
            }
            (IDS_INT64, 8) => Section::I64(self.values(n, i64::from_le_bytes)?),
            (WEIGHTS_F64, 8) => Section::F64(self.values(n, f64::from_le_bytes)?),
            (IDS_STR_OFFSETS, 4) => Section::I32(self.values(n, i32::from_le_bytes)?),
            (IDS_STR_BYTES | KEPT_ROWS | OUT_BYTES | IN_BYTES, 1) => Section::Bytes(self.bytes(n)?),
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

/// A section table entry, as read.
#[derive(Debug, Clone, Copy)]
struct Entry {
    kind: u32,
    width: u64,
    offset: u64,
    count: u64,
    /// The section's CRC (zero in version 2, which has none).
    crc: u32,
}

impl Entry {
    fn range(&self) -> Range<usize> {
        // Within the main part, which `read_head` checked fits in memory.
        self.offset as usize..(self.offset + self.width * self.count) as usize
    }
}

/// The main part's header, key and table, read and checked.
struct Head {
    version: u32,
    flags: u32,
    n_nodes: usize,
    n_edges: usize,
    n_dropped: u64,
    main_len: u64,
    /// The end of the section table: where the sections may start.
    table_end: u64,
    /// By offset; they do not overlap.
    sections: Vec<Entry>,
    /// The trailer's CRC.
    crc: u32,
}

/// Read and check the header, key, section table and trailer of the index
/// file in `file`, `len` bytes, which must be stored under `key`. The
/// trailer CRC (over the header, key and table) is checked here; version 2's
/// covers the sections, so [`parse`] checks it.
fn read_head<R: Read + Seek>(file: &mut R, len: u64, key: &IndexKey) -> Result<Head, DecodeError> {
    let read = |file: &mut R, at: u64, n: u64| -> Result<Vec<u8>, DecodeError> {
        file.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
        let mut buf = vec![0; usize::try_from(n).map_err(|e| e.to_string())?];
        file.read_exact(&mut buf).map_err(|e| match e.kind() {
            io::ErrorKind::UnexpectedEof => "the file is truncated".to_string(),
            _ => e.to_string(),
        })?;
        Ok(buf)
    };
    if len < HEADER_LEN + TRAILER_LEN {
        return Err(format!("{len} bytes is too short for an index file"));
    }
    let header = read(file, 0, HEADER_LEN)?;
    if header[..8] != MAGIC {
        return Err("not an Ursa index file".into());
    }
    let version = u32_at(&header, 8);
    if ![VERSION, VERSION_5, VERSION_4, VERSION_3, VERSION_2].contains(&version) {
        return Err(format!(
            "index format version {version}; this build reads {VERSION_2} to {VERSION}"
        ));
    }
    let flags = u32_at(&header, 12);
    let n_nodes = usize::try_from(u64_at(&header, 16)).map_err(|e| e.to_string())?;
    let n_edges = usize::try_from(u64_at(&header, 24)).map_err(|e| e.to_string())?;
    let key_len = u64_at(&header, 32);
    let n_sections = u32_at(&header, 40) as u64;
    let main_len = u64_at(&header, 48);
    let n_dropped = u64_at(&header, 56);
    if main_len > len || main_len < HEADER_LEN + TRAILER_LEN || usize::try_from(main_len).is_err() {
        return Err("the file is truncated".into());
    }
    let body = main_len - TRAILER_LEN;
    let key_end = HEADER_LEN
        .checked_add(key_len)
        .filter(|&end| end <= body)
        .ok_or("the key runs past the end of the file")?;
    let table = align(key_end, 8);
    let entry_len = match version {
        VERSION_2 => CHUNK_ENTRY_LEN,
        _ => SECTION_ENTRY_LEN,
    };
    let table_end = n_sections
        .checked_mul(entry_len)
        .and_then(|n| n.checked_add(table))
        .filter(|&end| end <= body)
        .ok_or("the section table runs past the end of the file")?;
    let rest = read(file, HEADER_LEN, table_end - HEADER_LEN)?;
    let trailer = read(file, body, TRAILER_LEN)?;
    if trailer[12..] != END_MAGIC || u64_at(&trailer, 0) != main_len {
        return Err("the file is truncated".into());
    }
    let crc = u32_at(&trailer, 8);
    if version != VERSION_2 {
        let mut meta = crc32fast::Hasher::new();
        meta.update(&header);
        meta.update(&rest);
        if meta.finalize() != crc {
            return Err("the checksum does not match".into());
        }
        if rest[(key_end - HEADER_LEN) as usize..(table - HEADER_LEN) as usize]
            .iter()
            .any(|&b| b != 0)
        {
            return Err("the padding is not zero".into());
        }
    }
    let stored: Value = serde_json::from_slice(&rest[..key_len as usize])
        .map_err(|e| format!("the key is not JSON: {e}"))?;
    if IndexKey::from_json(&stored).as_ref() != Some(key) {
        return Err("the file holds an index for another key".into());
    }

    let entries = &rest[(table - HEADER_LEN) as usize..];
    let mut sections = Vec::with_capacity(n_sections as usize);
    for entry in entries.chunks_exact(entry_len as usize) {
        let (kind, width) = (u32_at(entry, 0), u32_at(entry, 4) as u64);
        let (offset, count) = (u64_at(entry, 8), u64_at(entry, 16));
        let crc = match version {
            VERSION_2 => 0,
            _ => u32_at(entry, 24),
        };
        width
            .checked_mul(count)
            .and_then(|n| n.checked_add(offset))
            .filter(|&end| offset >= table_end && end <= body)
            .ok_or("a section runs past the end of the file")?;
        sections.push(Entry {
            kind,
            width,
            offset,
            count,
            crc,
        });
    }
    sections.sort_by_key(|e| e.offset);
    if sections
        .windows(2)
        .any(|w| w[0].offset + w[0].width * w[0].count > w[1].offset)
    {
        return Err("the sections overlap".into());
    }
    Ok(Head {
        version,
        flags,
        n_nodes,
        n_edges,
        n_dropped,
        main_len,
        table_end,
        sections,
        crc,
    })
}

/// The sections whose CRCs a mapped file defers (see `super::mapped`): the
/// `O(edges)` arrays, which only a whole-graph algorithm reads whole.
fn deferred_kind(kind: u32) -> Option<&'static str> {
    match kind {
        OUT_TARGETS => Some("targets"),
        OUT_BYTES => Some("compressed targets"),
        OUT_EDGE_IDS => Some("edge ids"),
        _ => None,
    }
}

/// The `count` values of type `T` at byte `offset` of `data`, borrowed.
fn scalars<T: ArrowNativeType>(data: &Buffer, entry: &Entry) -> ScalarBuffer<T> {
    let size = std::mem::size_of::<T>();
    debug_assert_eq!(entry.width as usize, size);
    // `data` starts 8-aligned (a map, or a vector of `u64`s) and every
    // section at a multiple of 64, so the values are aligned.
    ScalarBuffer::new(
        data.clone(),
        entry.offset as usize / size,
        entry.count as usize,
    )
}

/// What [`parse`] makes of a main part.
type Parsed = (Topology, IdMap, MainPart, Dropped, Vec<Region>);

/// The index in `data`, the main part `head` describes (and maybe more),
/// borrowing its arrays. A `mapped` file's edge arrays are not read: their
/// CRCs come back to be checked later. Every other checksum is checked here.
fn parse(head: &Head, data: &Buffer, mapped: bool) -> Result<Parsed, DecodeError> {
    let main_len = head.main_len as usize;
    if data.len() < main_len {
        return Err("the file is truncated".into());
    }
    let bytes = &data.as_slice()[..main_len];
    let body = main_len - TRAILER_LEN as usize;
    let mut deferred = Vec::new();
    if head.version == VERSION_2 {
        if super::mapped::crc(&bytes[..body]) != head.crc {
            return Err("the checksum does not match".into());
        }
    } else {
        // Padding is zero, and each section's CRC matches (or is deferred).
        let mut at = head.table_end as usize;
        for entry in &head.sections {
            let range = entry.range();
            if bytes[at..range.start].iter().any(|&b| b != 0) {
                return Err("the padding is not zero".into());
            }
            at = range.end;
            match deferred_kind(entry.kind).filter(|_| mapped) {
                Some(what) => deferred.push(Region {
                    what,
                    range,
                    crc: entry.crc,
                }),
                None => {
                    if super::mapped::crc(&bytes[range]) != entry.crc {
                        return Err(format!(
                            "the checksum of section {} does not match",
                            entry.kind
                        ));
                    }
                }
            }
        }
        if bytes[at..body].iter().any(|&b| b != 0) {
            return Err("the padding is not zero".into());
        }
    }

    let (n_nodes, n_edges) = (head.n_nodes, head.n_edges);
    let take = |kind: u32, width: u64, count: Option<usize>| -> Result<Entry, DecodeError> {
        let mut found = head.sections.iter().filter(|e| e.kind == kind);
        let entry = found
            .next()
            .ok_or_else(|| format!("section {kind} is missing"))?;
        if found.next().is_some() || entry.width != width {
            return Err(format!("section {kind} is malformed"));
        }
        if count.is_some_and(|count| entry.count != count as u64) {
            return Err(format!("section {kind} has the wrong length"));
        }
        Ok(*entry)
    };
    let has = |kind: u32| head.sections.iter().any(|e| e.kind == kind);
    // Only version 6 may leave out identity edge ids, or be compressed.
    if head.version != VERSION && (!has(OUT_EDGE_IDS) || has(OUT_BYTES)) {
        return Err(format!(
            "the out-adjacency is not one of version {}",
            head.version
        ));
    }
    let offsets = scalars(data, &take(OUT_OFFSETS, 8, Some(n_nodes + 1))?);
    let (targets, packed) = match has(OUT_BYTES) {
        false => (scalars(data, &take(OUT_TARGETS, 4, Some(n_edges))?), None),
        true => {
            if has(OUT_TARGETS) {
                return Err("the out-adjacency has both targets and compressed segments".into());
            }
            let byte_offsets = scalars(data, &take(OUT_BYTE_OFFSETS, 8, Some(n_nodes + 1))?);
            let bytes = scalars::<u8>(data, &take(OUT_BYTES, 1, None)?);
            (
                ScalarBuffer::from(Vec::<u32>::new()),
                Some(Packed {
                    byte_offsets,
                    bytes,
                }),
            )
        }
    };
    let edge_ids = match has(OUT_EDGE_IDS) {
        true => scalars(data, &take(OUT_EDGE_IDS, 4, Some(n_edges))?).into(),
        false => EdgeIds::identity(n_edges),
    };
    let out = Adjacency {
        offsets,
        targets,
        edge_ids,
        packed,
    };
    let topology = match head.version {
        // No section CRCs to vouch for the arrays: check them in full.
        VERSION_2 => Topology::from_parts(n_nodes, out, None)?,
        _ => Topology::from_checksummed_parts(n_nodes, out)?,
    };
    // The nodes' original ids, if the build relabelled them (#172): only a
    // version 5 file has them, and only when it was relabelled.
    let topology = match head.sections.iter().any(|e| e.kind == ORIGINAL_IDS) {
        true if head.version >= VERSION_5 => {
            let original = scalars::<u32>(data, &take(ORIGINAL_IDS, 4, Some(n_nodes))?);
            topology.with_original_ids(original)?
        }
        true => return Err(format!("section {ORIGINAL_IDS} is not in this version")),
        false => topology,
    };

    let ids: Arc<dyn Array> = if head.flags & FLAG_STRING_IDS == 0 {
        let values = scalars::<i64>(data, &take(IDS_INT64, 8, Some(n_nodes))?);
        Arc::new(Int64Array::new(values, None))
    } else {
        let offsets = scalars::<i32>(data, &take(IDS_STR_OFFSETS, 4, Some(n_nodes + 1))?);
        let strings = take(IDS_STR_BYTES, 1, None)?;
        if offsets[0] != 0
            || offsets.windows(2).any(|w| w[0] > w[1])
            || offsets[n_nodes] as u64 != strings.count
        {
            return Err("the string ids' offsets are malformed".into());
        }
        let values = data.slice_with_length(strings.offset as usize, strings.count as usize);
        if !super::mapped::are_strings(&values, &offsets) {
            return Err("the string ids are malformed: they are not UTF-8".into());
        }
        // SAFETY: the offsets start at 0, never decrease and end at
        // `values.len()` (checked above), and they bound valid UTF-8 strings
        // (`are_strings`); there are no nulls.
        Arc::new(unsafe { StringArray::new_unchecked(OffsetBuffer::new(offsets), values, None) })
    };
    // The ids' lookup table, as it lies in the file; an older file has none,
    // and the id map builds it if a query looks an id up.
    let table = match head.version {
        VERSION | VERSION_5 | VERSION_4 => {
            let slots = usize::try_from(lookup_table_len(n_nodes)).map_err(|e| e.to_string())?;
            Some(scalars::<u32>(data, &take(ID_TABLE, 4, Some(slots))?))
        }
        _ => None,
    };
    let ids = IdMap::from_stored_ids(ids, table).map_err(|e| e.to_string())?;
    if ids.len() != n_nodes {
        return Err("the id map does not match the topology".into());
    }
    let dropped = match head.n_dropped {
        0 => Dropped::default(),
        n_dropped => {
            let rows = usize::try_from(n_dropped)
                .ok()
                .and_then(|d| d.checked_add(n_edges))
                .ok_or("the kept rows are malformed")?;
            let bits = take(KEPT_ROWS, 1, Some(rows.div_ceil(8)))?;
            let bits = data.slice_with_length(bits.offset as usize, bits.count as usize);
            let kept = BooleanBuffer::new(bits, 0, rows);
            if kept.count_set_bits() != n_edges {
                return Err("the kept rows do not match the edges".into());
            }
            Dropped {
                n_dropped,
                kept: Some(kept),
            }
        }
    };
    let main = MainPart {
        len: head.main_len,
        crc: head.crc,
    };
    Ok((topology, ids, main, dropped, deferred))
}

/// The in-adjacency in `chunk` of the mapped file `data`, borrowed, for a
/// topology of `n_nodes` nodes and `n_edges` edges. Its data CRC is not
/// checked here.
fn mapped_incoming(
    data: &Buffer,
    chunk: &ChunkInfo,
    n_nodes: usize,
    n_edges: usize,
) -> Result<Adjacency, DecodeError> {
    if (data.len() as u64) < chunk.end {
        return Err("the file is shorter than the chunk".into());
    }
    let entry = |kind: u32, width: u64, count: usize| -> Result<Entry, DecodeError> {
        let mut found = chunk.sections.iter().filter(|s| s.0 == kind);
        match (found.next(), found.next()) {
            (Some(&(kind, w, offset, c)), None) if w == width && c == count as u64 => Ok(Entry {
                kind,
                width,
                offset,
                count: c,
                crc: 0,
            }),
            _ => Err("the in-adjacency is malformed".into()),
        }
    };
    let has = |kind: u32| chunk.sections.iter().any(|s| s.0 == kind);
    let (targets, packed) = match has(IN_BYTES) {
        false => (scalars(data, &entry(IN_TARGETS, 4, n_edges)?), None),
        true => {
            let bytes = chunk
                .sections
                .iter()
                .find(|s| s.0 == IN_BYTES && s.1 == 1)
                .ok_or("the in-adjacency is malformed")?;
            let bytes = Entry {
                kind: IN_BYTES,
                width: 1,
                offset: bytes.2,
                count: bytes.3,
                crc: 0,
            };
            (
                ScalarBuffer::from(Vec::<u32>::new()),
                Some(Packed {
                    byte_offsets: scalars(data, &entry(IN_BYTE_OFFSETS, 8, n_nodes + 1)?),
                    bytes: scalars::<u8>(data, &bytes),
                }),
            )
        }
    };
    let edge_ids = match has(IN_EDGE_IDS) {
        true => scalars(data, &entry(IN_EDGE_IDS, 4, n_edges)?).into(),
        false => EdgeIds::identity(n_edges),
    };
    Ok(Adjacency {
        offsets: scalars(data, &entry(IN_OFFSETS, 8, n_nodes + 1)?),
        targets,
        edge_ids,
        packed,
    })
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
    use datafusion::arrow::datatypes::DataType;

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

        // Sections are 64-byte aligned, each with its CRC in the table.
        let n_sections = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        let table = align(HEADER_LEN + u64_at(&bytes, 32), 8) as usize;
        for i in 0..n_sections {
            let entry = table + 32 * i;
            let (width, offset) = (
                u32_at(&bytes, entry + 4) as usize,
                u64_at(&bytes, entry + 8),
            );
            let count = u64_at(&bytes, entry + 16) as usize;
            assert_eq!(offset % 64, 0);
            let section = &bytes[offset as usize..offset as usize + width * count];
            assert_eq!(u32_at(&bytes, entry + 24), crc32fast::hash(section));
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
        let (back, _, _, dropped) = decode_main(&bytes, &key()).unwrap();
        same(&topology, &back);
        assert_eq!(dropped.n_dropped, 2);
        assert_eq!(dropped.kept, Some(kept));
        // A file with nothing left out has no such section, as before #311.
        let plain = encode(&key(), &topology, &ids);
        let (_, _, _, none) = decode_main(&plain, &key()).unwrap();
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
        let reads = thread_incoming_reads();
        let stored = read_file(&path, &key()).unwrap();
        assert!(stored.chunks.is_empty());
        assert_eq!(
            stored.topology.incoming().targets,
            topology.incoming().targets
        );
        assert_eq!(thread_incoming_reads(), reads);

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
        assert!(thread_incoming_reads() > reads);
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

    /// The section table entry of `kind` in the main part `bytes` (version
    /// 3 or 4): its offset and length in bytes.
    fn section_of(bytes: &[u8], kind: u32) -> (usize, usize) {
        let n_sections = u32_at(bytes, 40) as usize;
        let table = align(HEADER_LEN + u64_at(bytes, 32), 8) as usize;
        (0..n_sections)
            .map(|i| table + 32 * i)
            .find(|&entry| u32_at(bytes, entry) == kind)
            .map(|entry| {
                let width = u32_at(bytes, entry + 4) as usize;
                let count = u64_at(bytes, entry + 16) as usize;
                (u64_at(bytes, entry + 8) as usize, width * count)
            })
            .unwrap()
    }

    fn a_graph() -> (Arc<Topology>, Arc<IdMap>) {
        let ints = |v: &[i64]| Arc::new(Int64Array::from(v.to_vec())) as ArrayRef;
        built(
            ints(&[10, 20, 10, 30, 20, 40, 40, 50]),
            ints(&[20, 30, 30, 10, 40, 10, 50, 10]),
        )
    }

    #[test]
    fn a_version_2_file_from_0_10_is_read_whole_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        let (topology, ids) = a_graph();
        let encoded = Encoded::new(&key(), &topology, &ids)
            .unwrap()
            .as_version_2();
        let mut v2 = Vec::new();
        let main = encoded.write_to(&mut v2).unwrap();
        assert_eq!(u32_at(&v2, 8), VERSION_2);
        assert_eq!(main.crc, crc32fast::hash(&v2[..v2.len() - 16]));
        std::fs::write(&path, &v2).unwrap();
        let stored = read_file(&path, &key()).unwrap();
        assert_eq!(stored.version, VERSION_2);
        // Read whole, every byte checked: nothing is left to check.
        assert!(stored.unverified.is_none());
        assert_eq!(stored.main, main);
        same(&topology, &stored.topology);
        assert_eq!(
            stored.ids.user_id_array().as_ref(),
            ids.user_id_array().as_ref()
        );
        // With an in-adjacency appended, as 0.10 appended it.
        let incoming = topology.incoming().clone();
        append_chunk(&path, main, CHUNK_INCOMING, b"", |at| {
            EncodedChunk::incoming(&incoming, at)
        })
        .unwrap()
        .unwrap();
        let stored = read_file(&path, &key()).unwrap();
        assert_eq!(stored.topology.incoming().targets, incoming.targets);

        // A damaged or truncated version 2 file is an error, never a crash.
        for at in [9, 70, v2.len() / 2, v2.len() - 20] {
            let mut flipped = v2.clone();
            flipped[at] ^= 0x40;
            std::fs::write(&path, &flipped).unwrap();
            assert!(read_file(&path, &key()).is_err(), "flipped at {at}");
        }
        for cut in [0, 10, 64, v2.len() / 2, v2.len() - 1] {
            std::fs::write(&path, &v2[..cut]).unwrap();
            assert!(read_file(&path, &key()).is_err(), "cut at {cut}");
        }
    }

    /// Graphs with integer and with string ids, each id on several edges.
    fn graphs() -> [(Arc<Topology>, Arc<IdMap>); 2] {
        let n = 300i64;
        let (src, dst): (Vec<i64>, Vec<i64>) = (0..n)
            .map(|i| (i * 7 - 50, ((i * 13) % n) * 7 - 50))
            .unzip();
        let name = |v: &[i64]| -> ArrayRef {
            Arc::new(StringArray::from(
                v.iter()
                    .map(|i| format!("did:plc:{i:024}"))
                    .collect::<Vec<_>>(),
            ))
        };
        [
            built(name(&src), name(&dst)),
            built(
                Arc::new(Int64Array::from(src)),
                Arc::new(Int64Array::from(dst)),
            ),
        ]
    }

    /// Every id of `ids` is found at its own node in `stored`, and one that
    /// is not there is not found.
    fn finds_every_id(stored: &IdMap, ids: &IdMap) {
        let column = ids.user_id_array();
        assert_eq!(stored.user_id_array().as_ref(), column.as_ref());
        let expected: Vec<Option<u32>> = (0..ids.len() as u32).map(Some).collect();
        assert_eq!(stored.dense_from_array(column.as_ref()).unwrap(), expected);
        let missing: ArrayRef = match column.data_type() {
            DataType::Int64 => Arc::new(Int64Array::from(vec![i64::MAX])),
            _ => Arc::new(StringArray::from(vec!["nobody"])),
        };
        assert_eq!(stored.dense_from_array(&missing).unwrap(), vec![None]);
    }

    /// #421: the ids' lookup table is a section of the file, and a loaded
    /// index finds ids through it as it lies there.
    #[test]
    fn the_id_lookup_table_is_stored_and_used_as_it_lies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        for (topology, ids) in graphs() {
            let bytes = encode(&key(), &topology, &ids);
            assert_eq!(u32_at(&bytes, 8), VERSION);
            let (at, len) = section_of(&bytes, ID_TABLE);
            let table = ids.lookup_table().unwrap();
            assert_eq!(len, 4 * table.len());
            assert_eq!(len as u64, 4 * lookup_table_len(ids.len()));
            let stored_table: Vec<u32> = read(&bytes[at..at + len], u32::from_le_bytes).collect();
            assert_eq!(stored_table, table.as_ref());

            std::fs::write(&path, &bytes).unwrap();
            let stored = read_file(&path, &key()).unwrap();
            assert_eq!(stored.version, VERSION);
            finds_every_id(&stored.ids, &ids);
            // The loaded map's table is the file's: borrowed from the map
            // (or the read buffer), never built.
            let loaded = stored.ids.lookup_table().unwrap();
            assert_eq!(loaded, table);
            if super::super::mapped::MAPS {
                let mapped = stored.unverified.as_ref().unwrap();
                assert!(mapped.holds(loaded.inner().as_ptr()));
                // And so are the ids themselves.
                let column = stored.ids.user_id_array().to_data();
                assert!(mapped.holds(column.buffers()[0].as_ptr()));
            }
            // Stored again from the loaded index: the same file.
            assert_eq!(encode(&key(), &stored.topology, &stored.ids), bytes);
            // A table that is not the ids' cannot be told from theirs by its
            // CRC alone, so the CRC is what vouches for it: a flipped bit is
            // an error when the file loads.
            let mut flipped = bytes.clone();
            flipped[at + len / 2] ^= 1;
            std::fs::write(&path, &flipped).unwrap();
            assert!(read_file(&path, &key()).is_err());
        }
    }

    /// #421: a file 0.11 wrote has no lookup table. It is mapped all the
    /// same, and its ids are found through a table built on demand.
    #[test]
    fn a_version_3_file_from_0_11_is_read_and_its_lookup_table_built_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        for (topology, ids) in graphs() {
            let v4 = encode(&key(), &topology, &ids);
            let encoded = Encoded::new(&key(), &topology, &ids)
                .unwrap()
                .as_version_3();
            let mut v3 = Vec::new();
            let main = encoded.write_to(&mut v3).unwrap();
            assert_eq!(v3.len() as u64, encoded.file_size());
            assert_eq!(u32_at(&v3, 8), VERSION_3);
            assert!(v3.len() < v4.len());
            let n_sections = u32_at(&v3, 40) as usize;
            let table = align(HEADER_LEN + u64_at(&v3, 32), 8) as usize;
            assert!((0..n_sections).all(|i| u32_at(&v3, table + 32 * i) != ID_TABLE));

            std::fs::write(&path, &v3).unwrap();
            let stored = read_file(&path, &key()).unwrap();
            assert_eq!(stored.version, VERSION_3);
            assert_eq!(stored.main, main);
            assert_eq!(stored.unverified.is_some(), super::super::mapped::MAPS);
            same(&topology, &stored.topology);
            finds_every_id(&stored.ids, &ids);
            // Stored again, it is the version 4 file.
            assert_eq!(encode(&key(), &stored.topology, &stored.ids), v4);

            // With an in-adjacency appended, as 0.11 appended it.
            let incoming = topology.incoming().clone();
            append_chunk(&path, main, CHUNK_INCOMING, b"", |at| {
                EncodedChunk::incoming(&incoming, at)
            })
            .unwrap()
            .unwrap();
            let stored = read_file(&path, &key()).unwrap();
            assert_eq!(stored.topology.incoming().targets, incoming.targets);
        }
    }

    /// An id stored twice, which no writer stores: a version 4 file cannot
    /// be written with one, and an older file with one is an error when an
    /// id is first looked up.
    #[test]
    fn an_id_stored_twice_is_an_error() {
        let (topology, _) = a_graph();
        let n = topology.n_nodes();
        let twice: ArrayRef = Arc::new(Int64Array::from(vec![7i64; n]));
        let ids = IdMap::from_stored_ids(Arc::clone(&twice), None).unwrap();
        let mut out = Vec::new();
        let err = Encoded::new(&key(), &topology, &ids)
            .unwrap()
            .write_to(&mut out)
            .unwrap_err();
        assert!(err.to_string().contains("twice"), "{err}");
        let mut v3 = Vec::new();
        Encoded::new(&key(), &topology, &ids)
            .unwrap()
            .as_version_3()
            .write_to(&mut v3)
            .unwrap();
        let (_, back) = decode(&v3, &key()).unwrap();
        assert_eq!(back.len(), n);
        assert!(back.dense_from_array(&twice).is_err());
    }

    #[test]
    fn a_truncated_or_damaged_version_3_file_is_an_error_never_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        let (topology, ids) = a_graph();
        let main = write_main(&path, &topology, &ids);
        let incoming = topology.incoming().clone();
        append_chunk(&path, main, CHUNK_INCOMING, b"", |at| {
            EncodedChunk::incoming(&incoming, at)
        })
        .unwrap();
        let good = std::fs::read(&path).unwrap();
        let main_len = main.len as usize;
        // Cut anywhere in the main part: an error (a mapped reader never
        // maps past the end of the file).
        for cut in (0..main_len).step_by(7).chain([main_len - 1]) {
            std::fs::write(&path, &good[..cut]).unwrap();
            assert!(read_file(&path, &key()).is_err(), "cut at {cut}");
        }
        // Cut in the chunk: the main part reads, the chunk is not listed.
        std::fs::write(&path, &good[..good.len() - 1]).unwrap();
        let stored = read_file(&path, &key()).unwrap();
        assert!(stored.chunks.is_empty());
        assert_eq!(stored.topology.incoming().targets, incoming.targets);
        // A flipped byte anywhere in the main part but the edge arrays (the
        // header, key, table, offsets, ids, padding, trailer) is an error at
        // once.
        let (targets, t_len) = section_of(&good, OUT_TARGETS);
        let (edge_ids, e_len) = section_of(&good, OUT_EDGE_IDS);
        let edge_arrays = |at: usize| {
            (targets..targets + t_len).contains(&at) || (edge_ids..edge_ids + e_len).contains(&at)
        };
        for at in (0..main_len).filter(|&at| !edge_arrays(at)) {
            let mut flipped = good.clone();
            flipped[at] ^= 0x10;
            std::fs::write(&path, &flipped).unwrap();
            assert!(read_file(&path, &key()).is_err(), "flipped at {at}");
        }
    }

    #[test]
    fn the_edge_arrays_checksums_are_checked_before_a_whole_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        let (topology, ids) = a_graph();
        let main = write_main(&path, &topology, &ids);
        let good = std::fs::read(&path).unwrap();
        let stored = read_file(&path, &key()).unwrap();
        same(&topology, &stored.topology);
        if let Some(checks) = &stored.unverified {
            checks.verify().unwrap();
        }

        for kind in [OUT_TARGETS, OUT_EDGE_IDS] {
            let (at, _) = section_of(&good, kind);
            let mut flipped = good.clone();
            // Still in range, so a point traversal could not tell.
            flipped[at] ^= 0x01;
            std::fs::write(&path, &flipped).unwrap();
            // Read whole (as on Windows, and by `decode`): an error at once.
            assert!(decode(&flipped, &key()).is_err());
            match read_file(&path, &key()) {
                // Mapped: the damage is found before a whole-graph read.
                Ok(stored) => {
                    let checks = stored.unverified.expect("a mapped file defers");
                    assert!(!checks.is_damaged());
                    let why = checks.verify().unwrap_err();
                    assert!(why.contains("does not match"), "{why}");
                    assert!(checks.is_damaged());
                }
                // Read whole where files are not mapped: an error at once.
                Err(e) if !super::super::mapped::MAPS => {
                    assert!(e.contains("does not match"), "{e}")
                }
                Err(e) => panic!("{e}"),
            }
        }

        // A damaged in-adjacency, read after the check: the transpose is
        // built instead, the same.
        std::fs::write(&path, &good).unwrap();
        let incoming = topology.incoming().clone();
        append_chunk(&path, main, CHUNK_INCOMING, b"", |at| {
            EncodedChunk::incoming(&incoming, at)
        })
        .unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let chunk = read_file(&path, &key()).unwrap().chunks[0].clone();
        let (_, _, edge_ids, _) = chunk.sections[2];
        bytes[edge_ids as usize] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let stored = read_file(&path, &key()).unwrap();
        if let Some(checks) = &stored.unverified {
            checks.verify().unwrap();
        }
        assert_eq!(stored.topology.incoming().targets, incoming.targets);
        assert_eq!(stored.topology.incoming().edge_ids, incoming.edge_ids);
    }

    #[test]
    fn a_mapped_index_survives_its_file_being_replaced_or_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        let (topology, ids) = a_graph();
        let main = write_main(&path, &topology, &ids);
        let incoming = topology.incoming().clone();
        append_chunk(&path, main, CHUNK_INCOMING, b"", |at| {
            EncodedChunk::incoming(&incoming, at)
        })
        .unwrap();
        let stored = read_file(&path, &key()).unwrap();

        // Another index replaces the file the way the store does: written to
        // a temporary file, then renamed over it.
        let ints = |v: &[i64]| Arc::new(Int64Array::from(v.to_vec())) as ArrayRef;
        let (other, other_ids) = built(ints(&[7, 8, 9]), ints(&[8, 9, 7]));
        let temp = dir.path().join("x.idx.tmp");
        write_main(&temp, &other, &other_ids);
        std::fs::rename(&temp, &path).unwrap();
        assert_eq!(read_file(&path, &key()).unwrap().topology.n_nodes(), 3);

        // The index loaded before reads as it was, the arrays whole, and its
        // checksums still match (a map keeps the replaced file's pages).
        same(&topology, &stored.topology);
        assert_eq!(stored.topology.incoming().targets, incoming.targets);
        if let Some(checks) = &stored.unverified {
            checks.verify().unwrap();
        }
        // And after the file is removed (an eviction).
        std::fs::remove_file(&path).unwrap();
        same(&topology, &stored.topology);
        assert_eq!(
            stored.ids.user_id_array().as_ref(),
            ids.user_id_array().as_ref()
        );
    }

    #[test]
    fn a_version_1_file_is_not_read() {
        let ints = |v: &[i64]| Arc::new(Int64Array::from(v.to_vec())) as ArrayRef;
        let (topology, ids) = built(ints(&[1, 2]), ints(&[2, 3]));
        let mut bytes = encode(&key(), &topology, &ids);
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            decode(&bytes, &key()).unwrap_err(),
            "index format version 1; this build reads 2 to 6"
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

    #[test]
    fn a_key_of_another_version_is_not_read() {
        let mut json = key().to_json();
        json["version"] = 1.into();
        assert_eq!(IndexKey::from_json(&json), None);
        json.as_object_mut().unwrap().remove("version");
        assert_eq!(IndexKey::from_json(&json), None);
    }

    /// A relabelled index (#172), with string and with integer ids: a
    /// topology built in in-degree order, with its id map renumbered to match.
    fn relabelled() -> [(Topology, IdMap); 2] {
        let n = 200i64;
        // Every node points at a few low ids, so in-degrees fall with the id
        // and the first-seen order is not the in-degree order.
        let (src, dst): (Vec<i64>, Vec<i64>) = (0..4 * n)
            .map(|i| ((i * 37) % n, ((i % n) * (i % n)) % (n / 3 + 1 + i % 7)))
            .unzip();
        let strings = |v: &[i64]| -> ArrayRef {
            Arc::new(StringArray::from(
                v.iter().map(|i| format!("user-{i}")).collect::<Vec<_>>(),
            ))
        };
        let ints = |v: &[i64]| -> ArrayRef { Arc::new(Int64Array::from(v.to_vec())) };
        let build = |column: &dyn Fn(&[i64]) -> ArrayRef| {
            let (map, s, d) =
                IdMap::from_edge_arrays(column(&src).as_ref(), column(&dst).as_ref()).unwrap();
            let topology = Topology::build_ordered(map.len(), s, d, ursa_core::NodeOrder::InDegree);
            let original = topology.original_ids().expect("relabelled").to_vec();
            let ids = map.reordered(&original);
            (topology, ids)
        };
        [build(&strings), build(&ints)]
    }

    /// #172: a relabelled index stores its nodes' original ids, and reads
    /// them back; its components keep their labels.
    #[test]
    fn a_relabelled_index_round_trips_with_its_original_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        for (topology, ids) in relabelled() {
            let bytes = encode(&key(), &topology, &ids);
            assert_eq!(u32_at(&bytes, 8), VERSION);
            let (at, len) = section_of(&bytes, ORIGINAL_IDS);
            let stored: Vec<u32> = read(&bytes[at..at + len], u32::from_le_bytes).collect();
            assert_eq!(stored, topology.original_ids().unwrap());
            std::fs::write(&path, &bytes).unwrap();
            let back = read_file(&path, &key()).unwrap();
            assert_eq!(back.version, VERSION);
            same(&topology, &back.topology);
            assert_eq!(back.topology.original_ids(), topology.original_ids());
            finds_every_id(&back.ids, &ids);
            assert_eq!(
                ursa_core::algo::connected_components_weak(&back.topology, None),
                ursa_core::algo::connected_components_weak(&topology, None)
            );
            // Stored again from the loaded index: the same file.
            assert_eq!(encode(&key(), &back.topology, &back.ids), bytes);
            // A damaged order is an error when the file loads.
            let mut flipped = bytes.clone();
            flipped[at] ^= 1;
            std::fs::write(&path, &flipped).unwrap();
            assert!(read_file(&path, &key()).is_err());
        }
        // An index that was not relabelled has no such section.
        for (topology, ids) in graphs() {
            let bytes = encode(&key(), &topology, &ids);
            let n_sections = u32_at(&bytes, 40) as usize;
            let table = align(HEADER_LEN + u64_at(&bytes, 32), 8) as usize;
            assert!((0..n_sections).all(|i| u32_at(&bytes, table + 32 * i) != ORIGINAL_IDS));
        }
    }

    /// #172: a file 0.12 wrote (version 4: no original ids, its nodes in
    /// first-seen order) still opens, keeps its numbering and finds its ids,
    /// and is written again as version 5.
    #[test]
    fn a_version_4_file_from_0_12_is_read_with_its_numbering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        for (topology, ids) in graphs() {
            let v5 = encode(&key(), &topology, &ids);
            let encoded = Encoded::new(&key(), &topology, &ids)
                .unwrap()
                .as_version_4();
            let mut v4 = Vec::new();
            encoded.write_to(&mut v4).unwrap();
            assert_eq!(u32_at(&v4, 8), VERSION_4);
            // Version 5 adds no section for an index that is not relabelled:
            // the files differ in the version and the header's CRC only.
            assert_eq!(v4.len(), v5.len());
            let differ: Vec<usize> = (0..v4.len()).filter(|&i| v4[i] != v5[i]).collect();
            assert!(
                differ.iter().all(|&i| i < 12 || i >= v4.len() - 8),
                "{differ:?}"
            );
            std::fs::write(&path, &v4).unwrap();
            let stored = read_file(&path, &key()).unwrap();
            assert_eq!(stored.version, VERSION_4);
            same(&topology, &stored.topology);
            assert!(stored.topology.original_ids().is_none());
            finds_every_id(&stored.ids, &ids);
            assert_eq!(
                stored.ids.lookup_table().unwrap(),
                ids.lookup_table().unwrap()
            );
            assert_eq!(encode(&key(), &stored.topology, &stored.ids), v5);
        }
    }

    /// The compressed layout (#173) is stored as version 6: byte offsets and
    /// bytes in place of the targets, in the main part and in the
    /// in-adjacency's chunk, and read back compressed, mapped or not.
    #[test]
    fn a_compressed_index_round_trips_as_version_6() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        let mut cases: Vec<(Topology, IdMap)> = relabelled().into();
        for (topology, ids) in graphs() {
            let out = topology.out().clone();
            let topology = Topology::from_parts(topology.n_nodes(), out, None).unwrap();
            cases.push((topology, (*ids).clone()));
        }
        for (topology, ids) in cases {
            // What the compressed topology decodes to.
            let out = topology.out().clone();
            let mut sorted = Topology::from_parts(topology.n_nodes(), out, None).unwrap();
            if let Some(original) = topology.original_ids() {
                sorted = sorted.with_original_ids(original.to_vec().into()).unwrap();
            }
            let sorted = sorted.with_sorted_segments();
            let compressed = topology.compressed(None);
            assert!(compressed.is_compressed());
            let bytes = encode(&key(), &compressed, &ids);
            assert_eq!(u32_at(&bytes, 8), VERSION);
            let (_, len) = section_of(&bytes, OUT_BYTES);
            assert_eq!(len, compressed.out().packed.as_ref().unwrap().bytes.len());
            let n_sections = u32_at(&bytes, 40) as usize;
            let table = align(HEADER_LEN + u64_at(&bytes, 32), 8) as usize;
            assert!((0..n_sections).all(|i| u32_at(&bytes, table + 32 * i) != OUT_TARGETS));

            // Read whole, and mapped (where the platform maps files).
            let (decoded, _) = decode(&bytes, &key()).unwrap();
            let main = write_main(&path, &compressed, &ids);
            let incoming = compressed.incoming().clone();
            assert!(incoming.is_packed());
            append_chunk(&path, main, CHUNK_INCOMING, b"", |at| {
                EncodedChunk::incoming(&incoming, at)
            })
            .unwrap()
            .unwrap();
            let stored = read_file(&path, &key()).unwrap();
            if let Some(checks) = &stored.unverified {
                checks.verify().unwrap();
            }
            for back in [&decoded, &stored.topology] {
                assert!(back.is_compressed());
                assert_eq!(back.out().unpacked().targets, sorted.out().targets);
                assert_eq!(back.out().edge_ids, sorted.out().edge_ids);
                assert_eq!(back.original_ids(), compressed.original_ids());
                assert_eq!(
                    ursa_core::algo::connected_components_weak(back, None),
                    ursa_core::algo::connected_components_weak(&sorted, None)
                );
                assert_eq!(
                    ursa_core::algo::bfs_distances(back, 0, ursa_core::Direction::Both),
                    ursa_core::algo::bfs_distances(&sorted, 0, ursa_core::Direction::Both)
                );
            }
            // The stored in-adjacency is read, compressed.
            let back_in = stored.topology.incoming();
            assert!(back_in.is_packed());
            assert_eq!(back_in.unpacked().targets, sorted.incoming().targets);
            assert_eq!(back_in.edge_ids, sorted.incoming().edge_ids);
            // Stored again from the loaded index: the same main part.
            assert_eq!(encode(&key(), &stored.topology, &stored.ids), bytes);
            // A damaged segment is found by its checksum.
            let (at, _) = section_of(&bytes, OUT_BYTES);
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x40;
            assert!(decode(&flipped, &key()).is_err());
            std::fs::write(&path, &flipped).unwrap();
            if let Ok(stored) = read_file(&path, &key()) {
                let checks = stored.unverified.expect("only a mapped file defers");
                assert!(checks.verify().is_err());
            }
        }
    }

    /// #155: version 6 leaves out edge ids that are the identity (rows
    /// grouped by source), and reads them back as the identity; version 5,
    /// which 0.13 wrote, stored them and is still read.
    #[test]
    fn identity_edge_ids_are_left_out_and_version_5_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        let ints = |v: &[i64]| Arc::new(Int64Array::from(v.to_vec())) as ArrayRef;
        // Rows grouped by source.
        let (topology, ids) = built(ints(&[1, 1, 2, 3, 3, 3]), ints(&[2, 3, 3, 1, 2, 4]));
        assert!(topology.out().edge_ids.is_identity());
        let v6 = encode(&key(), &topology, &ids);
        let n_sections = u32_at(&v6, 40) as usize;
        let table = align(HEADER_LEN + u64_at(&v6, 32), 8) as usize;
        assert!((0..n_sections).all(|i| u32_at(&v6, table + 32 * i) != OUT_EDGE_IDS));
        std::fs::write(&path, &v6).unwrap();
        let stored = read_file(&path, &key()).unwrap();
        assert!(stored.topology.out().edge_ids.is_identity());
        same(&topology, &stored.topology);

        let mut cases: Vec<(Arc<Topology>, Arc<IdMap>)> = graphs().into();
        cases.push((topology, ids));
        for (topology, ids) in cases {
            let encoded = Encoded::new(&key(), &topology, &ids)
                .unwrap()
                .as_version_5();
            let mut v5 = Vec::new();
            encoded.write_to(&mut v5).unwrap();
            assert_eq!(u32_at(&v5, 8), VERSION_5);
            section_of(&v5, OUT_EDGE_IDS);
            std::fs::write(&path, &v5).unwrap();
            let stored = read_file(&path, &key()).unwrap();
            assert_eq!(stored.version, VERSION_5);
            same(&topology, &stored.topology);
            finds_every_id(&stored.ids, &ids);
            // Written again as version 6.
            assert_eq!(
                encode(&key(), &stored.topology, &stored.ids),
                encode(&key(), &topology, &ids)
            );
        }
        // An older version may not leave them out.
        let mut v5 = v6.clone();
        v5[8..12].copy_from_slice(&VERSION_5.to_le_bytes());
        let end = table + 32 * n_sections;
        let crc = crc32fast::hash(&v5[..end]);
        let at = v5.len() - 8;
        v5[at..at + 4].copy_from_slice(&crc.to_le_bytes());
        let e = decode(&v5, &key()).unwrap_err();
        assert!(e.contains("is not one of version 5"), "{e}");
    }
}
