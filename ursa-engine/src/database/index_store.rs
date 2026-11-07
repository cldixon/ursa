//! The database's stored graph indexes: `club.ursa.d/indexes/`.
//!
//! ```text
//! indexes/
//!   manifest.json    one entry per stored index
//!   <hash>.idx       one index per file (see `index_file`)
//! ```
//!
//! The manifest is the source of truth: an index file it does not list is
//! removed when a writer first uses the store (a leftover of a process that
//! stopped between writing a file and listing it), and a missing or
//! unreadable manifest is an empty one. It is rewritten atomically
//! (temporary file, `fsync`, rename) on every change.
//!
//! ```json
//! {
//!   "format": "ursa-indexes",
//!   "version": 2,
//!   "indexes": [
//!     {"file": "…idx", "graph": "g", "size": 123, "created": 1730000000000,
//!      "last_used": 1730000000000,
//!      "key": {"version": 2, "fingerprint": "…", "files": […]}}
//!   ]
//! }
//! ```
//!
//! `created` and `last_used` are milliseconds since the Unix epoch; `graph`
//! names the graph the index was built for, for notices. **Version 2**
//! (#327) has the portable key (`crate::graph::index`); a manifest of another
//! version is an empty one, so a writer removes the files it lists, once, and
//! says so (a notice), and every index is built again when first used. An
//! index is never read under a key it was not written for.
//!
//! **Lookup** ([`IndexStore::load`]) uses an entry only if its full key
//! matches, file identities included, and the file reads back whole; a file
//! that does not (truncated, corrupt, another version) is a miss, and a
//! writer drops its entry. A hit updates `last_used`.
//!
//! **The limit** is the database's: its stored `ursa.index_storage_limit`,
//! or the default ([`IndexStore::set_limit`]), never a session's own `SET`,
//! so a read-only or in-memory session cannot shrink what a database keeps.
//!
//! **Storing** ([`IndexStore::save`]) decides at once, in the query that
//! built the index, and says what it decided ([`Saved`]):
//!
//! 1. A read-only store stores nothing.
//! 2. An index larger than the limit on its own is not stored.
//! 3. Otherwise it replaces any entry with the same fingerprint (an index of
//!    the same edges over an older version of a file), and evicts entries,
//!    least recently used first, only as many as it takes to fit within the
//!    limit beside the indexes being written. Never evicted: an index a
//!    running query holds. If the indexes in use leave no room, nothing is
//!    evicted and the index is not stored.
//!
//! Evictions happen before the write, so the room is there on disk too, and
//! each is a notice. The write itself runs on a background thread, so the
//! query is not held up by the disk; [`IndexStore::flush`] waits for it. The
//! database calls it when it closes (before it releases its lock), so a short
//! `ursa -c` process does not exit with a write half done. A failed write is
//! a notice, and leaves the store as it was less what it evicted.
//!
//! **The in-adjacency** is built after the index, by the first algorithm that
//! needs it (PageRank, in-degree, `direction => 'both'`), usually after the
//! index was stored. So it is stored as a chunk of the index file (see
//! `index_file`): with the index if it exists when the index is stored, and
//! otherwise appended by [`IndexStore::flush`], for every index this process
//! stored or loaded whose in-adjacency has been built since. A later process
//! lists the chunk when it loads the index and reads it only when an
//! algorithm asks for the in-adjacency, so a query that does not need it does
//! not pay for it. The manifest's `size` counts the chunks, and a chunk makes
//! room for itself as an index does (evicting other indexes, never its own),
//! or is not appended (a notice).
//!
//! **Evaluated weights** are stored the same way, one chunk per set, named by
//! the weights' key (the weighted edge plan's fingerprint and the identity of
//! the files it reads, which include the weight column's). They are used only
//! with an index whose full key matched, and only under the same weights key,
//! so a changed weight expression or source file is never served stale
//! weights. A later process reads a set when a query asks for it, and reads
//! no edge rows. They are evicted with their index, as part of its file.
//!
//! A read-only database reads the manifest afresh on each lookup (a writer
//! may be changing it) and never writes.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::index_file::{
    self, Encoded, EncodedChunk, IndexKey, MainPart, CHUNK_INCOMING, CHUNK_WEIGHTS,
};
use crate::graph::GraphIndex;
use crate::notices::{self, NoticeLevel, Notifier};
use crate::options::ByteSize;

