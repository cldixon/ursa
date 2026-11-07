//! The interactive shell, driven through a pseudo-terminal as a person
//! drives it. The read–eval loop itself is unit-tested in `src/shell.rs`;
//! this checks the terminal side: the banner, `~/.ursarc`, the prompts,
//! multi-line input, Ctrl-C, history and Ctrl-D.
#![cfg(unix)]

use std::process::Command;

use rexpect::session::{spawn_command, PtySession};

const TIMEOUT_MS: u64 = 30_000;

fn shell(home: &std::path::Path, args: &[&str]) -> PtySession {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ursa"));
    command
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env_remove("URSA_HISTORY")
        .env("TERM", "xterm");
    spawn_command(command, Some(TIMEOUT_MS)).expect("ursa starts in a pseudo-terminal")
}

#[test]
fn a_session_in_the_shell() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(".ursarc"), ".mode csv\n").unwrap();
    let mut p = shell(home.path(), &["club.ursa"]);

    p.exp_string(&format!(
        "ursa {}, database club.ursa.",
        env!("URSA_VERSION")
    ))
    .unwrap();
    p.exp_string("ursa> ").unwrap();

    // A statement over two lines, printed as CSV (from ~/.ursarc).
    p.send_line("SELECT 1 AS x,").unwrap();
    p.exp_string(" ...> ").unwrap();
    p.send_line("  'a;b' AS y;").unwrap();
    p.exp_string("x,y\r\n1,a;b\r\n").unwrap();
    p.exp_string("ursa> ").unwrap();

    // Ursa's own statements, kept in the database.
    p.send_line("CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2)) AS t(src, dst);")
        .unwrap();
    p.exp_string("ursa> ").unwrap();
    p.send_line(
        "CREATE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
    )
    .unwrap();
    p.exp_string("ursa> ").unwrap();
    p.send_line(".graphs").unwrap();
    p.exp_string("g,ties,src,dst").unwrap();

    // An error, and the session goes on.
    p.send_line(".tabels").unwrap();
    p.exp_string("did you mean '.tables'?").unwrap();
    p.send_line("SELECT nope;").unwrap();
    p.exp_string("Error: ").unwrap();
    p.exp_string("ursa> ").unwrap();

    // Ctrl-C drops a half-typed statement.
    p.send_line("SELECT 'dropped'").unwrap();
    p.exp_string(" ...> ").unwrap();
    p.send_control('c').unwrap();
    p.exp_string("ursa> ").unwrap();
    p.send_line("SELECT 2 AS z;").unwrap();
    p.exp_string("z\r\n2\r\n").unwrap();
    p.exp_string("ursa> ").unwrap();

    // Ctrl-D exits.
    p.send_control('d').unwrap();
    p.exp_eof().unwrap();

    let history = std::fs::read_to_string(home.path().join(".ursa_history")).unwrap();
    assert!(history.contains("SELECT 2 AS z;"), "{history}");
    assert!(history.contains(".graphs"), "{history}");
    assert!(!history.contains("dropped"), "{history}");
    let catalog = std::fs::read_to_string(home.path().join("club.ursa")).unwrap();
    assert!(catalog.contains("CREATE PROPERTY GRAPH g"), "{catalog}");
}

#[test]
fn ctrl_c_cancels_a_running_query() {
    let home = tempfile::tempdir().unwrap();
    let mut p = shell(home.path(), &["--no-history"]);
    p.exp_string("in memory.").unwrap();
    p.exp_string("ursa> ").unwrap();
    // Long enough that it is still running when Ctrl-C arrives.
    p.send_line(
        "SELECT count(*) FROM generate_series(1, 100000000000) a CROSS JOIN generate_series(1, 10) b;",
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    p.send_control('c').unwrap();
    p.exp_string("^C (cancelled)").unwrap();
    p.exp_string("ursa> ").unwrap();
    // An algorithm too, which runs on a blocking thread (#328): the prompt
    // comes back, and the shell still exits at once after it.
    p.send_line(
        "CREATE VIEW e AS SELECT value % 2000000 AS src, (value * 7919 + 13) % 2000000 AS dst \
         FROM generate_series(1, 20000000); \
         CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (src) DESTINATION KEY (dst)); \
         SELECT count(*) FROM louvain(g);",
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2000));
    p.send_control('c').unwrap();
    p.exp_string("^C (cancelled)").unwrap();
    p.exp_string("ursa> ").unwrap();
    p.send_line("SELECT 41 + 1 AS answer;").unwrap();
    p.exp_string("| 42     |").unwrap();
    p.send_line(".quit").unwrap();
    p.exp_eof().unwrap();
    assert!(!home.path().join(".ursa_history").exists());
}
