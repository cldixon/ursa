//! Sessions (`design/console.md` §3): each is an actor, a thread of its own
//! that owns a [`Session`] and runs its requests one at a time, in the order
//! they came, from every connection attached to it.
//!
//! `Shell` is built on single-threaded state (`Rc`, `RefCell`), so a session
//! never moves between threads. Its thread drives it on the engine's
//! multi-threaded runtime (`Handle::block_on`), inside a `LocalSet`: what the
//! engine spawns (DataFusion's partitions) runs on that runtime's workers, in
//! parallel, and only the session's own bookkeeping stays on its thread. The
//! engine's runtime is not the one the server's sockets run on, so a query
//! that keeps every core busy does not keep the server from reading a
//! `cancel`.
//!
//! **Cancelling** a request (§4.3's `cancel`) drops its future, which is
//! DataFusion's cancellation: the streams it was polling stop, and the tasks
//! they spawned are aborted. The shell is built for that (its Ctrl+C does the
//! same): a statement is taken from what was typed before it runs, so a
//! cancelled one leaves nothing behind. A graph kernel that runs without
//! yielding finishes first; its result is then dropped.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch, Notify};
use ursa_shell::session::{Error, Options, Reply, Root, Session};

use crate::Config;

/// What the server's sessions are: rooted in the served directory (relative
/// paths in their queries, `.read` and `ATTACH` resolve under it), with
/// its database file, if it has one, and read-only if it is.
///
/// What `COPY … TO 'name'` writes depends on the session ([`Sessions`]):
/// the shell's page asks for a session of its own (`hello {session:
/// "new"}`), which keeps its copies in memory, at `memory://uploads/`, as
/// the browser demo does, so the page behaves alike on both (`.files` lists
/// them, and the e2e suite runs both); the shared session, the console's
/// and agents', writes them to disk under the root.
pub(crate) fn options(config: &Config) -> Options {
    Options {
        version: config.version.clone(),
        transport: "native",
        capabilities: vec!["cancel", "uploads"],
        // The files a page adds (`addFile`), and the shell's copies.
        uploads: true,
        root: Some(Root::Dir(config.root.clone())),
        copy_to_disk: false,
        database: config.database.clone(),
        read_only: config.read_only,
    }
}

/// A request for a session, and where its reply goes.
pub(crate) struct Job {
    pub(crate) op: String,
    pub(crate) args: Value,
    pub(crate) parts: Vec<Vec<u8>>,
    /// Becomes `true` (or its sender is dropped, when the connection closes)
    /// to cancel the request.
    pub(crate) cancel: watch::Receiver<bool>,
    pub(crate) reply: oneshot::Sender<Result<Reply, Error>>,
}

/// A session's address: where its requests go.
#[derive(Clone)]
pub(crate) struct SessionHandle {
    /// The session's id, which `hello` reports: `s1`, `s2`, …
    pub(crate) id: String,
    /// Whether it is the server's shared session.
    pub(crate) shared: bool,
    /// What the session said to its first `hello`: the shared session's
    /// later ones are answered with it, without waiting for its turn
    /// (`hello` bypasses the queue, §4.5).
    pub(crate) hello: Arc<OnceLock<Value>>,
    /// The session's root, which relative paths resolve against.
    pub(crate) root: Arc<str>,
    jobs: mpsc::UnboundedSender<Job>,
}

impl SessionHandle {
    /// Queue `job`. If the session has stopped (its engine panicked), the
    /// job is dropped, and its reply's receiver hears so.
    pub(crate) fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    /// The session has stopped.
    fn is_closed(&self) -> bool {
        self.jobs.is_closed()
    }
}

/// The server's sessions: it makes new ones, and keeps the shared one.
pub(crate) struct Sessions {
    options: Options,
    engine: EngineRuntime,
    shared: Mutex<Option<SessionHandle>>,
    next: AtomicU64,
    live: Arc<Live>,
}

impl Sessions {
    pub(crate) fn new(options: Options, live: Arc<Live>) -> std::io::Result<Sessions> {
        let engine = tokio::runtime::Builder::new_multi_thread()
            .thread_name("ursa-engine")
            .enable_all()
            .build()?;
        Ok(Sessions {
            options,
            engine: EngineRuntime(Some(engine)),
            shared: Mutex::new(None),
            next: AtomicU64::new(1),
            live,
        })
    }

