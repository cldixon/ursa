//! The terminal side of the shell: rustyline for line editing and history,
//! and Ctrl-C to cancel a running query.
//!
//! Adapted from datafusion-cli's `exec_from_repl` and `CliHelper`
//! (Apache-2.0), with Ursa's parser in place of `DFParser`: each line goes to
//! [`Shell::feed`], which knows when a statement is complete, so the prompt
//! can show a continuation while one is being typed. A complete statement,
//! all its lines together, is one history entry.

use std::borrow::Cow;
use std::io::Write;
use std::path::PathBuf;

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

    // The lines of the statement being typed, for its history entry.
    let mut entry = String::new();
    loop {
        // `memory U `, or `club U ` with club.ursa open; then spaces as wide.
        let prompt = if shell.is_pending() {
            shell.continuation()
        } else {
            shell.prompt()
        };
        match rl.readline(&prompt) {
            Ok(line) => {
                if !entry.is_empty() {
                    entry.push('\n');
                }
                entry.push_str(&line);
                let flow = tokio::select! {
                    flow = shell.feed(&line) => Some(flow),
                    _ = tokio::signal::ctrl_c() => None,
                };
                let flow = flow.unwrap_or_else(|| {
                    eprintln!("^C (cancelled)");
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
            Cow::Owned(highlight::ansi(line))
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
