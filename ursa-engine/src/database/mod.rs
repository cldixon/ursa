//! The `.ursa` database file: a session's catalog, kept between processes.
//!
//! `ursa club.ursa` opens (or creates) a [`Database`] and installs it on the
//! session. The design is the Tempra page `/sql/ursa-file.md` in
//! `cl_dixon/ursa` (cldixon/ursa#237). On disk:
//!
//! ```text
//! club.ursa          the catalog: the statements that defined each object
//! club.ursa.d/       created on first use
//!   lock             held while a process has the database open for writing
//!   indexes/         stored graph indexes: `manifest.json` and `<hash>.idx`
//! ```
//!
//! This module owns the file, the sidecar directory and the lock. It stores
//! the catalog as an ordered list of statement texts ([`catalog_file`]) and
//! knows nothing of SQL: which statement defines which object, and replaying
//! them, is `ursa-sql`'s `database` module.
//!
//! A database is reached from the session config ([`database`],
//! [`database_of_config`]), so any code holding the session state or a task
//! context (the graph index cache, which stores indexes in it) can find it.
//!
//! **Relative paths** follow DuckDB. A relative file path in SQL (in `FROM`,
//! a graph's edge or vertex table, an external table's `LOCATION`, a
//! `COPY … TO` target) resolves against the current directory when the
//! statement runs, with or without a database. When a definition is stored in
//! the catalog, each of its local file paths is rewritten relative to the
//! database's directory ([`Database::stored_location`]), or made absolute if
//! it is outside that directory; replaying the catalog resolves the stored
//! paths against the database's directory ([`Database::replayed_location`]).
//! So a later process finds the same files from any directory, and a database
//! and its data files can move together. URLs are never rewritten.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionState;
use datafusion::prelude::SessionConfig;

pub mod catalog_file;
pub mod index_file;
pub mod index_store;

use crate::options::{ByteSize, DEFAULT_INDEX_STORAGE_LIMIT};

pub use catalog_file::{CatalogFile, FORMAT, VERSION};
pub use index_file::IndexKey;
pub use index_store::IndexStore;

/// How long a writer waits for another process to close the database.
pub const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// How to open a database.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    /// Open without the lock, and refuse statements that change the catalog.
    /// The file must exist.
    pub read_only: bool,
    /// How long to wait for the lock before failing with "… is in use by
    /// another process".
    pub lock_timeout: Duration,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            read_only: false,
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
        }
    }
}

impl OpenOptions {
    pub fn read_only() -> Self {
        Self {
            read_only: true,
            ..Self::default()
        }
    }
}

/// An open `.ursa` database.
#[derive(Debug)]
pub struct Database {
    /// The path as the user gave it: how messages name the database.
    name: String,
    /// The catalog file, absolute.
    path: PathBuf,
    /// The directory holding the catalog file, absolute: stored paths are
    /// relative to it.
    dir: PathBuf,
    /// `club.ursa.d`, absolute.
    sidecar: PathBuf,
    read_only: bool,
    /// The locked `club.ursa.d/lock`, held until the database is dropped.
    /// `None` when read-only.
    _lock: Option<File>,
    /// The catalog (its statements and settings), as last read or written.
    catalog: Mutex<CatalogFile>,
    /// The stored graph indexes, opened on first use.
    indexes: OnceLock<Arc<IndexStore>>,
}

/// The setting a database keeps in its catalog (#327).
pub const INDEX_STORAGE_LIMIT: &str = "ursa.index_storage_limit";

impl Database {
    /// Open the database at `path`, creating it (and its sidecar directory)
    /// unless read-only. A writer takes the lock first, waiting up to
    /// `options.lock_timeout`, then reads the catalog.
    pub fn open(path: impl AsRef<Path>, options: &OpenOptions) -> Result<Self> {
        let given = path.as_ref();
        let name = given.display().to_string();
        let path = std::path::absolute(given).map_err(|e| {
            DataFusionError::Execution(format!("cannot open database '{name}': {e}"))
        })?;
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        let file_name = path.file_name().ok_or_else(|| {
            DataFusionError::Execution(format!("'{name}' does not name a database file"))
        })?;
        let mut sidecar_name = file_name.to_os_string();
        sidecar_name.push(".d");
        let sidecar = dir.join(sidecar_name);

        if options.read_only {
            if !path.exists() {
                return Err(DataFusionError::Execution(format!(
                    "cannot open '{name}' read-only: the file does not exist"
                )));
            }
            let catalog = CatalogFile::read(&path, &name)?;
            return Ok(Self {
                name,
                path,
                dir,
                sidecar,
                read_only: true,
                _lock: None,
                catalog: Mutex::new(catalog),
                indexes: OnceLock::new(),
            });
        }

        if !dir.is_dir() {
            return Err(DataFusionError::Execution(format!(
                "cannot create database '{name}': the directory {} does not exist",
                dir.display()
            )));
        }
        let io = |what: &str, e: std::io::Error| {
            DataFusionError::Execution(format!("cannot open database '{name}': {what}: {e}"))
        };
        std::fs::create_dir_all(&sidecar)
            .map_err(|e| io(&format!("creating {}", sidecar.display()), e))?;
        let mut lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(sidecar.join("lock"))
            .map_err(|e| io("opening its lock file", e))?;
        acquire(&lock, &name, options.lock_timeout)?;
        record_holder(&mut lock);

        let catalog = if path.exists() {
            CatalogFile::read(&path, &name)?
        } else {
            let empty = CatalogFile::default();
            empty.write(&path, &sidecar, &name)?;
            empty
        };
        Ok(Self {
            name,
            path,
            dir,
            sidecar,
            read_only: false,
            _lock: Some(lock),
            catalog: Mutex::new(catalog),
            indexes: OnceLock::new(),
        })
    }

