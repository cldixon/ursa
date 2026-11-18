//! Loading a stored graph index without reading it whole (#382).
//!
//! At full scale a stored index is tens of GiB, and reading it into memory
//! took 15 s on one core, which every query in a new process paid, even one
//! that reads the few thousand edges around one node. So an index file is
//! **memory-mapped** where it can be, and read **in parallel** where it
//! cannot.
//!
//! **The format** (`index_file`, version 3) is the version 2 layout: raw
//! little-endian arrays at 64-byte-aligned offsets, each a ready `&[u64]` or
//! `&[u32]` in place, with no decode step. What changes is the checksum. A
//! version 2 file had one CRC over the whole main part, so nothing in it could
//! be trusted before every byte was read. Version 3 gives **each section its
//! own CRC**, in the section table, and the trailer's CRC covers the header,
//! key and table (so, through the section CRCs, still every byte: it remains
//! the main part's identity, which chunks are checked against). Padding is
//! zero, and checked.
//!
//! **What is resident and what is mapped.** Loading checks the header, key
//! and table, then the `O(nodes)` sections in full: the out-adjacency's
//! offsets (CRC, and that they rise from 0 to the edge count, so every
//! neighbour slice is in bounds) and the node ids (CRC, then the id map is
//! built from them, as before). The `O(edges)` sections, the targets and edge
//! ids, are borrowed from the map as Arrow buffers (`ursa_core::Adjacency`
//! holds `ScalarBuffer`s) and not read: their CRCs are **deferred**
//! ([`Unverified`]). The in-adjacency chunk, when a query first asks for it,
//! is borrowed from the same map the same way, its data CRC deferred too.
//!
//! **Who checks the deferred CRCs.** A whole-graph algorithm reads every byte
//! of the arrays anyway, so before one runs (`GraphAlgorithmExec`) the engine
//! checks every deferred CRC, in parallel, once per loaded index; a CRC that
//! does not match drops the stored index and builds it again from its
//! sources, with a notice, never a result from damaged arrays. A point
//! traversal (`hop`, `bfs`, a path search) reads only the adjacency lists it
//! visits and does not check the arrays' CRCs: that is the point of mapping.
//! Its safety does not rest on them: the offsets are checked, so every slice
//! is in bounds, and a damaged target or edge id can only fail a bounds check
//! in a kernel (no kernel indexes with one unchecked), which is a query
//! error, never undefined behaviour.
//!
//! **Atomic replacement, and files that change under a map.** Ursa never
//! changes a byte of a stored index in place: a new index is written to a
//! temporary file and renamed over the old one, an eviction unlinks the file,
//! and a chunk is only appended after the last chunk that checks out. On
//! Unix a rename or unlink leaves a mapped file's pages in place until the
//! last map of them goes, so a process keeps reading the index it loaded
//! while another replaces it. A process maps only the main part and the
//! chunks that check out when it loads, never the bytes past them, which are
//! the only bytes an append truncates (a chunk cut short by a crash), so
//! nothing it maps ever shrinks under it (which would be `SIGBUS`). Editing a
//! file in `indexes/` by other means while Ursa runs is not supported, as for
//! any memory-mapped database.
//!
//! **Windows** keeps an open mapping from being renamed over or deleted, which
//! the store relies on, so there (and on any target without `mmap`, and for a
//! version 2 file, whose single CRC needs every byte read anyway) the file is
//! read whole, in parallel, into memory ([`read_whole`]), and every CRC is
//! checked as it loads.
//!
//! **Memory accounting.** Mapped pages are the operating system's page
//! cache: they count in a process's RSS once touched, but they are shared
//! between processes reading the same index, and the kernel drops clean pages
//! under memory pressure and reads them again on demand, instead of the
//! process being killed. A fresh process that answers a point query touches
//! only the pages it reads. `ursa.index_storage_limit` counts the files on
//! disk, as before.
//!
//! **Compatibility.** A version 2 file (written by 0.10) is read as before,
//! whole and checked, then rewritten as version 3 by a writable database
//! (a notice says so), so the next process maps it. Version 1 files are not
//! read, as before: they are built again.

