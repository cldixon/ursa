//! Ctrl-C (SIGINT) during a script: once it has cancelled the run, nothing
//! more reaches stdout.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once SIGINT has cancelled a script (see `cancel_on_interrupt` in
/// `main.rs`).
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Stop [`Interruptible`] writers from writing anything more.
pub fn interrupt() {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// A writer that drops everything once [`interrupt`] is called, so a script
/// cancelled with Ctrl-C prints nothing more: not the rest of a result, and
/// not what is still buffered in front of this writer.
pub struct Interruptible<W>(pub W);

impl<W: Write> Write for Interruptible<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Ok(buf.len());
        }
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if INTERRUPTED.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.0.flush()
    }
}