    /// The database as the user named it (`club.ursa`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The catalog file, absolute.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory holding the catalog file: stored relative paths are
    /// relative to it.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The sidecar directory, `club.ursa.d`. It exists unless read-only.
    pub fn sidecar_dir(&self) -> &Path {
        &self.sidecar
    }

    /// Where stored graph indexes go (`club.ursa.d/indexes`), created on
    /// the first write.
    pub fn index_dir(&self) -> PathBuf {
        self.sidecar.join("indexes")
    }

    /// The database's stored graph indexes. A read-only database reads them
    /// and never writes.
    pub fn index_store(&self) -> Arc<IndexStore> {
        Arc::clone(self.indexes.get_or_init(|| {
            Arc::new(IndexStore::new(
                self.index_dir(),
                !self.read_only,
                self.name.clone(),
                self.index_storage_limit(),
            ))
        }))
    }

    /// Wait for stored-index writes still running.
    pub fn flush(&self) {
        if let Some(store) = self.indexes.get() {
            store.flush();
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn catalog(&self) -> std::sync::MutexGuard<'_, CatalogFile> {
        self.catalog.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The catalog's statements, in replay order.
    pub fn statements(&self) -> Vec<String> {
        self.catalog().statements.clone()
    }

    /// The settings the catalog keeps, by full name, as `SET` takes them.
    pub fn settings(&self) -> BTreeMap<String, String> {
        self.catalog().settings.clone()
    }

    /// The most bytes of stored indexes this database keeps: its stored
    /// `ursa.index_storage_limit`, or the default. A session's own `SET` in
    /// a read-only database does not change it.
    pub fn index_storage_limit(&self) -> u64 {
        self.catalog()
            .settings
            .get(INDEX_STORAGE_LIMIT)
            .and_then(|v| ByteSize::parse(v).ok())
            .map_or(DEFAULT_INDEX_STORAGE_LIMIT, |b| b.0)
    }

    /// Replace the catalog's statements with `statements`, rewriting the
    /// file atomically (temporary file, `fsync`, rename). Refused when
    /// read-only.
    pub fn save(&self, statements: Vec<String>) -> Result<()> {
        let mut catalog = self.catalog();
        if !self.read_only && catalog.statements == statements {
            return Ok(());
        }
        let updated = CatalogFile {
            statements,
            settings: catalog.settings.clone(),
        };
        self.write_catalog(&mut catalog, updated)
    }

    /// Keep the setting `name` as `value` in the catalog (`None`: remove
    /// it, back to the default), rewriting the file. Refused when read-only.
    /// `value` must be valid for the setting.
    pub fn set_setting(&self, name: &str, value: Option<&str>) -> Result<()> {
        if name == INDEX_STORAGE_LIMIT {
            if let Some(value) = value {
                ByteSize::parse(value)?;
            }
        }
        let mut catalog = self.catalog();
        let mut updated = catalog.clone();
        match value {
            Some(value) => updated.settings.insert(name.to_string(), value.to_string()),
            None => updated.settings.remove(name),
        };
        if !self.read_only && updated == *catalog {
            return Ok(());
        }
        self.write_catalog(&mut catalog, updated)?;
        drop(catalog);
        if let Some(store) = self.indexes.get() {
            store.set_limit(self.index_storage_limit());
        }
        Ok(())
    }

    fn write_catalog(&self, current: &mut CatalogFile, updated: CatalogFile) -> Result<()> {
        if self.read_only {
            return Err(DataFusionError::Plan(format!(
                "{} is open read-only",
                self.name
            )));
        }
        updated.write(&self.path, &self.sidecar, &self.name)?;
        *current = updated;
        Ok(())
    }

    /// How the catalog stores `location`, written in a statement run from
    /// `cwd`: a local path inside the database's directory becomes relative to
    /// it, any other local path becomes absolute. `None` when the location is
    /// stored as written: a URL, a `~/…` path, or a path already in its
    /// stored form.
    pub fn stored_location(&self, location: &str, cwd: &Path) -> Option<String> {
        if !is_local_path(location) {
            return None;
        }
        let absolute = normalize(&cwd.join(location));
        let canonical_dir = std::fs::canonicalize(&self.dir).ok();
        let inside = [Some(&self.dir), canonical_dir.as_ref()]
            .into_iter()
            .flatten()
            .find_map(|dir| absolute.strip_prefix(dir).ok());
        let mut stored = match inside {
            Some(relative) if relative.as_os_str().is_empty() => ".".to_string(),
            Some(relative) => relative.to_string_lossy().into_owned(),
            None => absolute.to_string_lossy().into_owned(),
        };
        keep_trailing_separator(location, &mut stored);
        (stored != location).then_some(stored)
    }

    /// `location` from a stored statement as replay runs it: a relative local
    /// path resolved against the database's directory; `None` for anything
    /// else (a URL, an absolute path, `~/…`), which runs as stored.
    pub fn replayed_location(&self, location: &str) -> Option<String> {
        if !is_relative_path(location) {
            return None;
        }
        let mut resolved = normalize(&self.dir.join(location))
            .to_string_lossy()
            .into_owned();
        keep_trailing_separator(location, &mut resolved);
        Some(resolved)
    }
}

impl Drop for Database {
    /// Finish writing stored indexes before the lock is released.
    fn drop(&mut self) {
        self.flush();
    }
}

/// Whether `location` is a local path (not a URL, not starting with `~`).
pub fn is_local_path(location: &str) -> bool {
    if location.is_empty() || location.starts_with('~') {
        return false;
    }
    match url::Url::parse(location) {
        // A one-letter scheme is a Windows drive letter.
        Ok(url) => url.scheme().len() == 1,
        Err(_) => true,
    }
}

/// Whether `location` is a relative local path (not a URL, not absolute, not
/// starting with `~`).
pub fn is_relative_path(location: &str) -> bool {
    is_local_path(location) && Path::new(location).is_relative()
}

/// `path` with `.` and `..` components removed lexically, without touching
/// the filesystem (the path may be a glob, or not exist yet).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `..` at the root stays at the root.
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other),
        }
    }
    out
}