use std::fs::File;
use std::ops::Range;
use std::sync::Mutex;

use datafusion::arrow::buffer::Buffer;

/// Whether this build maps index files: Unix, little-endian (the files are
/// little-endian, so a mapped array is usable in place).
pub const MAPS: bool = cfg!(all(
    unix,
    target_endian = "little",
    not(target_arch = "wasm32")
));

/// The first `len` bytes of `file`, memory-mapped read-only, as an Arrow
/// buffer that unmaps them when its last clone is dropped.
#[cfg(all(unix, target_endian = "little", not(target_arch = "wasm32")))]
pub fn map(file: &File, len: u64) -> Result<Buffer, String> {
    use std::ptr::NonNull;
    use std::sync::Arc;

    use rustix::mm::{mmap, munmap, MapFlags, ProtFlags};

    /// A mapping, unmapped on drop.
    struct Mapping {
        ptr: NonNull<u8>,
        len: usize,
    }
    // SAFETY: the mapping is read-only and owned by this value alone; the
    // bytes are only ever read, from any thread.
    unsafe impl Send for Mapping {}
    unsafe impl Sync for Mapping {}
    impl std::panic::RefUnwindSafe for Mapping {}
    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: `ptr` and `len` are one whole mapping made by `map`,
            // which no buffer refers to once this, their owner, is dropped.
            let _ = unsafe { munmap(self.ptr.as_ptr().cast(), self.len) };
        }
    }

    let len = usize::try_from(len).map_err(|e| e.to_string())?;
    if len == 0 {
        return Ok(Buffer::from_vec(Vec::<u8>::new()));
    }
    // SAFETY: a fresh read-only shared mapping of a file Ursa never changes
    // in place (see the module docs), at an address the kernel picks.
    let ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            ProtFlags::READ,
            MapFlags::SHARED,
            file,
            0,
        )
    }
    .map_err(|e| format!("cannot map the file: {e}"))?;
    let ptr = NonNull::new(ptr.cast::<u8>()).ok_or("cannot map the file")?;
    let owner = Arc::new(Mapping { ptr, len });
    // SAFETY: `ptr` is valid for `len` bytes for as long as `owner` lives,
    // which the buffer keeps alive.
    Ok(unsafe { Buffer::from_custom_allocation(ptr, len, owner) })
}

/// Mapping is not available on this target: see [`MAPS`].
#[cfg(not(all(unix, target_endian = "little", not(target_arch = "wasm32"))))]
pub fn map(_file: &File, _len: u64) -> Result<Buffer, String> {
    Err("index files are not mapped on this platform".into())
}

/// The bytes a parallel read or CRC gives each thread, at least.
const PIECE: usize = 32 << 20;

/// How many threads a parallel read or CRC uses for `len` bytes.
fn threads_for(len: usize) -> usize {
    let pieces = len.div_ceil(PIECE).max(1);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        cores.min(pieces)
    }
    // One thread in the browser.
    #[cfg(target_arch = "wasm32")]
    {
        pieces.min(1)
    }
}

/// The first `len` bytes of `file`, read into memory, in parallel, as an
/// Arrow buffer aligned for 64-bit values.
pub fn read_whole(file: &File, len: u64) -> Result<Buffer, String> {
    let len = usize::try_from(len).map_err(|e| e.to_string())?;
    let mut words = vec![0u64; len.div_ceil(8)];
    {
        // SAFETY: any bytes are valid `u64`s and the other way round; the
        // view covers the vector's own allocation and ends before it does.
        let bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), len) };
        read_parallel(file, bytes)?;
    }
    Ok(Buffer::from_vec(words).slice_with_length(0, len))
}

