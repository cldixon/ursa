//! Ursa's session settings, set with `SET ursa.<name> = …` and shown with
//! `SHOW ursa.<name>`: a DataFusion config extension with the prefix `ursa`.
//!
//! Settings last for the session, as in DuckDB and Postgres (#353): `SET` and
//! `RESET` change only the current session, in any database, and nothing is
//! written to the catalog. To keep a setting, put the `SET` in `~/.ursarc` or
//! an `-init` file. (Ursa 0.8 and 0.9 stored `ursa.index_storage_limit` in a
//! writable database's catalog; such an entry is no longer applied, see
//! `ursa-sql`'s `database` module.)
//!
//! `ursa.index_storage_limit` is `auto` unless `SET`: a database then keeps
//! at most [`default_index_storage_limit`], a share of the free space on its
//! disk.

use std::fmt;

use datafusion::common::extensions_options;
use datafusion::config::{ConfigExtension, ConfigField, Visit};
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionConfig;

/// The least the default `ursa.index_storage_limit` is: 4 GiB, whatever the
/// free space (the default of Ursa 0.8 and 0.9).
pub const MIN_DEFAULT_INDEX_STORAGE_LIMIT: u64 = 4 << 30;
/// The most the default `ursa.index_storage_limit` is: 64 GiB.
pub const MAX_DEFAULT_INDEX_STORAGE_LIMIT: u64 = 64 << 30;
/// The share of the space available to indexes that the default
/// `ursa.index_storage_limit` takes, in percent.
pub const DEFAULT_INDEX_STORAGE_SHARE: u64 = 25;

/// The default `ursa.index_storage_limit` of a database whose indexes could
/// use `available` bytes (the free space on its disk plus what its stored
/// indexes already take; `None` if the free space cannot be read): 25% of
/// it, in whole GiB, at least 4 GiB and at most 64 GiB (#353).
///
/// Relative, so a large disk keeps the indexes of several large graphs (an
/// index of 95.7M edges takes about 1.3 GiB, and 2.3 GiB once an algorithm
/// adds its reverse adjacency) instead of evicting one to store another; and
/// a share, so indexes, which are a cache, never take most of a disk: three
/// quarters of it stays free for everything else. Capped, so a vast disk does
/// not keep an unbounded cache; and at least the 4 GiB earlier versions kept,
/// so no database keeps less than it did. The stored indexes count as
/// available, so storing them does not shrink the limit at the next open.
pub fn default_index_storage_limit(available: Option<u64>) -> u64 {
    let Some(available) = available else {
        return MIN_DEFAULT_INDEX_STORAGE_LIMIT;
    };
    let share = (available as u128 * DEFAULT_INDEX_STORAGE_SHARE as u128 / 100) as u64;
    (share >> 30 << 30).clamp(
        MIN_DEFAULT_INDEX_STORAGE_LIMIT,
        MAX_DEFAULT_INDEX_STORAGE_LIMIT,
    )
}

/// `ursa.index_storage_limit`: `auto` (the default, see
/// [`default_index_storage_limit`]) or a size `SET` for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexStorageLimit {
    /// The database's default, relative to the free space on its disk.
    #[default]
    Auto,
    /// A size `SET` for the session.
    Bytes(ByteSize),
}

impl IndexStorageLimit {
    /// `auto` (or `default`), in any case, or a size ([`ByteSize::parse`]).
    pub fn parse(text: &str) -> Result<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" | "default" => Ok(Self::Auto),
            _ => ByteSize::parse(text).map(Self::Bytes),
        }
    }

    /// The size `SET`, if one was.
    pub fn bytes(&self) -> Option<u64> {
        match self {
            Self::Auto => None,
            Self::Bytes(b) => Some(b.0),
        }
    }
}

impl fmt::Display for IndexStorageLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Bytes(b) => b.fmt(f),
        }
    }
}

impl ConfigField for IndexStorageLimit {
    fn visit<V: Visit>(&self, v: &mut V, key: &str, description: &'static str) {
        v.some(key, self, description)
    }

    fn set(&mut self, _key: &str, value: &str) -> Result<()> {
        *self = IndexStorageLimit::parse(value)?;
        Ok(())
    }
}

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
        /// The most bytes of graph indexes a `.ursa` database keeps on disk:
        /// a new index is stored only within it, removing the least recently
        /// used to make room. Lowering it removes nothing. Default auto: 25%
        /// of the free space on the database's disk (counting its stored
        /// indexes), at least 4GiB and at most 64GiB. Lasts for the session.
        pub index_storage_limit: IndexStorageLimit, default = IndexStorageLimit::Auto
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

/// The session's `ursa.index_storage_limit`, if it was `SET`; `None` when it
/// is `auto`, the database's default (`Database::index_storage_limit` gives
/// the limit in effect).
pub fn index_storage_limit_set(config: &SessionConfig) -> Option<u64> {
    config
        .options()
        .extensions
        .get::<UrsaOptions>()
        .and_then(|o| o.index_storage_limit.bytes())
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

    #[test]
    fn the_limit_is_auto_or_a_size() {
        assert_eq!(IndexStorageLimit::default().to_string(), "auto");
        for auto in ["auto", "AUTO", "default"] {
            assert_eq!(
                IndexStorageLimit::parse(auto).unwrap(),
                IndexStorageLimit::Auto
            );
        }
        let set = IndexStorageLimit::parse("50MiB").unwrap();
        assert_eq!(set.to_string(), "50MiB");
        assert_eq!(set.bytes(), Some(50 << 20));
        assert!(IndexStorageLimit::parse("lots").is_err());
    }

    #[test]
    fn the_default_limit_is_a_quarter_of_the_space_within_bounds() {
        let gib = 1u64 << 30;
        assert_eq!(default_index_storage_limit(None), 4 * gib);
        assert_eq!(default_index_storage_limit(Some(0)), 4 * gib);
        assert_eq!(default_index_storage_limit(Some(10 * gib)), 4 * gib);
        // 25%, in whole GiB.
        assert_eq!(default_index_storage_limit(Some(100 * gib)), 25 * gib);
        assert_eq!(default_index_storage_limit(Some(103 * gib)), 25 * gib);
        assert_eq!(default_index_storage_limit(Some(1 << 50)), 64 * gib);
        assert_eq!(default_index_storage_limit(Some(u64::MAX)), 64 * gib);
    }
}
