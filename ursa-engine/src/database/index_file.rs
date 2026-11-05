//! One stored graph index: `club.ursa.d/indexes/<hash>.idx`.
//!
//! A file holds one built index, the CSR topology and the id map, under the
//! full key it was built for (the edge plan's fingerprint and the identity of
//! every source file). Version 1, all integers little-endian:
//!
//! ```text
//! offset  size  field
//!      0     8  magic            "URSAIDX\0"
//!      8     4  version          1
//!     12     4  flags            bit 0: string ids; bit 1: in-adjacency stored
//!     16     8  n_nodes
//!     24     8  n_edges
//!     32     8  key_len          bytes of the key that follows the header
//!     40     4  n_sections
//!     44    20  reserved (zero)
//!     64     …  key              JSON: {"fingerprint": …, "files": [{…}, …]}
//!      …     …  section table    at the next multiple of 8; 24 bytes a section:
//!                                kind u32, element width u32, offset u64, count u64
//!      …     …  sections         each at a multiple of 64 from the file's start
//!      …    16  trailer          at the next multiple of 8: file length u64,
//!                                CRC-32 of every byte before the trailer u32,
//!                                "XDIU"
//! ```
//!
//! Sections, in this order: the out-adjacency's offsets (`u64`, `n_nodes + 1`),
//! targets (`u32`, `n_edges`) and edge ids (`u32`, `n_edges`); the
//! in-adjacency's three, if it was built when the index was stored; then the
//! ids, either `i64` values (`n_nodes`) or, for string ids, Arrow-style `i32`
//! offsets (`n_nodes + 1`, from 0) and UTF-8 bytes.
//!
//! **Why raw arrays, not Arrow IPC.** The arrays have different lengths and
//! no schema: an IPC stream would need one record batch per array, or padding
//! them into one table, and its metadata says nothing a section table does
//! not. Raw little-endian arrays at 64-byte-aligned offsets are what a later
//! memory-mapped reader wants (each section is a ready `&[u32]`/`&[u64]` on a
//! little-endian machine), need no dependency, and the string ids are already
//! in Arrow's layout, so they become a `StringArray` without a copy once
//! mapped. Version 1 reads the whole file into memory and copies the
//! sections out.
//!
//! **Corruption.** A file is read only if its length matches the trailer, the
//! CRC matches, the version is 1, the key matches the key asked for, every
//! section lies within the file, and the arrays form a valid topology and id
//! map (`ursa_core::Topology::from_parts`, `IdMap::from_user_ids`). Anything
//! else is an error, which the store treats as a miss.

use std::io::{self, Write};
use std::sync::Arc;

use datafusion::arrow::array::{Array, Int64Array, StringArray};
use datafusion::arrow::buffer::{Buffer, OffsetBuffer, ScalarBuffer};
use serde_json::{json, Value};
use ursa_core::{Adjacency, IdMap, Topology};

use crate::graph::FileIdentity;

/// The first bytes of every index file.
pub const MAGIC: [u8; 8] = *b"URSAIDX\0";
/// The last bytes of every index file.
const END_MAGIC: [u8; 4] = *b"XDIU";
/// The index file version this build reads and writes.
pub const VERSION: u32 = 1;

const HEADER_LEN: u64 = 64;
const TRAILER_LEN: u64 = 16;
const SECTION_ENTRY_LEN: u64 = 24;
const SECTION_ALIGN: u64 = 64;

const FLAG_STRING_IDS: u32 = 1;
const FLAG_INCOMING: u32 = 2;

const OUT_OFFSETS: u32 = 1;
const OUT_TARGETS: u32 = 2;
const OUT_EDGE_IDS: u32 = 3;
const IN_OFFSETS: u32 = 4;
const IN_TARGETS: u32 = 5;
const IN_EDGE_IDS: u32 = 6;
const IDS_INT64: u32 = 7;
const IDS_STR_OFFSETS: u32 = 8;
const IDS_STR_BYTES: u32 = 9;

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
    U32(&'a [u32]),
    /// The values of an `Int64Array`.
    Ints(Int64Array),
    /// The offsets of a `StringArray`, written less their first value so
    /// they start at 0.
    StrOffsets(StringArray),
    /// The bytes of a `StringArray` its offsets span.
    StrBytes(StringArray),
}

