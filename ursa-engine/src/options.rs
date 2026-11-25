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
/// The default `ursa.index_storage_limit` is a multiple of this, 4 GiB, so it
/// does not move between opens as the free space changes a little (#381).
pub const DEFAULT_INDEX_STORAGE_STEP: u64 = 4 << 30;
/// The share of the space available to indexes that the default
/// `ursa.index_storage_limit` takes, in percent.
pub const DEFAULT_INDEX_STORAGE_SHARE: u64 = 25;

/// The default `ursa.index_storage_limit` of a database whose indexes could
/// use `available` bytes (the free space on its disk plus what its stored
/// indexes already take; `None` if the free space cannot be read): 25% of
/// it, rounded down to a multiple of 4 GiB, at least 4 GiB and at most 64 GiB
/// (#353, #381).
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
    (share / DEFAULT_INDEX_STORAGE_STEP * DEFAULT_INDEX_STORAGE_STEP).clamp(
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

/// `ursa.build_read_ahead`: `auto` (the default: 256 MiB per partition of
/// the plan read, at most 1 GiB and at most an eighth of the memory
/// available) or a size `SET` for the session; 0 reads a build's partitions
/// one after another (#385).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadAhead {
    /// Scaled with the partitions read, and capped by the memory available.
    #[default]
    Auto,
    /// A size `SET` for the session.
    Bytes(ByteSize),
}

impl ReadAhead {
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

impl fmt::Display for ReadAhead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Bytes(b) => b.fmt(f),
        }
    }
}

impl ConfigField for ReadAhead {
    fn visit<V: Visit>(&self, v: &mut V, key: &str, description: &'static str) {
        v.some(key, self, description)
    }

    fn set(&mut self, _key: &str, value: &str) -> Result<()> {
        *self = ReadAhead::parse(value)?;
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

/// A limit `SET` as a whole number, 0 for no limit (`ursa.max_paths`,
/// `ursa.graph_max_edges`): `name` is the setting, for the message when
/// `text` is not one, in the style of the other settings' messages (rather
/// than DataFusion's "Error parsing '-5' as usize").
pub fn parse_limit(name: &str, text: &str) -> Result<usize> {
    text.trim().parse::<usize>().map_err(|_| {
        DataFusionError::Configuration(format!(
            "{name} must be a non-negative integer (0 for no limit); got {text}"
        ))
    })
}

/// A setting that is a limit: a whole number, 0 for no limit, parsed with
/// [`parse_limit`] so a bad value names the setting.
macro_rules! limit_setting {
    ($(#[$doc:meta])* $ty:ident, $name:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $ty(pub usize);

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl ConfigField for $ty {
            fn visit<V: Visit>(&self, v: &mut V, key: &str, description: &'static str) {
                v.some(key, self, description)
            }

            fn set(&mut self, _key: &str, value: &str) -> Result<()> {
                self.0 = parse_limit($name, value)?;
                Ok(())
            }
        }
    };
}

limit_setting!(
    /// `ursa.max_paths`: the most paths one path search holds.
    MaxPaths,
    "ursa.max_paths"
);
limit_setting!(
    /// `ursa.graph_max_edges`: the most edges the graph view draws.
    GraphMaxEdges,
    "ursa.graph_max_edges"
);

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
        /// The most paths one GRAPH_TABLE path search holds before it stops
        /// with an error; 0 for no limit. Under ANY SHORTEST (and SHORTEST
        /// k) it counts the paths kept, never fewer than one (k) per node of
        /// the graph; otherwise the paths and prefixes held. Default
        /// 10000000.
        pub max_paths: MaxPaths, default = MaxPaths(crate::graph::path_search::MAX_PATHS)
        /// The most bytes of decoded rows an index build reads ahead of the
        /// rows it is interning, so the partitions of its edge and vertex
        /// tables decode in parallel.
        /// Default auto: 256MiB per partition read, at most 1GiB and at most
        /// an eighth of the memory available. 0 reads the partitions one
        /// after another.
        pub build_read_ahead: ReadAhead, default = ReadAhead::Auto
        /// The most edges the graph view draws (`.graph`): a result or a
        /// graph with more is refused, with a message that names this
        /// setting. 0 for no limit. Default 5000000, which a desktop
        /// browser draws; the web shell sets 500000 on a phone.
        pub graph_max_edges: GraphMaxEdges, default = GraphMaxEdges(crate::graph::view::MAX_EDGES)
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

/// The session's `ursa.max_paths`: `None` when it is 0, no limit.
pub fn max_paths(config: &SessionConfig) -> Option<usize> {
    let n = config
        .options()
        .extensions
        .get::<UrsaOptions>()
        .map_or(crate::graph::path_search::MAX_PATHS, |o| o.max_paths.0);
    (n > 0).then_some(n)
}

/// The session's `ursa.graph_max_edges`: the most edges the graph view
/// draws; `usize::MAX` when it is 0, no limit.
pub fn graph_max_edges(config: &SessionConfig) -> usize {
    let n = config
        .options()
        .extensions
        .get::<UrsaOptions>()
        .map_or(crate::graph::view::MAX_EDGES, |o| o.graph_max_edges.0);
    if n == 0 {
        usize::MAX
    } else {
        n
    }
}

/// The session's `ursa.build_read_ahead`, if it was `SET`; `None` when it
/// is `auto` (see `graph::in_order`).
pub fn build_read_ahead_set(config: &SessionConfig) -> Option<u64> {
    config
        .options()
        .extensions
        .get::<UrsaOptions>()
        .and_then(|o| o.build_read_ahead.bytes())
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
    fn limits_parse_or_name_their_setting() {
        assert_eq!(parse_limit("ursa.max_paths", "1000").unwrap(), 1000);
        assert_eq!(parse_limit("ursa.max_paths", " 0 ").unwrap(), 0);
        for bad in ["-5", "lots", "1.5", ""] {
            let e = parse_limit("ursa.max_paths", bad).unwrap_err().to_string();
            assert!(
                e.ends_with(&format!(
                    "ursa.max_paths must be a non-negative integer (0 for no limit); got {bad}"
                )),
                "{e}"
            );
        }
    }

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
        // 25%, rounded down to a multiple of 4 GiB.
        assert_eq!(default_index_storage_limit(Some(100 * gib)), 24 * gib);
        assert_eq!(default_index_storage_limit(Some(103 * gib)), 24 * gib);
        assert_eq!(default_index_storage_limit(Some(87 * gib)), 20 * gib);
        assert_eq!(default_index_storage_limit(Some(95 * gib)), 20 * gib);
        assert_eq!(default_index_storage_limit(Some(96 * gib)), 24 * gib);
        assert_eq!(default_index_storage_limit(Some(1 << 50)), 64 * gib);
        assert_eq!(default_index_storage_limit(Some(u64::MAX)), 64 * gib);
    }
}