/// The `format` field of the manifest.
pub const MANIFEST_FORMAT: &str = "ursa-indexes";
/// The manifest version this build reads and writes.
pub const MANIFEST_VERSION: u64 = 2;
const MANIFEST: &str = "manifest.json";
/// How notices name an index whose entry names no graph.
const UNNAMED: &str = "a graph";

/// One stored index, as the manifest lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: IndexKey,
    /// The file's name in the index directory.
    pub file: String,
    /// The graph it was built for, as notices name it.
    pub graph: String,
    pub size: u64,
    /// Milliseconds since the Unix epoch.
    pub created: u64,
    /// Milliseconds since the Unix epoch.
    pub last_used: u64,
}

impl Entry {
    fn to_json(&self) -> Value {
        json!({
            "file": self.file,
            "graph": self.graph,
            "size": self.size,
            "created": self.created,
            "last_used": self.last_used,
            "key": self.key.to_json(),
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        let file = value.get("file")?.as_str()?.to_string();
        // A file name, never a path out of the directory.
        if file.is_empty() || file.contains(['/', '\\']) || file.starts_with('.') {
            return None;
        }
        Some(Self {
            key: IndexKey::from_json(value.get("key")?)?,
            file,
            graph: value
                .get("graph")
                .and_then(Value::as_str)
                .unwrap_or(UNNAMED)
                .to_string(),
            size: value.get("size")?.as_u64()?,
            created: value.get("created")?.as_u64()?,
            last_used: value.get("last_used")?.as_u64()?,
        })
    }
}

/// What [`IndexStore::save`] decided for an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saved {
    /// Being written, in `size` bytes.
    Stored { size: u64 },
    /// The database is open read-only.
    ReadOnly,
    /// `size` bytes is more than the limit on its own.
    TooLarge { size: u64, limit: u64 },
    /// The indexes running queries hold leave less than `size` bytes of the
    /// limit.
    NoRoom { size: u64, limit: u64 },
    /// It cannot be stored (node ids of a type the file cannot hold).
    Unstorable(String),
}

impl Saved {
    /// Whether this loses work a later process will do again, so it is said
    /// however quick the build was.
    pub fn loses_work(&self) -> bool {
        matches!(
            self,
            Saved::TooLarge { .. } | Saved::NoRoom { .. } | Saved::Unstorable(_)
        )
    }

