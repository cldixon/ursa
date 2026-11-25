//! The session actor (`design/console.md` §3): a thread of its own that owns
//! a [`Session`] and runs its requests one at a time, in the order they came.
//!
//! `Shell` is built on single-threaded state (`Rc`, `RefCell`), so the
//! session never moves between threads. Its thread drives it on the server's
//! multi-threaded runtime (`Handle::block_on`), inside a `LocalSet`: what
//! the engine spawns (DataFusion's partitions) runs on the runtime's
//! workers, in parallel, and only the session's own bookkeeping stays here.

use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use ursa_shell::session::{Error, Options, Reply, Session};

use crate::Config;

/// A request for the actor, and where its reply goes.
pub(crate) struct Job {
    pub(crate) op: String,
    pub(crate) args: Value,
    pub(crate) parts: Vec<Vec<u8>>,
    pub(crate) reply: oneshot::Sender<Result<Reply, Error>>,
}

/// What the server's sessions are.
pub(crate) fn options(config: &Config) -> Options {
    Options {
        version: config.version.clone(),
        transport: "native",
        capabilities: vec!["uploads", "query", "arrow"],
        // The shell's page keeps imports and `COPY … TO 'name'` in memory,
        // as the browser demo does (the e2e suite runs both alike).
        uploads: true,
    }
}

/// Start a session on a thread of its own. It ends when the sender is
/// dropped (the connection closed). A panic in the engine ends it too: the
/// requests waiting then get no reply, which the connection reports as a
/// crash.
pub(crate) fn spawn(options: Options) -> std::io::Result<mpsc::Sender<Job>> {
    let (send, mut jobs) = mpsc::channel::<Job>(16);
    let runtime = Handle::current();
    std::thread::Builder::new()
        .name("ursa-session".into())
        .spawn(move || {
            let local = tokio::task::LocalSet::new();
            runtime.block_on(local.run_until(async move {
                let mut session = Session::new(options);
                while let Some(job) = jobs.recv().await {
                    let outcome = session.handle(&job.op, job.args, job.parts).await;
                    // The connection may have closed meanwhile.
                    let _ = job.reply.send(outcome);
                }
            }));
        })?;
    Ok(send)
}