/// Fill `bytes` from the start of `file`, a piece per thread.
fn read_parallel(file: &File, bytes: &mut [u8]) -> Result<(), String> {
    let threads = threads_for(bytes.len());
    if threads <= 1 {
        return read_exact_at(file, bytes, 0);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let piece = bytes.len().div_ceil(threads);
        std::thread::scope(|scope| {
            let reads: Vec<_> = bytes
                .chunks_mut(piece)
                .enumerate()
                .map(|(i, part)| scope.spawn(move || read_exact_at(file, part, (i * piece) as u64)))
                .collect();
            reads
                .into_iter()
                .try_for_each(|r| r.join().unwrap_or_else(|_| Err("a read failed".into())))
        })
    }
    #[cfg(target_arch = "wasm32")]
    {
        read_exact_at(file, bytes, 0)
    }
}

/// Fill `buf` from `file` at `offset`.
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> Result<(), String> {
    let short = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => "the file is truncated".to_string(),
        _ => e.to_string(),
    };
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buf, offset).map_err(short)
    }
    #[cfg(windows)]
    {
        let (mut buf, mut offset) = (buf, offset);
        while !buf.is_empty() {
            match std::os::windows::fs::FileExt::seek_read(file, buf, offset) {
                Ok(0) => return Err("the file is truncated".into()),
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = file;
        file.seek(SeekFrom::Start(offset)).map_err(short)?;
        file.read_exact(buf).map_err(short)
    }
}

/// The CRC-32 of `bytes`, computed in parallel pieces that are then combined
/// (the same value as one pass).
pub fn crc(bytes: &[u8]) -> u32 {
    let threads = threads_for(bytes.len());
    if threads <= 1 {
        return crc32fast::hash(bytes);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let piece = bytes.len().div_ceil(threads);
        let parts: Vec<crc32fast::Hasher> = std::thread::scope(|scope| {
            let parts: Vec<_> = bytes
                .chunks(piece)
                .map(|part| {
                    scope.spawn(move || {
                        let mut hasher = crc32fast::Hasher::new();
                        hasher.update(part);
                        hasher
                    })
                })
                .collect();
            parts
                .into_iter()
                .map(|p| p.join().expect("a CRC thread does not panic"))
                .collect()
        });
        let mut parts = parts.into_iter();
        let mut all = parts.next().unwrap_or_default();
        for part in parts {
            all.combine(&part);
        }
        all.finalize()
    }
    #[cfg(target_arch = "wasm32")]
    {
        crc32fast::hash(bytes)
    }
}

/// One deferred checksum: the bytes `range` of a mapped file, and their CRC.
#[derive(Debug, Clone)]
pub struct Region {
    /// What the bytes are, as a message names them.
    pub what: &'static str,
    pub range: Range<usize>,
    pub crc: u32,
}

/// The checksums of a mapped index that have not been checked yet (see the
/// module docs): its edge arrays', and its in-adjacency's once read.
pub struct Unverified {
    data: Buffer,
    state: Mutex<Checks>,
}

#[derive(Default)]
struct Checks {
    pending: Vec<Region>,
    /// Whether [`Unverified::verify`] has run: a region deferred since is
    /// checked at once.
    verified: bool,
    /// Why the index is damaged, once a checksum has not matched.
    damage: Option<String>,
    /// Said once, when the damage is found: drops the stored index.
    on_damage: Option<Box<dyn FnOnce() + Send>>,
}

impl std::fmt::Debug for Unverified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        write!(
            f,
            "Unverified({} pending, verified: {}, damage: {:?})",
            state.pending.len(),
            state.verified,
            state.damage
        )
    }
}