impl Data<'_> {
    fn width(&self) -> u64 {
        match self {
            Data::U64(_) | Data::Ints(_) => 8,
            Data::U32(_) | Data::StrOffsets(_) => 4,
            Data::StrBytes(_) => 1,
        }
    }

    fn count(&self) -> u64 {
        (match self {
            Data::U64(v) => v.len(),
            Data::U32(v) => v.len(),
            Data::Ints(a) => a.len(),
            Data::StrOffsets(a) => a.len() + 1,
            Data::StrBytes(a) => str_bytes(a).len(),
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
    key: Vec<u8>,
    sections: Vec<(u32, Data<'a>, u64)>,
    table_offset: u64,
    trailer_offset: u64,
}

impl<'a> Encoded<'a> {
    /// Lay out `topology` and `ids` under `key`. Nothing is written yet.
    pub fn new(key: &IndexKey, topology: &'a Topology, ids: &'a IdMap) -> io::Result<Self> {
        let id_array = ids.user_id_array();
        let mut flags = 0;
        let out = topology.out();
        let mut sections: Vec<(u32, Data<'a>)> = vec![
            (OUT_OFFSETS, Data::U64(&out.offsets)),
            (OUT_TARGETS, Data::U32(&out.targets)),
            (OUT_EDGE_IDS, Data::U32(&out.edge_ids)),
        ];
        if let Some(incoming) = topology.incoming_if_built() {
            flags |= FLAG_INCOMING;
            sections.extend([
                (IN_OFFSETS, Data::U64(&incoming.offsets)),
                (IN_TARGETS, Data::U32(&incoming.targets)),
                (IN_EDGE_IDS, Data::U32(&incoming.edge_ids)),
            ]);
        }
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
            key,
            sections,
            table_offset,
            trailer_offset: align(at, 8),
        })
    }

    /// The size of the file, in bytes.
    pub fn file_size(&self) -> u64 {
        self.trailer_offset + TRAILER_LEN
    }

    /// Write the file to `out`.
    pub fn write_to(&self, out: impl Write) -> io::Result<()> {
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
        header.resize(HEADER_LEN as usize, 0);
        sink.put(&header)?;
        sink.put(&self.key)?;
        sink.pad_to(self.table_offset)?;
        for (kind, data, offset) in &self.sections {
            let mut entry = Vec::with_capacity(SECTION_ENTRY_LEN as usize);
            entry.extend_from_slice(&kind.to_le_bytes());
            entry.extend_from_slice(&(data.width() as u32).to_le_bytes());
            entry.extend_from_slice(&offset.to_le_bytes());
            entry.extend_from_slice(&data.count().to_le_bytes());
            sink.put(&entry)?;
        }
        for (_, data, offset) in &self.sections {
            sink.pad_to(*offset)?;
            match data {
                Data::U64(v) => sink.put_all(v, |x| x.to_le_bytes())?,
                Data::U32(v) => sink.put_all(v, |x| x.to_le_bytes())?,
                Data::Ints(a) => sink.put_all(a.values(), |x| x.to_le_bytes())?,
                Data::StrOffsets(a) => {
                    let offsets = a.value_offsets();
                    let base = offsets[0];
                    sink.put_all(offsets, |x| (x - base).to_le_bytes())?
                }
                Data::StrBytes(a) => sink.put(str_bytes(a))?,
            }
        }
        sink.pad_to(self.trailer_offset)?;
        let crc = sink.crc.clone().finalize();
        let mut trailer = Vec::with_capacity(TRAILER_LEN as usize);
        trailer.extend_from_slice(&self.file_size().to_le_bytes());
        trailer.extend_from_slice(&crc.to_le_bytes());
        trailer.extend_from_slice(&END_MAGIC);
        sink.put(&trailer)?;
        sink.out.flush()
    }
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

/// Read the index in `bytes`, which must be stored under `key`.
pub fn decode(bytes: &[u8], key: &IndexKey) -> Result<(Topology, IdMap), DecodeError> {
    let len = bytes.len() as u64;
    if len < HEADER_LEN + TRAILER_LEN {
        return Err(format!("{len} bytes is too short for an index file"));
    }
    if bytes[..8] != MAGIC {
        return Err("not an Ursa index file".into());
    }
    let trailer = &bytes[(len - TRAILER_LEN) as usize..];
    if trailer[12..] != END_MAGIC || u64_at(trailer, 0) != len {
        return Err("the file is truncated".into());
    }
    let crc = u32::from_le_bytes(trailer[8..12].try_into().expect("4 bytes"));
    if crc32fast::hash(&bytes[..(len - TRAILER_LEN) as usize]) != crc {
        return Err("the checksum does not match".into());
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes"));
    if version != VERSION {
        return Err(format!(
            "index format version {version}; this build reads {VERSION}"
        ));
    }
    let flags = u32::from_le_bytes(bytes[12..16].try_into().expect("4 bytes"));
    let n_nodes = usize::try_from(u64_at(bytes, 16)).map_err(|e| e.to_string())?;
    let n_edges = usize::try_from(u64_at(bytes, 24)).map_err(|e| e.to_string())?;
    let key_len = u64_at(bytes, 32);
    let n_sections = u32::from_le_bytes(bytes[40..44].try_into().expect("4 bytes")) as u64;

    let body = len - TRAILER_LEN;
    let key_end = HEADER_LEN
        .checked_add(key_len)
        .filter(|&end| end <= body)
        .ok_or("the key runs past the end of the file")?;
    let stored: Value = serde_json::from_slice(&bytes[HEADER_LEN as usize..key_end as usize])
        .map_err(|e| format!("the key is not JSON: {e}"))?;
    if IndexKey::from_json(&stored).as_ref() != Some(key) {
        return Err("the file holds an index for another key".into());
    }

    let table = align(key_end, 8);
    let table_end = n_sections
        .checked_mul(SECTION_ENTRY_LEN)
        .and_then(|n| n.checked_add(table))
        .filter(|&end| end <= body)
        .ok_or("the section table runs past the end of the file")?;
    let mut sections: Vec<(u32, &[u8], u64)> = Vec::new();
    for entry in bytes[table as usize..table_end as usize].chunks_exact(SECTION_ENTRY_LEN as usize)
    {
        let kind = u32::from_le_bytes(entry[0..4].try_into().expect("4 bytes"));
        let width = u32::from_le_bytes(entry[4..8].try_into().expect("4 bytes")) as u64;
        let offset = u64_at(entry, 8);
        let count = u64_at(entry, 16);
        let end = width
            .checked_mul(count)
            .and_then(|n| n.checked_add(offset))
            .filter(|&end| offset >= table_end && end <= body)
            .ok_or("a section runs past the end of the file")?;
        sections.push((kind, &bytes[offset as usize..end as usize], width));
    }
    let section = |kind: u32, width: u64, count: Option<usize>| -> Result<&[u8], DecodeError> {
        let mut found = sections.iter().filter(|(k, _, _)| *k == kind);
        let (_, data, w) = found
            .next()
            .ok_or_else(|| format!("section {kind} is missing"))?;
        if found.next().is_some() || *w != width {
            return Err(format!("section {kind} is malformed"));
        }
        if let Some(count) = count {
            if data.len() as u64 != count as u64 * width {
                return Err(format!("section {kind} has the wrong length"));
            }
        }
        Ok(data)
    };
    let adjacency = |kinds: [u32; 3]| -> Result<Adjacency, DecodeError> {
        Ok(Adjacency {
            offsets: read(section(kinds[0], 8, Some(n_nodes + 1))?, u64::from_le_bytes),
            targets: read(section(kinds[1], 4, Some(n_edges))?, u32::from_le_bytes),
            edge_ids: read(section(kinds[2], 4, Some(n_edges))?, u32::from_le_bytes),
        })
    };
    let out = adjacency([OUT_OFFSETS, OUT_TARGETS, OUT_EDGE_IDS])?;
    let incoming = match flags & FLAG_INCOMING {
        0 => None,
        _ => Some(adjacency([IN_OFFSETS, IN_TARGETS, IN_EDGE_IDS])?),
    };
    let topology = Topology::from_parts(n_nodes, out, incoming)?;

    let ids: Arc<dyn Array> = if flags & FLAG_STRING_IDS == 0 {
        let values = read(section(IDS_INT64, 8, Some(n_nodes))?, i64::from_le_bytes);
        Arc::new(Int64Array::from(values))
    } else {
        let offsets = read(
            section(IDS_STR_OFFSETS, 4, Some(n_nodes + 1))?,
            i32::from_le_bytes,
        );
        let data = section(IDS_STR_BYTES, 1, None)?;
        if offsets[0] != 0
            || offsets.windows(2).any(|w| w[0] > w[1])
            || offsets[n_nodes] as usize != data.len()
        {
            return Err("the string ids' offsets are malformed".into());
        }
        let offsets = OffsetBuffer::new(ScalarBuffer::from(offsets));
        Arc::new(
            StringArray::try_new(offsets, Buffer::from(data.to_vec()), None)
                .map_err(|e| format!("the string ids are malformed: {e}"))?,
        )
    };
    let ids = IdMap::from_user_ids(ids.as_ref()).map_err(|e| e.to_string())?;
    if ids.len() != n_nodes {
        return Err("the id map does not match the topology".into());
    }
    Ok((topology, ids))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

/// The little-endian values in `bytes`.
fn read<T, const N: usize>(bytes: &[u8], le: fn([u8; N]) -> T) -> Vec<T> {
    bytes
        .chunks_exact(N)
        .map(|c| le(c.try_into().expect("N bytes")))
        .collect()
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

        // With the in-adjacency built, it is stored too.
        let incoming = topology.incoming().clone();
        let (back, _) = decode(&encode(&key(), &topology, &ids), &key()).unwrap();
        assert_eq!(back.incoming_if_built().unwrap().targets, incoming.targets);
        assert_eq!(
            back.incoming_if_built().unwrap().edge_ids,
            incoming.edge_ids
        );

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
