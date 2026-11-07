//! Notices: one-line messages about work the engine does on its own, such as
//! building a graph index or deciding what a database keeps on disk (#327).
//!
//! A notice is never part of a result. The engine hands each one to the
//! session's sink, which the host installs ([`set_sink`]); with no sink,
//! nothing is said. The CLI's sink writes `ursa: <notice>` to stderr, so
//! stdout carries only results whoever reads it.
//!
//! Which notices are said is `ursa.notices` ([`NoticeLevel`]):
//!
//! - `on` (the default): a build that has run for a second announces itself
//!   (`building the index for g (95.7M edges)…`) and says when it is done and
//!   what became of it (`built the index for g in 30.7 s; stored it in
//!   club.ursa (1.4 GiB)`); and every stored-index decision that loses work is
//!   said, however quick: an index not stored (too large for
//!   `ursa.index_storage_limit`, or no room beside the indexes in use), an
//!   index evicted to make room, a write that failed, and stored indexes
//!   dropped because another version of Ursa wrote them. A quick build that
//!   is stored, or is in memory only, says nothing.
//! - `all`: also every build, load and store, however quick.
//! - `off`: nothing.
//!
//! A notice decided on a background thread (a failed write, a chunk appended
//! when the database closes) goes to the sink of the session that started the
//! work, at the level it had then ([`Notifier`]).

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use datafusion::execution::context::SessionState;
use datafusion::prelude::SessionConfig;

pub use crate::options::NoticeLevel;

/// Where a session's notices go: called with each notice, one line, without
/// a trailing newline.
pub type Sink = Arc<dyn Fn(&str) + Send + Sync>;

/// How long a build runs before it announces itself, at `ursa.notices = on`.
pub const BUILD_NOTICE_AFTER: Duration = Duration::from_secs(1);

/// The session's sink, set by the host (a session config extension, installed
/// by `new_session`).
#[derive(Default)]
pub struct NoticeSlot {
    sink: Mutex<Option<Sink>>,
}

impl fmt::Debug for NoticeSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = self.sink.lock().map(|s| s.is_some()).unwrap_or(false);
        write!(f, "NoticeSlot(sink: {set})")
    }
}

/// Install an empty notice slot on `config` (done by `new_session`).
pub(crate) fn with_notice_slot(config: SessionConfig) -> SessionConfig {
    config.with_extension(Arc::new(NoticeSlot::default()))
}

/// Send the session's notices to `sink` (`None`: say nothing).
pub fn set_sink(state: &SessionState, sink: Option<Sink>) {
    if let Some(slot) = state.config().get_extension::<NoticeSlot>() {
        *slot.sink.lock().unwrap_or_else(|e| e.into_inner()) = sink;
    }
}

/// A session's sink and notice level, as they were when it was taken: what
/// background work says its notices with.
#[derive(Clone, Default)]
pub struct Notifier {
    sink: Option<Sink>,
    level: NoticeLevel,
}

impl fmt::Debug for Notifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Notifier({}, sink: {})", self.level, self.sink.is_some())
    }
}

impl Notifier {
    /// The notifier of the session `config` belongs to.
    pub fn of(config: &SessionConfig) -> Self {
        let sink = config
            .get_extension::<NoticeSlot>()
            .and_then(|slot| slot.sink.lock().unwrap_or_else(|e| e.into_inner()).clone());
        Self {
            level: if sink.is_some() {
                crate::options::notice_level(config)
            } else {
                NoticeLevel::Off
            },
            sink,
        }
    }

    /// A notifier that says nothing.
    pub fn silent() -> Self {
        Self::default()
    }

    /// One that says notices at `level` to `sink` (for tests and hosts
    /// without a session).
    pub fn new(sink: Sink, level: NoticeLevel) -> Self {
        Self {
            sink: Some(sink),
            level,
        }
    }

    /// The level notices are said at: `Off` without a sink.
    pub fn level(&self) -> NoticeLevel {
        if self.sink.is_some() {
            self.level
        } else {
            NoticeLevel::Off
        }
    }

    /// Say `text` if the level is at least `level`.
    pub fn say(&self, level: NoticeLevel, text: impl FnOnce() -> String) {
        if level == NoticeLevel::Off || self.level() < level {
            return;
        }
        if let Some(sink) = &self.sink {
            sink(&text());
        }
    }

