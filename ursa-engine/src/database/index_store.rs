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
//!   "version": 1,
//!   "indexes": [
//!     {"file": "…idx", "size": 123, "created": 1730000000000,
//!      "last_used": 1730000000000, "key": {"fingerprint": "…", "files": […]}}
//!   ]
//! }
//! ```
//!
//! `created` and `last_used` are milliseconds since the Unix epoch.
//!
//! **Lookup** ([`IndexStore::load`]) uses an entry only if its full key
//! matches, file identities included, and the file reads back whole; a file
//! that does not (truncated, corrupt, another version) is a miss, and a
//! writer drops its entry. A hit updates `last_used`.
//!
//! **Writes** ([`IndexStore::save`]) run on a background thread, so the query
//! that built the index is not held up by the disk. [`IndexStore::flush`]
//! waits for them; the database calls it when it closes (before it releases
//! its lock), so a short `ursa -c` process does not exit with a write half
//! done.
//!
//! **The in-adjacency** is built after the index, by the first algorithm that
//! needs it (PageRank, in-degree, `direction => 'both'`), usually after the
//! background write has started. So it is stored as a chunk of the index
//! file (see `index_file`): with the index if it exists when the index is
//! written, and otherwise appended by [`IndexStore::flush`], for every index
//! this process stored or loaded whose in-adjacency has been built since. A
//! later process lists the chunk when it loads the index and reads it only
//! when an algorithm asks for the in-adjacency, so a query that does not
//! need it does not pay for it. The manifest's `size` counts the chunks, and
//! a chunk that would take the store past the limit is not appended. A new entry replaces any entry with the same fingerprint (an index of
//! the same edges over an older version of a file), then entries are evicted,
//! least recently used first, until the total is within the session's
//! `ursa.index_storage_limit`. Never evicted: the entry just written, and an
//! index a running query holds. An index larger than the limit on its own is
//! not stored.
//!
//! A read-only database reads the manifest afresh on each lookup (a writer
//! may be changing it) and never writes.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::index_file::{self, Encoded, EncodedChunk, IndexKey, MainPart, CHUNK_INCOMING};
use crate::graph::GraphIndex;

/// The `format` field of the manifest.
pub const MANIFEST_FORMAT: &str = "ursa-indexes";
/// The manifest version this build reads and writes.
pub const MANIFEST_VERSION: u64 = 1;
const MANIFEST: &str = "manifest.json";

/// One stored index, as the manifest lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: IndexKey,
    /// The file's name in the index directory.
    pub file: String,
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
            size: value.get("size")?.as_u64()?,
            created: value.get("created")?.as_u64()?,
            last_used: value.get("last_used")?.as_u64()?,
        })
    }
}

/// The stored indexes of one database.
#[derive(Debug)]
pub struct IndexStore {
    dir: PathBuf,
    writable: bool,
    state: Mutex<State>,
    pending: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Debug, Default)]
struct State {
    /// Whether `entries` has been read from disk (a writer reads it once).
    loaded: bool,
    entries: Vec<Entry>,
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
    /// Whether the file has the in-adjacency's chunk.
    incoming: bool,
    /// The session's storage limit when it was loaded or stored.
    limit: u64,
}

