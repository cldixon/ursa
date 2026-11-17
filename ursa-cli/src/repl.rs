//! The terminal side of the shell: rustyline for line editing and history,
//! and Ctrl-C to cancel a running query.
//!
//! Adapted from datafusion-cli's `exec_from_repl` and `CliHelper`
//! (Apache-2.0), with Ursa's parser in place of `DFParser`: each line goes to
//! [`Shell::feed`], which knows when a statement is complete, so the prompt
//! can show a continuation while one is being typed. A complete statement,
//! all its lines together, is one history entry.

use std::borrow::Cow;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::Validator;
use rustyline::{Config, Context, Editor, Helper};

use ursa_shell::highlight;
use ursa_shell::shell::{Flow, Shell, COMMANDS};

/// Read lines from the terminal and feed them to `shell` until `.quit` or
/// Ctrl-D. History is read from and saved to `history`, if given. With
/// `color`, the prompt and what is typed are coloured as the browser shell
/// colours them ([`ursa_shell::highlight`]).
pub async fn run<O: Write, E: Write>(
    shell: &mut Shell<O, E>,
    history: Option<PathBuf>,
    color: bool,
) -> rustyline::Result<()> {
    let config = Config::builder()
        .max_history_size(10_000)?
        .history_ignore_dups(true)?
        .auto_add_history(false)
        .build();
    let mut rl: Editor<ShellHelper, FileHistory> = Editor::with_config(config)?;
    rl.set_helper(Some(ShellHelper {
        color,
        ..ShellHelper::default()
    }));
    if let Some(path) = &history {
        // A missing (or unreadable) history file is not an error.
        let _ = rl.load_history(path);
    }

    let interrupts = Interrupts::start();
    // The lines of the statement being typed, for its history entry.
    let mut entry = String::new();
    loop {
        // `memory U `, or `club U ` with club.ursa open; then spaces as wide.
        let prompt = if shell.is_pending() {
            shell.continuation()
        } else {
            shell.prompt()
        };
        // The line is coloured as the next of the statement typed so far.
        if let Some(helper) = rl.helper_mut() {
            helper.state = shell.highlight_state().clone();
        }
        match rl.readline(&prompt) {
            Ok(line) => {
                if !entry.is_empty() {
                    entry.push('\n');
                }
                entry.push_str(&line);
                let interrupted = interrupts.armed();
                let flow = tokio::select! {
                    flow = shell.feed(&line) => Some(flow),
                    _ = interrupted => None,
                };
                interrupts.disarm();
                // `Interrupted` is said by the signal thread, at once.
                let flow = flow.unwrap_or_else(|| {
                    shell.cancel_input();
                    Flow::Continue
                });
                if !shell.is_pending() {
                    let text = std::mem::take(&mut entry);
                    if !text.trim().is_empty() {
                        rl.add_history_entry(text)?;
                        if let Some(path) = &history {
                            save(&mut rl, path);
                        }
                    }
                }
                if flow == Flow::Exit {
                    break;
                }
            }
            // Ctrl-C at the prompt drops what was typed.
            Err(ReadlineError::Interrupted) => {
                shell.cancel_input();
                entry.clear();
            }
            // Ctrl-D.
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Ctrl-C while a statement runs (N39). The terminal is not in raw mode
/// then, so Ctrl-C is SIGINT; it is waited for on a thread of its own, as
/// `cancel_on_interrupt` in `main.rs` waits for it in a script, so a query
/// that keeps every runtime worker busy cannot delay it. That thread says
/// `Interrupted` at once and wakes the read loop, which drops the statement
/// and draws the prompt again.
///
/// At the prompt the line editor reads Ctrl-C as a key; a SIGINT that comes
/// while no statement runs is ignored.
struct Interrupts {
    /// Whether a statement is running.
    running: Arc<AtomicBool>,
    /// Woken on SIGINT while one is.
    notify: Arc<Notify>,
}

impl Interrupts {
    fn start() -> Self {
        let running = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let (r, n) = (Arc::clone(&running), Arc::clone(&notify));
        let (ready, listening) = std::sync::mpsc::channel();
        let waiting = std::thread::Builder::new()
            .name("ursa-sigint".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(async {
                    #[cfg(unix)]
                    let sigint =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt());
                    #[cfg(windows)]
                    let sigint = tokio::signal::windows::ctrl_c();
                    let _ = ready.send(());
                    let Ok(mut sigint) = sigint else {
                        return;
                    };
                    while sigint.recv().await.is_some() {
                        if r.load(Ordering::SeqCst) {
                            let mut err = std::io::stderr().lock();
                            // Over the `^C` the terminal echoed.
                            let start = if err.is_terminal() { "\r" } else { "" };
                            let _ = writeln!(err, "{start}{INTERRUPTED}");
                            let _ = err.flush();
                            n.notify_waiters();
                        }
                    }
                });
            });
        // Listening before the first line is read: the line editor sets its
        // own SIGINT handler while it reads, and puts back the one it found,
        // so one installed meanwhile would be lost. Without the thread,
        // SIGINT ends the shell (its default).
        if waiting.is_ok() {
            let _ = listening.recv();
        }
        Interrupts { running, notify }
    }

    /// Mark a statement as running, and return a future that completes on
    /// Ctrl-C while it is.
    fn armed(&self) -> impl std::future::Future<Output = ()> + '_ {
        let mut notified = Box::pin(self.notify.notified());
        // Waiting from here on, before the signal thread can see `running`.
        notified.as_mut().enable();
        self.running.store(true, Ordering::SeqCst);
        notified
    }

    /// The statement is done (or dropped).
    fn disarm(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

/// What Ctrl-C says when it cancels a statement in the shell.
const INTERRUPTED: &str = "Interrupted";

/// Write the history, saying once if it cannot be.
fn save(rl: &mut Editor<ShellHelper, FileHistory>, path: &PathBuf) {
    if let Err(e) = rl.append_history(path) {
        if !rl.helper().is_some_and(|h| h.history_failed.replace(true)) {
            eprintln!(
                "Warning: cannot write the history file '{}': {e}",
                path.display()
            );
        }
    }
}

/// Completion of dot-commands, and of file names after `.read`, `.open` or
/// an open quote (`FROM 'karate_<Tab>`); colour, with `color`.
#[derive(Default)]
struct ShellHelper {
    color: bool,
    /// Where the line starts: in a new statement or the one being typed.
    state: highlight::State,
    files: FilenameCompleter,
    history_failed: std::cell::Cell<bool>,
}

impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];
        let start = before.len() - before.trim_start().len();
        let typed = &before[start..];
        if typed.starts_with('.') && !typed.contains(char::is_whitespace) {
            let candidates = COMMANDS
                .iter()
                .filter(|(name, _, _)| name.starts_with(typed))
                .map(|(name, _, _)| Pair {
                    display: name.to_string(),
                    replacement: format!("{name} "),
                })
                .collect();
            return Ok((start, candidates));
        }
        let names_a_file = typed.starts_with(".read ") || typed.starts_with(".open ");
        if names_a_file || before.matches('\'').count() % 2 == 1 {
            return self.files.complete(line, pos, ctx);
        }
        Ok((pos, Vec::new()))
    }
}

impl Hinter for ShellHelper {
    type Hint = String;
}

impl Highlighter for ShellHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if self.color {
            Cow::Owned(highlight::ansi_from(line, &self.state))
        } else {
            Cow::Borrowed(line)
        }
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        if self.color {
            Cow::Owned(format!("{}{prompt}{}", highlight::PROMPT, highlight::RESET))
        } else {
            Cow::Borrowed(prompt)
        }
    }

    // Colour each change as it is typed.
    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        self.color
    }
}

impl Validator for ShellHelper {}

impl Helper for ShellHelper {}
