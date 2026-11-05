//! The catalog file: a versioned JSON list of the statements that defined
//! each object, in the order they are replayed.
//!
//! ```json
//! {
//!   "format": "ursa",
//!   "version": 1,
//!   "statements": [
//!     "CREATE VIEW ties AS SELECT * FROM 'karate_edges.csv'"
//!   ]
//! }
//! ```
//!
//! `version` changes only when the file's shape does. A file with a newer
//! version is refused; anything that is not this shape is reported as not a
//! valid database. An empty file (`touch club.ursa`) is an empty catalog.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use datafusion::error::{DataFusionError, Result};
use serde_json::Value;

/// The `format` field of every catalog file.
pub const FORMAT: &str = "ursa";
/// The catalog file version this build reads and writes.
pub const VERSION: u64 = 1;

/// The contents of a catalog file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogFile {
    pub statements: Vec<String>,
}

impl CatalogFile {
    /// Parse a catalog file's text. `name` is how messages name the database.
    pub fn parse(text: &str, name: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let invalid = |why: String| {
            DataFusionError::Execution(format!("{name} is not a valid Ursa database: {why}"))
        };
        let value: Value =
            serde_json::from_str(text).map_err(|e| invalid(format!("it is not JSON ({e})")))?;
        let Value::Object(fields) = value else {
            return Err(invalid("it is not a JSON object".into()));
        };
        if fields.get("format").and_then(Value::as_str) != Some(FORMAT) {
            return Err(invalid(format!("its \"format\" is not \"{FORMAT}\"")));
        }
        let version = fields
            .get("version")
            .and_then(Value::as_u64)
            .filter(|v| *v >= 1)
            .ok_or_else(|| invalid("its \"version\" is not a positive integer".into()))?;
        if version > VERSION {
            return Err(DataFusionError::Execution(format!(
                "{name} was written by a newer version of Ursa (database format version \
                 {version}; this version of ursa reads version {VERSION}); upgrade ursa to \
                 open it"
            )));
        }
        let statements = fields
            .get("statements")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("it has no \"statements\" list".into()))?
            .iter()
            .map(|s| s.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| invalid("an entry of \"statements\" is not a string".into()))?;
        Ok(Self { statements })
    }

    /// Read and parse the catalog file at `path`.
    pub fn read(path: &Path, name: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| DataFusionError::Execution(format!("cannot read {name}: {e}")))?;
        Self::parse(&text, name)
    }

    /// The file's text: `format`, `version`, then one statement per line,
    /// with a trailing newline.
    pub fn to_text(&self) -> String {
        let quote = |s: &str| Value::String(s.to_string()).to_string();
        let mut text = format!(
            "{{\n  \"format\": {},\n  \"version\": {VERSION},\n  \"statements\": [",
            quote(FORMAT)
        );
        for (i, statement) in self.statements.iter().enumerate() {
            text.push_str(if i == 0 { "\n    " } else { ",\n    " });
            text.push_str(&quote(statement));
        }
        if !self.statements.is_empty() {
            text.push_str("\n  ");
        }
        text.push_str("]\n}\n");
        text
    }

    /// Write the file at `path` atomically: write a temporary file in
    /// `scratch_dir` (on the same filesystem), `fsync` it, rename it over
    /// `path`, then `fsync` the directory.
    pub fn write(&self, path: &Path, scratch_dir: &Path, name: &str) -> Result<()> {
        let failed =
            |e: std::io::Error| DataFusionError::Execution(format!("cannot save {name}: {e}"));
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "catalog".into());
        let temp = scratch_dir.join(format!("{file_name}.tmp"));
        {
            let mut file = File::create(&temp).map_err(failed)?;
            file.write_all(self.to_text().as_bytes()).map_err(failed)?;
            file.sync_all().map_err(failed)?;
        }
        std::fs::rename(&temp, path).map_err(failed)?;
        sync_dir(path.parent().unwrap_or(Path::new(".")));
        Ok(())
    }
}

/// `fsync` a directory so a rename in it is durable. Best effort: not every
/// platform can open a directory as a file.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}