/// A directory written with a trailing separator (`data/`) keeps it.
fn keep_trailing_separator(original: &str, rewritten: &mut String) {
    let separator = std::path::MAIN_SEPARATOR;
    if (original.ends_with('/') || original.ends_with(separator))
        && !rewritten.ends_with(['/', separator])
    {
        rewritten.push(separator);
    }
}

/// Take the exclusive lock on `lock`, retrying until `timeout` has passed.
fn acquire(lock: &File, name: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                if start.elapsed() >= timeout {
                    let holder = holder(lock)
                        .map(|pid| format!(" (process {pid})"))
                        .unwrap_or_default();
                    return Err(DataFusionError::Execution(format!(
                        "{name} is in use by another process{holder}, which has it open for \
                         writing; open it read-only to query it meanwhile (ursa --readonly \
                         {name}), or wait for that process to finish"
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(DataFusionError::Execution(format!(
                    "cannot lock database '{name}': {e}"
                )))
            }
        }
    }
}

/// Write this process's id into the lock file it holds, so another process
/// waiting for the lock can name it. Best effort.
fn record_holder(lock: &mut File) {
    let _ = lock.set_len(0).and_then(|()| {
        lock.rewind()?;
        writeln!(lock, "{}", std::process::id())?;
        lock.flush()
    });
}

/// The id of the process holding `lock`, as it recorded it, if it can be read.
fn holder(lock: &File) -> Option<u32> {
    let mut text = String::new();
    let mut reader = lock;
    reader.rewind().ok()?;
    reader.take(32).read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

/// Where a session keeps its database: empty for an in-memory session, set
/// once by opening or attaching one. Installed on every session's config by
/// `new_session`, so `ATTACH` can fill it later.
#[derive(Debug, Default)]
pub struct DatabaseSlot {
    database: OnceLock<Arc<Database>>,
}

impl DatabaseSlot {
    pub fn get(&self) -> Option<Arc<Database>> {
        self.database.get().cloned()
    }

    /// Put `database` in the slot; fails if the session already has one.
    pub fn set(&self, database: Arc<Database>) -> Result<()> {
        self.database
            .set(database)
            .map_err(|_| DataFusionError::Plan("this session already has a database open".into()))
    }
}

/// Install an empty database slot on `config` (done by `new_session`).
pub(crate) fn with_database_slot(config: SessionConfig) -> SessionConfig {
    config.with_extension(Arc::new(DatabaseSlot::default()))
}

/// The session's database slot.
pub fn slot(state: &SessionState) -> Arc<DatabaseSlot> {
    slot_of_config(state.config())
}

fn slot_of_config(config: &SessionConfig) -> Arc<DatabaseSlot> {
    config
        .get_extension::<DatabaseSlot>()
        .expect("an Ursa session has a database slot (built by ursa_engine::new_session)")
}

/// The session's database, if it has one.
pub fn database(state: &SessionState) -> Option<Arc<Database>> {
    database_of_config(state.config())
}

/// The database of the session `config` belongs to, if it has one. A task
/// context's `session_config()` reaches it too.
pub fn database_of_config(config: &SessionConfig) -> Option<Arc<Database>> {
    config.get_extension::<DatabaseSlot>()?.get()
}

/// Wait for the session's stored-index writes, if it has a database. Call it
/// before a process exits, so an index built by its last query is stored.
pub fn flush(state: &SessionState) {
    if let Some(db) = database(state) {
        db.flush();
    }
}

#[cfg(test)]
mod tests;
