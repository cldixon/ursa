//! Ursa's shell, apart from the terminal.
//!
//! The `ursa` CLI and the browser shell run the same shell: [`shell::Shell`]
//! takes a line at a time and runs dot-commands and SQL, [`table`] draws
//! results as DuckDB's box tables, and [`print`] writes the other output
//! formats. The host owns the screen: line editing, history and Ctrl-C are
//! `ursa-cli`'s (rustyline) in a terminal and the page's (xterm.js) in a
//! browser.

pub mod input;
pub mod print;
pub mod shell;
pub mod table;
mod timer;