    /// A new session, for one connection: it ends when that connection's
    /// handle is dropped.
    pub(crate) fn new_session(&self) -> std::io::Result<SessionHandle> {
        self.spawn(false)
    }

    /// The shared session, started on first use, and again if its engine
    /// stopped.
    pub(crate) fn shared(&self) -> std::io::Result<SessionHandle> {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        match &*shared {
            Some(handle) if !handle.is_closed() => Ok(handle.clone()),
            _ => {
                let handle = self.spawn(true)?;
                *shared = Some(handle.clone());
                Ok(handle)
            }
        }
    }

    /// Let go of the shared session: it ends once no connection holds it.
    pub(crate) fn close(&self) {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    fn spawn(&self, shared: bool) -> std::io::Result<SessionHandle> {
        let id = format!("s{}", self.next.fetch_add(1, Ordering::Relaxed));
        let (jobs, queue) = mpsc::unbounded_channel();
        let engine = self.engine.handle();
        // The shared session writes its copies to disk; one of a page's
        // own keeps them in memory ([`options`]).
        let options = Options {
            copy_to_disk: shared,
            ..self.options.clone()
        };
        spawn_actor(engine, options, queue, self.live.enter(), &id)?;
        let root = match &self.options.root {
            Some(Root::Dir(dir)) => dir.display().to_string(),
            Some(Root::Url(url)) => url.to_string(),
            None => String::new(),
        };
        Ok(SessionHandle {
            id,
            shared,
            hello: Arc::default(),
            root: root.into(),
            jobs,
        })
    }
}

/// Run a session on a thread of its own until every handle to it is
/// dropped. A panic in the engine ends it too: the requests waiting then
/// get no reply, which their connections report as a `crash`.
fn spawn_actor(
    runtime: Handle,
    options: Options,
    mut queue: mpsc::UnboundedReceiver<Job>,
    live: LiveGuard,
    id: &str,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(format!("ursa-session-{id}"))
        .spawn(move || {
            let _live = live;
            let local = tokio::task::LocalSet::new();
            runtime.block_on(local.run_until(async move {
                let mut session = Session::new(options);
                while let Some(job) = queue.recv().await {
                    let outcome = run(&mut session, job.op, job.args, job.parts, job.cancel).await;
                    // The connection may have closed meanwhile.
                    let _ = job.reply.send(outcome);
                }
            }));
        })?;
    Ok(())
}

/// Run one request, unless it is cancelled first.
async fn run(
    session: &mut Session,
    op: String,
    args: Value,
    parts: Vec<Vec<u8>>,
    mut cancel: watch::Receiver<bool>,
) -> Result<Reply, Error> {
    if *cancel.borrow() {
        return Err(cancelled());
    }
    tokio::select! {
        biased;
        // Cancelled, or the connection that sent it closed: no one wants
        // the reply.
        _ = cancel.wait_for(|c| *c) => Err(cancelled()),
        outcome = session.handle(&op, args, parts) => outcome,
    }
}

/// The reply to a cancelled request.
pub(crate) fn cancelled() -> Error {
    Error {
        kind: "cancelled",
        message: "cancelled".into(),
    }
}

/// The reply to a request whose session stopped.
pub(crate) fn crash() -> Error {
    Error {
        kind: "crash",
        message: "the engine stopped".into(),
    }
}

/// The runtime the sessions' engines run on. Dropped, it stops without
/// waiting for its tasks: it may be dropped inside the server's runtime.
struct EngineRuntime(Option<tokio::runtime::Runtime>);

impl EngineRuntime {
    fn handle(&self) -> Handle {
        self.0
            .as_ref()
            .expect("the engine's runtime")
            .handle()
            .clone()
    }
}

impl Drop for EngineRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// A count of what is still running (connections and sessions), for a
/// graceful shutdown to wait on.
#[derive(Default)]
pub(crate) struct Live {
    count: AtomicUsize,
    idle: Notify,
}

impl Live {
    /// Count one more until the guard is dropped.
    pub(crate) fn enter(self: &Arc<Self>) -> LiveGuard {
        self.count.fetch_add(1, Ordering::SeqCst);
        LiveGuard(Arc::clone(self))
    }

    /// Once nothing is running.
    pub(crate) async fn idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.count.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// One running thing, counted by [`Live`].
pub(crate) struct LiveGuard(Arc<Live>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}