    /// Announce `text` once `after` has passed, unless [`Announcement::finish`]
    /// is called first; at once if `after` is zero.
    pub fn announce_after(&self, after: Duration, text: String) -> Announcement {
        let state = Arc::new(AnnounceState::default());
        let Some(sink) = self
            .sink
            .clone()
            .filter(|_| self.level() >= NoticeLevel::On)
        else {
            return Announcement { state };
        };
        if after.is_zero() {
            sink(&text);
            state.lock().said = true;
            return Announcement { state };
        }
        // Native only: the browser has one thread, and no sink.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let waiting = Arc::clone(&state);
            let _ = std::thread::Builder::new()
                .name("ursa-notice".into())
                .spawn(move || {
                    let guard = waiting.lock();
                    let (mut guard, _) = waiting
                        .done
                        .wait_timeout_while(guard, after, |s| !s.finished)
                        .unwrap_or_else(|e| e.into_inner());
                    if !guard.finished {
                        // Said under the lock, so it comes before what
                        // `finish`'s caller says next.
                        sink(&text);
                        guard.said = true;
                    }
                });
        }
        #[cfg(target_arch = "wasm32")]
        let _ = (sink, text);
        Announcement { state }
    }
}

/// A notice that is said if the work it announces is still running when its
/// time comes (see [`Notifier::announce_after`]).
#[derive(Debug)]
pub struct Announcement {
    state: Arc<AnnounceState>,
}

#[derive(Debug, Default)]
struct AnnounceState {
    flags: Mutex<Flags>,
    done: std::sync::Condvar,
}

#[derive(Debug, Default)]
struct Flags {
    finished: bool,
    said: bool,
}

impl AnnounceState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Flags> {
        self.flags.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Announcement {
    /// The work is done: the notice is not said now if it has not been.
    /// Returns whether it was said.
    pub fn finish(&self) -> bool {
        let mut flags = self.state.lock();
        flags.finished = true;
        self.state.done.notify_all();
        flags.said
    }
}

impl Drop for Announcement {
    fn drop(&mut self) {
        self.finish();
    }
}

/// `bytes` for people, in binary units with one decimal: `1.4 GiB`, `512 B`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `n` for people: `950`, `12.3K`, `95.7M`, `1.2B`.
pub fn count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}K", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.1}B", n as f64 / 1e9),
    }
}

/// `duration` for people: `0.4 s`, `30.7 s`, `4 min 5 s`.
pub fn duration(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 120.0 {
        format!("{seconds:.1} s")
    } else {
        let whole = duration.as_secs();
        format!("{} min {} s", whole / 60, whole % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collecting(level: NoticeLevel) -> (Notifier, Arc<Mutex<Vec<String>>>) {
        let said = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let said = Arc::clone(&said);
            Arc::new(move |line: &str| said.lock().unwrap().push(line.to_string())) as Sink
        };
        (Notifier::new(sink, level), said)
    }

    #[test]
    fn levels_filter_what_is_said() {
        let (on, said) = collecting(NoticeLevel::On);
        on.say(NoticeLevel::On, || "kept".into());
        on.say(NoticeLevel::All, || "detail".into());
        assert_eq!(*said.lock().unwrap(), ["kept"]);
        let (all, said) = collecting(NoticeLevel::All);
        all.say(NoticeLevel::On, || "kept".into());
        all.say(NoticeLevel::All, || "detail".into());
        assert_eq!(*said.lock().unwrap(), ["kept", "detail"]);
        let (off, said) = collecting(NoticeLevel::Off);
        off.say(NoticeLevel::On, || "kept".into());
        assert!(said.lock().unwrap().is_empty());
        assert_eq!(Notifier::silent().level(), NoticeLevel::Off);
    }

    #[test]
    fn an_announcement_is_said_only_if_the_work_outlasts_its_delay() {
        let (on, said) = collecting(NoticeLevel::On);
        let quick = on.announce_after(Duration::from_secs(60), "quick".into());
        assert!(!quick.finish());
        let slow = on.announce_after(Duration::from_millis(1), "slow".into());
        let started = std::time::Instant::now();
        while said.lock().unwrap().is_empty() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(slow.finish());
        assert!(on.announce_after(Duration::ZERO, "now".into()).finish());
        assert_eq!(*said.lock().unwrap(), ["slow", "now"]);
        let (off, said) = collecting(NoticeLevel::Off);
        assert!(!off.announce_after(Duration::ZERO, "no".into()).finish());
        assert!(said.lock().unwrap().is_empty());
    }

    #[test]
    fn sizes_counts_and_durations_read_well() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(1_503_238_554), "1.4 GiB");
        assert_eq!(count(78), "78");
        assert_eq!(count(95_700_000), "95.7M");
        assert_eq!(count(12_345), "12.3K");
        assert_eq!(duration(Duration::from_millis(30_700)), "30.7 s");
        assert_eq!(duration(Duration::from_secs(245)), "4 min 5 s");
    }
}