impl Unverified {
    /// The deferred checksums of `regions` of the mapped bytes `data`.
    pub fn new(data: Buffer, regions: Vec<Region>) -> Self {
        Self {
            data,
            state: Mutex::new(Checks {
                pending: regions,
                ..Checks::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Checks> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Call `f` once, if the index is found damaged.
    pub fn on_damage(&self, f: Box<dyn FnOnce() + Send>) {
        self.lock().on_damage = Some(f);
    }

    /// Check every deferred checksum, in parallel, and from now on each
    /// region as it is deferred. `Err` says what is damaged; it stays damaged.
    pub fn verify(&self) -> Result<(), String> {
        let mut state = self.lock();
        state.verified = true;
        let pending = std::mem::take(&mut state.pending);
        for region in &pending {
            self.check(&mut state, region)?;
        }
        state.damage.clone().map_or(Ok(()), Err)
    }

    /// Defer the checksum of `region`, or check it now if [`Self::verify`]
    /// has run.
    pub fn defer(&self, region: Region) -> Result<(), String> {
        let mut state = self.lock();
        if state.verified {
            self.check(&mut state, &region)
        } else {
            state.pending.push(region);
            Ok(())
        }
    }

    fn check(&self, state: &mut Checks, region: &Region) -> Result<(), String> {
        if let Some(damage) = &state.damage {
            return Err(damage.clone());
        }
        let bytes = self.data.as_slice().get(region.range.clone());
        if bytes.is_some_and(|bytes| crc(bytes) == region.crc) {
            return Ok(());
        }
        let damage = format!("the checksum of its {} does not match", region.what);
        state.damage = Some(damage.clone());
        if let Some(f) = state.on_damage.take() {
            f();
        }
        Err(damage)
    }

    /// Whether a checksum has been found not to match.
    pub fn is_damaged(&self) -> bool {
        self.lock().damage.is_some()
    }

    /// Whether every checksum deferred so far has been checked.
    pub fn is_verified(&self) -> bool {
        let state = self.lock();
        state.verified && state.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_parallel_crc_is_the_serial_one() {
        let bytes: Vec<u8> = (0..(3 * PIECE + 12345))
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        assert_eq!(crc(&bytes), crc32fast::hash(&bytes));
        assert_eq!(crc(&bytes[..10]), crc32fast::hash(&bytes[..10]));
        assert_eq!(crc(&[]), crc32fast::hash(&[]));
    }

    #[test]
    fn a_whole_read_is_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        let bytes: Vec<u8> = (0..(2 * PIECE + 77)).map(|i| (i % 253) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let file = File::open(&path).unwrap();
        let read = read_whole(&file, bytes.len() as u64).unwrap();
        assert_eq!(read.as_slice(), &bytes[..]);
        assert_eq!(read.as_ptr() as usize % 8, 0);
        assert!(read_whole(&file, bytes.len() as u64 + 1).is_err());
        if MAPS {
            let mapped = map(&file, bytes.len() as u64).unwrap();
            assert_eq!(mapped.as_slice(), &bytes[..]);
        }
    }

    #[test]
    fn a_deferred_checksum_is_checked_once_verified() {
        let data = Buffer::from_vec(vec![1u8, 2, 3, 4, 5, 6, 7, 8]);
        let good = Region {
            what: "targets",
            range: 0..4,
            crc: crc32fast::hash(&[1, 2, 3, 4]),
        };
        let bad = Region {
            what: "in-adjacency",
            range: 4..8,
            crc: 0,
        };
        let checks = Unverified::new(data, vec![good.clone()]);
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&dropped);
        checks.on_damage(Box::new(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst)
        }));
        // Deferred, not checked, before `verify`.
        checks.defer(bad.clone()).unwrap();
        assert!(!checks.is_damaged());
        let err = checks.verify().unwrap_err();
        assert!(err.contains("in-adjacency"), "{err}");
        assert!(checks.is_damaged() && dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(checks.verify().is_err());

        let checks = Unverified::new(Buffer::from_vec(vec![1u8, 2, 3, 4, 5, 6, 7, 8]), vec![good]);
        checks.verify().unwrap();
        assert!(checks.is_verified());
        // Checked at once after `verify`.
        assert!(checks.defer(bad).is_err());
    }
}