    /// What became of the index, as a notice says it, for the database
    /// named `database`.
    pub fn describe(&self, database: &str) -> String {
        match self {
            Saved::Stored { size } => {
                format!("stored in {database} ({})", notices::size(*size))
            }
            Saved::ReadOnly => format!("not stored: {database} is open read-only"),
            Saved::TooLarge { size, limit } => format!(
                "not stored: it takes {}, more than ursa.index_storage_limit ({}); SET \
                 ursa.index_storage_limit = '{}' to keep it",
                notices::size(*size),
                ByteSize(*limit),
                ByteSize(suggested_limit(*size))
            ),
            Saved::NoRoom { size, limit } => format!(
                "not stored: the stored indexes running queries use leave less than its {} of \
                 ursa.index_storage_limit ({})",
                notices::size(*size),
                ByteSize(*limit)
            ),
            Saved::Unstorable(why) => format!("not stored: {why}"),
        }
    }
}

/// A limit to suggest for an index of `size` bytes: room for it and as much
/// again (its in-adjacency, which algorithms such as PageRank build, is about
/// as large), rounded up to a power of two.
pub fn suggested_limit(size: u64) -> u64 {
    size.saturating_mul(2)
        .checked_next_power_of_two()
        .unwrap_or(u64::MAX)
        .max(1 << 20)
}

/// The stored indexes of one database.
#[derive(Debug)]
pub struct IndexStore {
    dir: PathBuf,
    writable: bool,
    /// The database as notices name it.
    database: String,
    /// The most bytes the store keeps: the database's limit.
    limit: AtomicU64,
    state: Mutex<State>,
    pending: Mutex<Vec<JoinHandle<()>>>,
    /// The notifier of the session that last used the store: what notices
    /// decided when the database closes are said with.
    notifier: Mutex<Notifier>,
}

#[derive(Debug, Default)]
struct State {
    /// Whether `entries` has been read from disk (a writer reads it once).
    loaded: bool,
    entries: Vec<Entry>,
    /// Bytes of the indexes being written, which `entries` does not list yet.
    reserved: u64,
    /// Indexes this process loaded or stored, by file: an entry whose index
    /// a running query holds is not evicted (see `in_use`), and chunks built
    /// since are appended to its file (see [`IndexStore::flush`]).
    live: HashMap<String, Live>,
}

/// An index this process loaded or stored, and what its file holds.
#[derive(Debug)]
struct Live {
    index: Weak<GraphIndex>,
    main: MainPart,
    /// The chunks the file has, by kind and name.
    chunks: HashSet<(u32, Vec<u8>)>,
    /// Chunks not appended for want of room: said once, not tried again.
    declined: HashSet<(u32, Vec<u8>)>,
    graph: String,
}

impl IndexStore {
    /// The store in `dir`, of the database `database` (as notices name it),
    /// keeping at most `limit` bytes. A writable store creates `dir` on its
    /// first write.
    pub fn new(dir: PathBuf, writable: bool, database: String, limit: u64) -> Self {
        Self {
            dir,
            writable,
            database,
            limit: AtomicU64::new(limit),
            state: Mutex::new(State::default()),
            pending: Mutex::new(Vec::new()),
            notifier: Mutex::new(Notifier::silent()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// The database as notices name it.
    pub fn database(&self) -> &str {
        &self.database
    }

    /// The most bytes of indexes the store keeps.
    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    /// Keep at most `limit` bytes from now on: the next index stored makes
    /// room within it. Nothing is evicted now.
    pub fn set_limit(&self, limit: u64) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notifier(&self) -> Notifier {
        self.notifier
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn use_notifier(&self, notifier: &Notifier) {
        *self.notifier.lock().unwrap_or_else(|e| e.into_inner()) = notifier.clone();
    }

    /// The manifest's entries, as this process knows them.
    pub fn entries(&self) -> Vec<Entry> {
        if !self.writable {
            return read_manifest(&self.dir);
        }
        let mut state = self.state();
        self.ensure_loaded(&mut state);
        state.entries.clone()
    }

    /// Read the manifest and tidy the directory, once (writers only).
    fn ensure_loaded(&self, state: &mut State) {
        if state.loaded {
            return;
        }
        state.loaded = true;
        let manifest = read_manifest_checked(&self.dir);
        let mut entries = manifest.entries;
        let before = entries.len();
        entries.retain(|e| self.dir.join(&e.file).is_file());
        // One entry per file and per fingerprint, should a manifest have more.
        let mut files = HashSet::new();
        let mut fingerprints = HashSet::new();
        entries.retain(|e| {
            files.insert(e.file.clone()) && fingerprints.insert(e.key.fingerprint.clone())
        });
        let changed = entries.len() != before || manifest.foreign.is_some();
        // Remove what the manifest does not list: index files written but
        // never listed, and temporary files of interrupted writes.
        let mut freed = 0;
        if let Ok(listing) = std::fs::read_dir(&self.dir) {
            for item in listing.flatten() {
                let name = item.file_name().to_string_lossy().into_owned();
                let ours = name.ends_with(".idx") || name.ends_with(".tmp");
                if ours && !files.contains(&name) {
                    freed += item.metadata().map_or(0, |m| m.len());
                    let _ = std::fs::remove_file(item.path());
                }
            }
        }
        if let Some(n) = manifest.foreign.filter(|n| *n > 0) {
            self.notifier().say(NoticeLevel::On, || {
                format!(
                    "removed {n} stored {} ({}) from {} written by another version of ursa; \
                     each is built again when it is first used",
                    if n == 1 { "index" } else { "indexes" },
                    notices::size(freed),
                    self.database
                )
            });
        }
        state.entries = entries;
        if changed {
            let _ = write_manifest(&self.dir, &state.entries);
        }
    }

    /// The stored index for `key`, if there is one whose full key matches
    /// and whose file reads back whole, and the size of its file. Blocking:
    /// it reads the file.
    pub fn load(&self, key: &IndexKey, notifier: &Notifier) -> Option<(Arc<GraphIndex>, u64)> {
        let entry = if self.writable {
            self.use_notifier(notifier);
            let mut state = self.state();
            self.ensure_loaded(&mut state);
            state.entries.iter().find(|e| e.key == *key)?.clone()
        } else {
            read_manifest(&self.dir)
                .into_iter()
                .find(|e| e.key == *key)?
        };
        let file = entry.file.clone();
        let read = index_file::read_file(&self.dir.join(&file), key);
        let mut state = self.state();
        match read {
            Ok(stored) => {
                let chunks = stored
                    .chunks
                    .iter()
                    .map(|c| (c.kind, c.name.clone()))
                    .collect();
                let weights: Vec<index_file::ChunkInfo> = stored
                    .chunks
                    .into_iter()
                    .filter(|c| c.kind == CHUNK_WEIGHTS)
                    .collect();
                let keys = weights
                    .iter()
                    .filter_map(|c| String::from_utf8(c.name.clone()).ok())
                    .collect();
                let (path, main) = (self.dir.join(&file), stored.main);
                let n_edges = stored.topology.n_edges();
                let read = Box::new(move |key: &str| {
                    let chunk = weights.iter().find(|c| c.name == key.as_bytes())?;
                    index_file::read_weights(&path, main, chunk, n_edges).ok()
                });
                let index = Arc::new(
                    GraphIndex::new(Arc::new(stored.topology), Arc::new(stored.ids))
                        .with_dropped(stored.dropped.n_dropped, stored.dropped.kept)
                        .with_stored_weights(keys, read),
                );
                state.live.insert(
                    file.clone(),
                    Live {
                        index: Arc::downgrade(&index),
                        main: stored.main,
                        chunks,
                        declined: HashSet::new(),
                        graph: entry.graph.clone(),
                    },
                );
                if self.writable {
                    let now = tick(&state.entries);
                    if let Some(entry) = state.entries.iter_mut().find(|e| e.file == file) {
                        entry.last_used = now;
                        let _ = write_manifest(&self.dir, &state.entries);
                    }
                }
                Some((index, entry.size))
            }
            Err(_) => {
                if self.writable {
                    let before = state.entries.len();
                    state.entries.retain(|e| e.file != file);
                    if state.entries.len() != before {
                        let _ = std::fs::remove_file(self.dir.join(&file));
                        let _ = write_manifest(&self.dir, &state.entries);
                    }
                }
                None
            }
        }
    }

    /// Decide whether to store `index`, built for the graph `graph`, under
    /// `key` (see the module docs), evict what that takes, and start the
    /// write on a background thread. Evictions and a failed write are said
    /// with `notifier`; what was decided is returned for the caller to say.
    pub fn save(
        self: &Arc<Self>,
        key: IndexKey,
        index: Arc<GraphIndex>,
        graph: &str,
        notifier: &Notifier,
    ) -> Saved {
        if !self.writable {
            return Saved::ReadOnly;
        }
        self.use_notifier(notifier);
        let limit = self.limit();
        let encoded = match Encoded::with_dropped(
            &key,
            &index.topology,
            &index.ids,
            index.n_dropped(),
            index.kept_rows(),
        ) {
            Ok(encoded) => encoded,
            Err(e) => return Saved::Unstorable(e.to_string()),
        };
        let main = encoded.file_size();
        let size = if main > limit {
            main
        } else {
            // The in-adjacency, if it is built by now and fits.
            index
                .topology
                .incoming_if_built()
                .map(|incoming| EncodedChunk::incoming(incoming, main).end())
                .filter(|end| *end <= limit)
                .unwrap_or(main)
        };
        drop(encoded);
        if size > limit {
            return Saved::TooLarge { size, limit };
        }
        let with_incoming = size > main;
        let file = format!("{:016x}.idx", key.hash());

        let mut state = self.state();
        self.ensure_loaded(&mut state);
        // Replaced: the same fingerprint (the same edges over files that have
        // changed since), or the same file name.
        let replaced: Vec<String> = state
            .entries
            .iter()
            .filter(|e| e.key.fingerprint == key.fingerprint || e.file == file)
            .map(|e| e.file.clone())
            .collect();
        let Some(evicted) = room_for(&state, size, &replaced, &[], limit) else {
            return Saved::NoRoom { size, limit };
        };
        let gone = self.remove(&mut state, &replaced, &evicted, notifier, limit);
        state.reserved += size;
        drop(state);
        for file in gone {
            let _ = std::fs::remove_file(self.dir.join(file));
        }

        let store = Arc::clone(self);
        let (graph, notifier) = (graph.to_string(), notifier.clone());
        let spawned = std::thread::Builder::new()
            .name("ursa-index-write".into())
            .spawn(move || {
                let written = store.write(&key, &index, &file, size, with_incoming, &graph);
                if let Err(e) = written {
                    store.state().reserved -= size;
                    notifier.say(NoticeLevel::On, || {
                        format!(
                            "could not store the index for {graph} in {}: {e}; it is built \
                             again when next needed",
                            store.database
                        )
                    });
                }
            });
        match spawned {
            Ok(handle) => {
                let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                pending.retain(|h| !h.is_finished());
                pending.push(handle);
                Saved::Stored { size }
            }
            Err(e) => {
                self.state().reserved -= size;
                Saved::Unstorable(format!("cannot start its write: {e}"))
            }
        }
    }

    /// Remove the entries of `replaced` and `evicted` from the manifest,
    /// saying each eviction. Returns the files to delete.
    fn remove(
        &self,
        state: &mut State,
        replaced: &[String],
        evicted: &[String],
        notifier: &Notifier,
        limit: u64,
    ) -> Vec<String> {
        if replaced.is_empty() && evicted.is_empty() {
            return Vec::new();
        }
        let mut gone = Vec::new();
        state.entries.retain(|e| {
            if evicted.contains(&e.file) {
                notifier.say(NoticeLevel::On, || {
                    format!(
                        "evicted the stored index for {} ({}, the least recently used) from {} \
                         to stay within ursa.index_storage_limit ({})",
                        e.graph,
                        notices::size(e.size),
                        self.database,
                        ByteSize(limit)
                    )
                });
            }
            let keep = !replaced.contains(&e.file) && !evicted.contains(&e.file);
            if !keep {
                gone.push(e.file.clone());
            }
            keep
        });
        let _ = write_manifest(&self.dir, &state.entries);
        gone
    }

    /// Wait for every write [`save`](Self::save) started, then append to
    /// each index file this process stored or loaded the parts of its index
    /// built since (the in-adjacency, evaluated weights), within the storage
    /// limit.
    pub fn flush(&self) {
        let handles = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        for handle in handles {
            let _ = handle.join();
        }
        if self.writable {
            self.append_chunks();
        }
    }

    /// Append to every live index file the in-adjacency and the weight sets
    /// its index has built since and the file lacks, each making room for
    /// itself as an index does. A failed append leaves the file as it was
    /// (less any partial chunk); what it held is built again by the next
    /// process that needs it.
    fn append_chunks(&self) {
        let notifier = self.notifier();
        let limit = self.limit();
        let mut state = self.state();
        self.ensure_loaded(&mut state);
        let files: Vec<String> = state.live.keys().cloned().collect();
        let mut gone = Vec::new();
        for file in files {
            let live = &state.live[&file];
            let Some(index) = live.index.upgrade() else {
                continue;
            };
            let (main, graph) = (live.main, live.graph.clone());
            let mut todo: Vec<(u32, Vec<u8>)> = Vec::new();
            if index.topology.incoming_if_built().is_some() {
                todo.push((CHUNK_INCOMING, Vec::new()));
            }
            let weight_sets = index.weight_sets();
            todo.extend(
                weight_sets
                    .iter()
                    .map(|(key, _)| (CHUNK_WEIGHTS, key.as_bytes().to_vec())),
            );
            todo.retain(|chunk| !live.chunks.contains(chunk) && !live.declined.contains(chunk));
            for (kind, name) in todo {
                let Some(entry) = state.entries.iter().position(|e| e.file == file) else {
                    break;
                };
                let columns: Vec<&[f64]> = match kind {
                    CHUNK_WEIGHTS => weight_sets
                        .iter()
                        .find(|(key, _)| key.as_bytes() == name.as_slice())
                        .map(|(_, w)| w.iter().map(|c| c.as_slice()).collect())
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                let encode = |start| match kind {
                    CHUNK_WEIGHTS => EncodedChunk::weights(
                        std::str::from_utf8(&name).unwrap_or_default(),
                        &columns,
                        start,
                    ),
                    _ => EncodedChunk::incoming(
                        index.topology.incoming_if_built().expect("checked above"),
                        start,
                    ),
                };
                let size = state.entries[entry].size;
                let need = encode(size).size();
                let what = match kind {
                    CHUNK_WEIGHTS => "evaluated weights",
                    _ => "in-adjacency",
                };
                let Some(evicted) = room_for(&state, need, &[], std::slice::from_ref(&file), limit)
                else {
                    notifier.say(NoticeLevel::On, || {
                        format!(
                            "did not store the {what} of the index for {graph} ({}) in {}: it \
                             would take the stored indexes past ursa.index_storage_limit ({})",
                            notices::size(need),
                            self.database,
                            ByteSize(limit)
                        )
                    });
                    if let Some(live) = state.live.get_mut(&file) {
                        live.declined.insert((kind, name));
                    }
                    continue;
                };
                gone.extend(self.remove(&mut state, &[], &evicted, &notifier, limit));
                let Some(entry) = state.entries.iter().position(|e| e.file == file) else {
                    break;
                };
                let result =
                    index_file::append_chunk(&self.dir.join(&file), main, kind, &name, encode);
                if let Ok(appended_len) = result {
                    if let Some(live) = state.live.get_mut(&file) {
                        live.chunks.insert((kind, name));
                    }
                    if let Some(len) = appended_len {
                        state.entries[entry].size = len;
                        let _ = write_manifest(&self.dir, &state.entries);
                    }
                }
            }
        }
        drop(state);
        for file in gone {
            let _ = std::fs::remove_file(self.dir.join(file));
        }
    }

    /// Write `index` under `key` to `file`, in `planned` bytes (with its
    /// in-adjacency as a chunk if `with_incoming`), then list it. Room was
    /// made by [`Self::save`].
    fn write(
        &self,
        key: &IndexKey,
        index: &Arc<GraphIndex>,
        file: &str,
        planned: u64,
        with_incoming: bool,
        graph: &str,
    ) -> std::io::Result<()> {
        let encoded = Encoded::with_dropped(
            key,
            &index.topology,
            &index.ids,
            index.n_dropped(),
            index.kept_rows(),
        )?;
        let incoming = index
            .topology
            .incoming_if_built()
            .filter(|_| with_incoming)
            .map(|incoming| EncodedChunk::incoming(incoming, encoded.file_size()));
        let size = incoming
            .as_ref()
            .map_or(encoded.file_size(), EncodedChunk::end);
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(file);
        let temp = self.dir.join(format!("{file}.tmp"));
        let written = (|| {
            let out = File::create(&temp)?;
            let mut writer = BufWriter::with_capacity(1 << 20, out);
            let main = encoded.write_to(&mut writer)?;
            if let Some(chunk) = &incoming {
                chunk.write_to(&mut writer)?;
            }
            let out = writer.into_inner().map_err(|e| e.into_error())?;
            out.sync_all()?;
            std::fs::rename(&temp, &path)?;
            Ok(main)
        })();
        let main = match written {
            Ok(main) => main,
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                return Err(e);
            }
        };

        let mut state = self.state();
        state.reserved -= planned;
        state.entries.retain(|e| e.file != file);
        let now = tick(&state.entries);
        state.entries.push(Entry {
            key: key.clone(),
            file: file.to_string(),
            graph: graph.to_string(),
            size,
            created: now,
            last_used: now,
        });
        let chunks = incoming
            .iter()
            .map(|_| (CHUNK_INCOMING, Vec::new()))
            .collect();
        state.live.insert(
            file.to_string(),
            Live {
                index: Arc::downgrade(index),
                main,
                chunks,
                declined: HashSet::new(),
                graph: graph.to_string(),
            },
        );
        state.live.retain(|_, live| live.index.strong_count() > 0);
        let saved = write_manifest(&self.dir, &state.entries);
        drop(state);
        sync_dir(&self.dir);
        saved
    }
}

impl Drop for IndexStore {
    fn drop(&mut self) {
        self.flush();
    }
}

/// The entries to evict, least recently used first, to fit `need` more bytes
/// within `limit`, beside the indexes being written and every entry but
/// those of `dropping` (which are going anyway). Entries of `keep` and any
/// index a running query holds are never chosen. `None` if no choice makes
/// room; nothing is evicted then.
fn room_for(
    state: &State,
    need: u64,
    dropping: &[String],
    keep: &[String],
    limit: u64,
) -> Option<Vec<String>> {
    let staying = |e: &&Entry| !dropping.contains(&e.file);
    let mut total: u64 = state.reserved
        + state
            .entries
            .iter()
            .filter(staying)
            .map(|e| e.size)
            .sum::<u64>();
    let mut candidates: Vec<&Entry> = state
        .entries
        .iter()
        .filter(staying)
        .filter(|e| !keep.contains(&e.file) && !in_use(&state.live, &e.file))
        .collect();
    candidates.sort_by_key(|e| (e.last_used, e.created));
    let mut evicted = Vec::new();
    let mut candidates = candidates.into_iter();
    while total.saturating_add(need) > limit {
        let entry = candidates.next()?;
        total -= entry.size;
        evicted.push(entry.file.clone());
    }
    Some(evicted)
}

/// Whether a running query holds the index stored in `file`: the session's
/// index cache holds one reference to an index it keeps, and a query using it
/// holds another.
fn in_use(live: &HashMap<String, Live>, file: &str) -> bool {
    live.get(file)
        .is_some_and(|live| live.index.strong_count() > 1)
}

/// Now, in milliseconds since the Unix epoch, and after every time in
/// `entries`, so the order of uses is the order of their times.
fn tick(entries: &[Entry]) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let latest = entries
        .iter()
        .map(|e| e.last_used.max(e.created))
        .max()
        .map_or(0, |t| t + 1);
    now.max(latest)
}

/// A manifest as read: its entries, and, for a manifest of another version,
/// how many indexes it lists (they are not read).
struct Manifest {
    entries: Vec<Entry>,
    foreign: Option<usize>,
}

/// The manifest in `dir`: empty if it is missing or not a manifest this
/// build reads. Entries it cannot read are skipped.
pub fn read_manifest(dir: &Path) -> Vec<Entry> {
    read_manifest_checked(dir).entries
}

fn read_manifest_checked(dir: &Path) -> Manifest {
    let empty = |foreign| Manifest {
        entries: Vec::new(),
        foreign,
    };
    let Ok(text) = std::fs::read_to_string(dir.join(MANIFEST)) else {
        return empty(None);
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return empty(None);
    };
    if value.get("format").and_then(Value::as_str) != Some(MANIFEST_FORMAT) {
        return empty(None);
    }
    let listed = value.get("indexes").and_then(Value::as_array);
    if value.get("version").and_then(Value::as_u64) != Some(MANIFEST_VERSION) {
        return empty(Some(listed.map_or(0, Vec::len)));
    }
    Manifest {
        entries: listed
            .map(|entries| entries.iter().filter_map(Entry::from_json).collect())
            .unwrap_or_default(),
        foreign: None,
    }
}

/// Rewrite the manifest in `dir` atomically.
fn write_manifest(dir: &Path, entries: &[Entry]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let value = json!({
        "format": MANIFEST_FORMAT,
        "version": MANIFEST_VERSION,
        "indexes": entries.iter().map(Entry::to_json).collect::<Vec<_>>(),
    });
    let mut text = serde_json::to_string_pretty(&value).map_err(std::io::Error::other)?;
    text.push('\n');
    let temp = dir.join(format!("{MANIFEST}.tmp"));
    {
        let mut file = File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, dir.join(MANIFEST))?;
    sync_dir(dir);
    Ok(())
}

/// `fsync` a directory so a rename in it is durable. Best effort.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_suggested_limit_leaves_room_for_the_in_adjacency() {
        let gib = 1u64 << 30;
        assert_eq!(suggested_limit(21 * gib), 64 * gib);
        assert_eq!(suggested_limit(1_503_238_554), 4 * gib);
        assert_eq!(suggested_limit(3000), 1 << 20);
        assert_eq!(ByteSize(suggested_limit(13 * gib)).to_string(), "32GiB");
    }
}
