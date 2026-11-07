//! Where the engine's notices go: stderr, one line each, as `ursa: …`.
//!
//! Notices say what the engine does on its own that a user waits for or
//! loses (`ursa_engine::notices`): a slow graph index build, an index not
//! stored or evicted from the database. They go to stderr whether or not it
//! is a terminal, so stdout carries only results (a pipe, `--format json`,
//! an agent reading it), and an agent reading stderr learns why a query was
//! slow. `--quiet` or `SET ursa.notices = 'off'` turns them off; `SET
//! ursa.notices = 'all'` says every build, load and store.

use std::io::Write;
use std::sync::Arc;

use datafusion::prelude::SessionContext;

/// Send `ctx`'s notices to stderr.
pub fn to_stderr(ctx: &SessionContext) {
    ursa_engine::notices::set_sink(
        &ctx.state(),
        Some(Arc::new(|line: &str| {
            // A closed stderr is no reason to fail a query.
            let _ = writeln!(std::io::stderr(), "ursa: {line}");
        })),
    );
}
