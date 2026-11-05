//! Which init file runs at startup, and where the shell keeps its history.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The init file in the home directory, read by the shell when `-init` is
/// not given (DuckDB's is `~/.duckdbrc`).
pub const RC_FILE: &str = ".ursarc";

/// The history file in the home directory.
pub const HISTORY_FILE: &str = ".ursa_history";

/// The environment variable that names the history file; empty turns history
/// off.
pub const HISTORY_VAR: &str = "URSA_HISTORY";

/// The file to run at startup: `-init FILE` if given; otherwise, for the
/// interactive shell only, `~/.ursarc` if it exists. `-no-init` runs none.
///
/// `~/.ursarc` is not read for `-c`, `--file` or SQL on stdin, so a script
/// gives the same output whoever runs it.
pub fn init_file(
    init: Option<&Path>,
    no_init: bool,
    interactive: bool,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if no_init {
        return None;
    }
    if let Some(init) = init {
        return Some(init.to_path_buf());
    }
    let rc = home?.join(RC_FILE);
    (interactive && rc.is_file()).then_some(rc)
}

/// The history file: `$URSA_HISTORY` if set (empty means none), otherwise
/// `~/.ursa_history`. `--no-history` keeps none.
pub fn history_file(
    no_history: bool,
    var: Option<OsString>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if no_history {
        return None;
    }
    match var {
        Some(path) if path.is_empty() => None,
        Some(path) => Some(PathBuf::from(path)),
        None => home.map(|home| home.join(HISTORY_FILE)),
    }
}

/// The user's home directory (`$HOME` on Unix).
pub fn home_dir() -> Option<PathBuf> {
    std::env::home_dir().filter(|home| !home.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_file_prefers_the_flag() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let given = Path::new("setup.sql");
        // No ~/.ursarc: only -init.
        assert_eq!(init_file(None, false, true, Some(home)), None);
        assert_eq!(
            init_file(Some(given), false, true, Some(home)),
            Some(given.to_path_buf())
        );
        assert_eq!(
            init_file(Some(given), false, false, Some(home)),
            Some(given.to_path_buf())
        );

        std::fs::write(home.join(RC_FILE), ".mode csv\n").unwrap();
        assert_eq!(
            init_file(None, false, true, Some(home)),
            Some(home.join(RC_FILE))
        );
        // -init replaces ~/.ursarc; -no-init skips both.
        assert_eq!(
            init_file(Some(given), false, true, Some(home)),
            Some(given.to_path_buf())
        );
        assert_eq!(init_file(None, true, true, Some(home)), None);
        assert_eq!(init_file(Some(given), true, true, Some(home)), None);
        // Scripts do not read ~/.ursarc.
        assert_eq!(init_file(None, false, false, Some(home)), None);
        // Nor does a shell without a home.
        assert_eq!(init_file(None, false, true, None), None);
    }

    #[test]
    fn a_directory_named_ursarc_is_not_read() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(RC_FILE)).unwrap();
        assert_eq!(init_file(None, false, true, Some(home.path())), None);
    }

    #[test]
    fn history_file_follows_the_variable() {
        let home = Path::new("/home/someone");
        assert_eq!(
            history_file(false, None, Some(home)),
            Some(home.join(HISTORY_FILE))
        );
        assert_eq!(
            history_file(false, Some("/tmp/h".into()), Some(home)),
            Some(PathBuf::from("/tmp/h"))
        );
        assert_eq!(history_file(false, Some("".into()), Some(home)), None);
        assert_eq!(history_file(true, None, Some(home)), None);
        assert_eq!(history_file(true, Some("/tmp/h".into()), Some(home)), None);
        assert_eq!(history_file(false, None, None), None);
    }
}
