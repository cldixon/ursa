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
//! **Relative paths.** In a session with a database, a relative file path in
//! SQL (in `FROM`, a graph's edge or vertex table, an external table's
//! `LOCATION`, a `COPY … TO` target) is resolved against the directory that
//! holds the `.ursa` file ([`resolve_location`]), so a database and its data
//! files can move together. The catalog keeps the path as written.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::SessionState;
use datafusion::prelude::SessionConfig;

pub mod catalog_file;
pub mod index_file;
pub mod index_store;

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
    /// The directory holding the catalog file, absolute: relative paths in
    /// SQL resolve against it.
    dir: PathBuf,
    /// `club.ursa.d`, absolute.
    sidecar: PathBuf,
    read_only: bool,
    /// The locked `club.ursa.d/lock`, held until the database is dropped.
    /// `None` when read-only.
    _lock: Option<File>,
    /// The catalog's statements, as last read or written.
    statements: Mutex<Vec<String>>,
    /// The stored graph indexes, opened on first use.
    indexes: OnceLock<Arc<IndexStore>>,
}

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
                statements: Mutex::new(catalog.statements),
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
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(sidecar.join("lock"))
            .map_err(|e| io("opening its lock file", e))?;
        acquire(&lock, &name, options.lock_timeout)?;

        let statements = if path.exists() {
            CatalogFile::read(&path, &name)?.statements
        } else {
            let empty = CatalogFile::default();
            empty.write(&path, &sidecar, &name)?;
            empty.statements
        };
        Ok(Self {
            name,
            path,
            dir,
            sidecar,
            read_only: false,
            _lock: Some(lock),
            statements: Mutex::new(statements),
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

    /// The directory holding the catalog file: relative paths resolve here.
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
        Arc::clone(
            self.indexes
                .get_or_init(|| Arc::new(IndexStore::new(self.index_dir(), !self.read_only))),
        )
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

    /// The catalog's statements, in replay order.
    pub fn statements(&self) -> Vec<String> {
        self.statements
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replace the catalog with `statements`, rewriting the file atomically
    /// (temporary file, `fsync`, rename). Refused when read-only.
    pub fn save(&self, statements: Vec<String>) -> Result<()> {
        if self.read_only {
            return Err(DataFusionError::Plan(format!(
                "{} is open read-only",
                self.name
            )));
        }
        let mut current = self.statements.lock().unwrap_or_else(|e| e.into_inner());
        if *current == statements {
            return Ok(());
        }
        let file = CatalogFile {
            statements: statements.clone(),
        };
        file.write(&self.path, &self.sidecar, &self.name)?;
        *current = statements;
        Ok(())
    }

    /// `location` resolved against the database's directory, if it is a
    /// relative local path; anything else (a URL, an absolute path, `~/…`) as
    /// given.
    pub fn resolve(&self, location: &str) -> String {
        if !is_relative_path(location) {
            return location.to_string();
        }
        self.dir.join(location).to_string_lossy().into_owned()
    }
}

impl Drop for Database {
    /// Finish writing stored indexes before the lock is released.
    fn drop(&mut self) {
        self.flush();
    }
}

/// Whether `location` is a relative local path (not a URL, not absolute, not
/// starting with `~`).
pub fn is_relative_path(location: &str) -> bool {
    if location.is_empty() || location.starts_with('~') {
        return false;
    }
    if let Ok(url) = url::Url::parse(location) {
        // A one-letter scheme is a Windows drive letter.
        if url.scheme().len() > 1 {
            return false;
        }
    }
    Path::new(location).is_relative()
}

/// Take the exclusive lock on `lock`, retrying until `timeout` has passed.
fn acquire(lock: &File, name: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                if start.elapsed() >= timeout {
                    return Err(DataFusionError::Execution(format!(
                        "{name} is in use by another process"
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

/// `location` as the session reads it: resolved against the database's
/// directory when the session has a database and `location` is a relative
/// path, otherwise unchanged.
pub fn resolve_location(state: &SessionState, location: &str) -> String {
    match database(state) {
        Some(db) => db.resolve(location),
        None => location.to_string(),
    }
}

#[cfg(test)]
mod tests;
