//! Ursa's session settings, set with `SET ursa.<name> = …` and shown with
//! `SHOW ursa.<name>`: a DataFusion config extension with the prefix `ursa`.
//!
//! Settings last for the session, with one exception: in a writable `.ursa`
//! database, `SET ursa.index_storage_limit` (and `RESET`) is stored in the
//! catalog file and applied again when the database is opened, because it
//! governs what the database keeps on disk (#327). The database's stored
//! indexes obey the stored limit (or the default), never a session's own: a
//! read-only or in-memory session's `SET` changes only what `SHOW` says.

use std::fmt;

use datafusion::common::extensions_options;
use datafusion::config::{ConfigExtension, ConfigField, Visit};
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionConfig;

/// The default `ursa.index_storage_limit`: 4 GiB.
pub const DEFAULT_INDEX_STORAGE_LIMIT: u64 = 4 << 30;

/// A number of bytes, written as plain bytes (`1048576`) or with a binary
/// unit (`512KiB`, `64MiB`, `8GiB`, `1TiB`; `B` for bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

const UNITS: [(&str, u64); 5] = [
    ("TiB", 1 << 40),
    ("GiB", 1 << 30),
    ("MiB", 1 << 20),
    ("KiB", 1 << 10),
    ("B", 1),
];

impl ByteSize {
    /// Parse `512KiB`, `8 GiB`, `1.5GiB` or `1048576`. Units are binary and
    /// matched without regard to case.
    pub fn parse(text: &str) -> Result<Self> {
        let invalid = || {
            DataFusionError::Configuration(format!(
                "'{text}' is not a size; write bytes (1048576) or a number with KiB, MiB, GiB \
                 or TiB (8GiB)"
            ))
        };
        let text = text.trim();
        let split = text
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(text.len());
        let (number, unit) = (text[..split].trim(), text[split..].trim());
        let scale = match unit {
            "" => 1,
            unit => {
                UNITS
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(unit))
                    .ok_or_else(invalid)?
                    .1
            }
        };
        if let Ok(n) = number.parse::<u64>() {
            return n.checked_mul(scale).map(ByteSize).ok_or_else(invalid);
        }
        let n: f64 = number.parse().map_err(|_| invalid())?;
        let bytes = n * scale as f64;
        if !bytes.is_finite() || bytes < 0.0 || bytes >= u64::MAX as f64 {
            return Err(invalid());
        }
        Ok(ByteSize(bytes as u64))
    }
}

impl fmt::Display for ByteSize {
    /// In the largest binary unit that divides it, as `SHOW` prints it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (name, scale) = UNITS
            .iter()
            .find(|(_, scale)| self.0 != 0 && self.0.is_multiple_of(*scale))
            .copied()
            .unwrap_or(("B", 1));
        if name == "B" {
            write!(f, "{}", self.0)
        } else {
            write!(f, "{}{name}", self.0 / scale)
        }
    }
}

impl ConfigField for ByteSize {
    fn visit<V: Visit>(&self, v: &mut V, key: &str, description: &'static str) {
        v.some(key, self, description)
    }

    fn set(&mut self, _key: &str, value: &str) -> Result<()> {
        *self = ByteSize::parse(value)?;
        Ok(())
    }
}

/// Which notices a session prints (`ursa.notices`): one line each, to the
/// sink the host installed (`crate::notices`; the CLI's is stderr).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum NoticeLevel {
    /// None.
    Off,
    /// Index builds that take more than a second, and every stored-index
    /// decision that loses work: an index not stored, an index evicted, a
    /// failed write.
    #[default]
    On,
    /// Every index build, load and store, however quick.
    All,
}

impl NoticeLevel {
    /// `on`, `off` or `all` (`true` and `false` too), in any case.
    pub fn parse(text: &str) -> Result<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "false" => Ok(Self::Off),
            "on" | "true" => Ok(Self::On),
            "all" => Ok(Self::All),
            _ => Err(DataFusionError::Configuration(format!(
                "'{text}' is not a notice level; use 'on', 'off' or 'all'"
            ))),
        }
    }
}

impl fmt::Display for NoticeLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::On => "on",
            Self::All => "all",
        })
    }
}

impl ConfigField for NoticeLevel {
    fn visit<V: Visit>(&self, v: &mut V, key: &str, description: &'static str) {
        v.some(key, self, description)
    }

    fn set(&mut self, _key: &str, value: &str) -> Result<()> {
        *self = NoticeLevel::parse(value)?;
        Ok(())
    }
}

extensions_options! {
    /// Ursa's settings (`SET ursa.<name> = …`).
    pub struct UrsaOptions {
        /// The most bytes of graph indexes a `.ursa` database keeps on disk.
        /// Past it, the least recently used are removed to make room for a
        /// new one. Default 4GiB. SET in a writable database, it is stored
        /// in the database.
        pub index_storage_limit: ByteSize, default = ByteSize(DEFAULT_INDEX_STORAGE_LIMIT)
        /// Which notices to print on stderr: 'on' (slow index builds, and
        /// indexes not stored or evicted), 'all' (every index build, load
        /// and store) or 'off'. Default on.
        pub notices: NoticeLevel, default = NoticeLevel::On
    }
}

impl ConfigExtension for UrsaOptions {
    const PREFIX: &'static str = "ursa";
}

/// `config` with Ursa's settings, unless it has them.
pub(crate) fn with_options(mut config: SessionConfig) -> SessionConfig {
    if config.options().extensions.get::<UrsaOptions>().is_none() {
        config
            .options_mut()
            .extensions
            .insert(UrsaOptions::default());
    }
    config
}

/// The session's `ursa.notices`.
pub fn notice_level(config: &SessionConfig) -> NoticeLevel {
    config
        .options()
        .extensions
        .get::<UrsaOptions>()
        .map_or(NoticeLevel::default(), |o| o.notices)
}

/// The session's `ursa.index_storage_limit`, in bytes: what `SHOW` says. The
/// database's stored indexes obey the database's limit
/// (`Database::index_storage_limit`), which the SQL layer keeps in step with
/// this in a writable database.
pub fn index_storage_limit(config: &SessionConfig) -> u64 {
    config
        .options()
        .extensions
        .get::<UrsaOptions>()
        .map_or(DEFAULT_INDEX_STORAGE_LIMIT, |o| o.index_storage_limit.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_and_print() {
        for (text, bytes) in [
            ("0", 0),
            ("1048576", 1 << 20),
            ("512KiB", 512 << 10),
            ("64 MiB", 64 << 20),
            ("8GiB", 8 << 30),
            ("8gib", 8 << 30),
            ("1.5GiB", 3 << 29),
            ("2TiB", 2 << 40),
            ("100B", 100),
        ] {
            assert_eq!(ByteSize::parse(text).unwrap(), ByteSize(bytes), "{text}");
        }
        for bad in [
            "",
            "GiB",
            "8GB",
            "-1",
            "8 gigabytes",
            "1e3",
            "99999999999TiB",
        ] {
            assert!(ByteSize::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(ByteSize(4 << 30).to_string(), "4GiB");
        assert_eq!(ByteSize(1536 << 20).to_string(), "1536MiB");
        assert_eq!(ByteSize(1000).to_string(), "1000");
        assert_eq!(ByteSize(0).to_string(), "0");
    }
}