impl IndexStore {
    /// The store in `dir`. A writable store creates `dir` on its first write.
    pub fn new(dir: PathBuf, writable: bool) -> Self {
        Self {
            dir,
            writable,
            state: Mutex::new(State::default()),
            pending: Mutex::new(Vec::new()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn is_writable(&self) -> bool {
        self.writable
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
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
        let mut entries = read_manifest(&self.dir);
        let before = entries.len();
        entries.retain(|e| self.dir.join(&e.file).is_file());
        // One entry per file and per fingerprint, should a manifest have more.
        let mut files = HashSet::new();
        let mut fingerprints = HashSet::new();
        entries.retain(|e| {
            files.insert(e.file.clone()) && fingerprints.insert(e.key.fingerprint.clone())
        });
        let changed = entries.len() != before;
        // Remove what the manifest does not list: index files written but
        // never listed, and temporary files of interrupted writes.
        if let Ok(listing) = std::fs::read_dir(&self.dir) {
            for item in listing.flatten() {
                let name = item.file_name().to_string_lossy().into_owned();
                let ours = name.ends_with(".idx") || name.ends_with(".tmp");
                if ours && !files.contains(&name) {
                    let _ = std::fs::remove_file(item.path());
                }
            }
        }
        state.entries = entries;
        if changed {
            let _ = write_manifest(&self.dir, &state.entries);
        }
    }

    /// The stored index for `key`, if there is one whose full key matches
    /// and whose file reads back whole. Blocking: it reads the file. `limit`
    /// is the session's storage limit, for chunks appended to it later.
    pub fn load(&self, key: &IndexKey, limit: u64) -> Option<Arc<GraphIndex>> {
        let file = if self.writable {
            let mut state = self.state();
            self.ensure_loaded(&mut state);
            state.entries.iter().find(|e| e.key == *key)?.file.clone()
        } else {
            read_manifest(&self.dir)
                .into_iter()
                .find(|e| e.key == *key)?
                .file
        };
        let read = index_file::read_file(&self.dir.join(&file), key);
        let mut state = self.state();
        match read {
            Ok(stored) => {
                let incoming = index_file::has_chunk(&stored.chunks, CHUNK_INCOMING, b"");
                let index = Arc::new(GraphIndex::new(
                    Arc::new(stored.topology),
                    Arc::new(stored.ids),
                ));
                state.live.insert(
                    file.clone(),
                    Live {
                        index: Arc::downgrade(&index),
                        main: stored.main,
                        incoming,
                        limit,
                    },
                );
                if self.writable {
                    let now = tick(&state.entries);
                    if let Some(entry) = state.entries.iter_mut().find(|e| e.file == file) {
                        entry.last_used = now;
                        let _ = write_manifest(&self.dir, &state.entries);
                    }
                }
                Some(index)
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

    /// Store `index` under `key` on a background thread, keeping the store
    /// within `limit` bytes. Nothing happens in a read-only store.
    pub fn save(self: &Arc<Self>, key: IndexKey, index: Arc<GraphIndex>, limit: u64) {
        if !self.writable {
            return;
        }
        // Tidy the directory now, never while a write's files are not yet
        // listed in the manifest.
        self.ensure_loaded(&mut self.state());
        let store = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("ursa-index-write".into())
            .spawn(move || {
                // A failed write leaves the store as it was; the index is
                // built again by a later process.
                let _ = store.write(&key, &index, limit);
            });
        if let Ok(handle) = spawned {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.retain(|h| !h.is_finished());
            pending.push(handle);
        }
    }

    /// Wait for every write [`save`](Self::save) started, then append to
    /// each index file this process stored or loaded the parts of its index
    /// built since (the in-adjacency), within the storage limit.
    pub fn flush(&self) {
        let handles = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        for handle in handles {
            let _ = handle.join();
        }
        if self.writable {
            self.append_chunks();
        }
    }

    /// Append the in-adjacency to every live index file that lacks it and
    /// whose index has built it since. A failed append leaves the file as
    /// it was (less any partial chunk); the in-adjacency is built again by
    /// the next process that needs it.
    fn append_chunks(&self) {
        let mut state = self.state();
        self.ensure_loaded(&mut state);
        let files: Vec<String> = state.live.keys().cloned().collect();
        let mut appended = Vec::new();
        for file in files {
            let live = &state.live[&file];
            if live.incoming {
                continue;
            }
            let Some(index) = live.index.upgrade() else {
                continue;
            };
            let Some(incoming) = index.topology.incoming_if_built() else {
                continue;
            };
            let (main, limit) = (live.main, live.limit);
            let Some(entry) = state.entries.iter().position(|e| e.file == file) else {
                continue;
            };
            // Not past the limit on its own, as for a whole index.
            let added = EncodedChunk::incoming(incoming, state.entries[entry].size).size();
            if state.entries[entry].size + added > limit {
                continue;
            }
            let result = index_file::append_chunk(
                &self.dir.join(&file),
                main,
                CHUNK_INCOMING,
                b"",
                |start| EncodedChunk::incoming(incoming, start),
            );
            if let Ok(appended_len) = result {
                if let Some(live) = state.live.get_mut(&file) {
                    live.incoming = true;
                }
                if let Some(len) = appended_len {
                    state.entries[entry].size = len;
                    appended.push(file.clone());
                }
            }
        }
        if !appended.is_empty() {
            let limit = state
                .live
                .values()
                .map(|l| l.limit)
                .min()
                .unwrap_or(u64::MAX);
            let removed = evict(&mut state, &appended, limit);
            let _ = write_manifest(&self.dir, &state.entries);
            drop(state);
            for gone in removed {
                let _ = std::fs::remove_file(self.dir.join(gone));
            }
        }
    }

    /// Write `index` under `key`, then update the manifest and evict. The
    /// in-adjacency, if it is built by now, is written as a chunk after the
    /// main part.
    fn write(&self, key: &IndexKey, index: &Arc<GraphIndex>, limit: u64) -> std::io::Result<()> {
        let encoded = Encoded::new(key, &index.topology, &index.ids)?;
        let incoming = index
            .topology
            .incoming_if_built()
            .map(|incoming| EncodedChunk::incoming(incoming, encoded.file_size()))
            .filter(|chunk| encoded.file_size() + chunk.size() <= limit);
        let size = incoming
            .as_ref()
            .map_or(encoded.file_size(), EncodedChunk::end);
        if size > limit {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        let file = format!("{:016x}.idx", key.hash());
        let path = self.dir.join(&file);
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
        self.ensure_loaded(&mut state);
        // Replace what this index supersedes: the same fingerprint (the same
        // edges over files that have changed since), or the same file name.
        let mut removed = Vec::new();
        state.entries.retain(|e| {
            let superseded = e.key.fingerprint == key.fingerprint || e.file == file;
            if superseded && e.file != file {
                removed.push(e.file.clone());
            }
            !superseded
        });
        let now = tick(&state.entries);
        state.entries.push(Entry {
            key: key.clone(),
            file: file.clone(),
            size,
            created: now,
            last_used: now,
        });
        state.live.insert(
            file.clone(),
            Live {
                index: Arc::downgrade(index),
                main,
                incoming: incoming.is_some(),
                limit,
            },
        );
        removed.extend(evict(&mut state, std::slice::from_ref(&file), limit));
        state.live.retain(|_, live| live.index.strong_count() > 0);
        let saved = write_manifest(&self.dir, &state.entries);
        drop(state);
        for gone in removed {
            let _ = std::fs::remove_file(self.dir.join(gone));
        }
        sync_dir(&self.dir);
        saved
    }
}

impl Drop for IndexStore {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Remove entries, least recently used first, until their total is at most
/// `limit`, keeping `keep` and any index a running query holds. Returns the
/// files of the entries removed.
fn evict(state: &mut State, keep: &[String], limit: u64) -> Vec<String> {
    let mut removed = Vec::new();
    let mut total: u64 = state.entries.iter().map(|e| e.size).sum();
    while total > limit {
        let candidate = state
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !keep.contains(&e.file) && !in_use(&state.live, &e.file))
            .min_by_key(|(_, e)| (e.last_used, e.created))
            .map(|(i, _)| i);
        let Some(i) = candidate else { break };
        let entry = state.entries.remove(i);
        total -= entry.size;
        removed.push(entry.file);
    }
    removed
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

/// The manifest in `dir`: empty if it is missing or not a manifest this
/// build reads. Entries it cannot read are skipped.
pub fn read_manifest(dir: &Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(dir.join(MANIFEST)) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    if value.get("format").and_then(Value::as_str) != Some(MANIFEST_FORMAT)
        || value.get("version").and_then(Value::as_u64) != Some(MANIFEST_VERSION)
    {
        return Vec::new();
    }
    value
        .get("indexes")
        .and_then(Value::as_array)
        .map(|entries| entries.iter().filter_map(Entry::from_json).collect())
        .unwrap_or_default()
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
