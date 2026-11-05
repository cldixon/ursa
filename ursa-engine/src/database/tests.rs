use std::time::Duration;

use super::*;

fn quick() -> OpenOptions {
    OpenOptions {
        lock_timeout: Duration::from_millis(200),
        ..OpenOptions::default()
    }
}

#[test]
fn open_creates_the_file_the_sidecar_and_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("club.ursa");
    let db = Database::open(&path, &quick()).unwrap();
    assert!(db.statements().is_empty());
    assert!(dir.path().join("club.ursa.d").is_dir());
    assert!(dir.path().join("club.ursa.d/lock").is_file());
    assert_eq!(db.index_dir(), dir.path().join("club.ursa.d/indexes"));
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        text,
        "{\n  \"format\": \"ursa\",\n  \"version\": 1,\n  \"statements\": []\n}\n"
    );
}

#[test]
fn save_rewrites_the_catalog_and_a_later_open_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("club.ursa");
    {
        let db = Database::open(&path, &quick()).unwrap();
        db.save(vec!["CREATE VIEW v AS SELECT 1".into()]).unwrap();
        // No temporary file is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("club.ursa.d"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(leftovers, ["lock"]);
    }
    let db = Database::open(&path, &quick()).unwrap();
    assert_eq!(db.statements(), ["CREATE VIEW v AS SELECT 1"]);
}

#[test]
fn a_second_writer_is_refused_while_the_first_has_it_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("club.ursa");
    let first = Database::open(&path, &quick()).unwrap();
    let err = Database::open(&path, &quick()).unwrap_err();
    assert_eq!(
        crate::user_message(&err),
        format!("{} is in use by another process", path.display())
    );
    // A reader takes no lock.
    let reader = Database::open(&path, &OpenOptions::read_only()).unwrap();
    assert!(reader.is_read_only());
    assert!(reader.save(vec![]).is_err());
    drop(first);
    Database::open(&path, &quick()).unwrap();
}

#[test]
fn read_only_needs_an_existing_file_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("club.ursa");
    let err = Database::open(&path, &OpenOptions::read_only()).unwrap_err();
    assert!(
        crate::user_message(&err).contains("the file does not exist"),
        "{err}"
    );
    assert!(!dir.path().join("club.ursa.d").exists());
    assert!(!path.exists());
}

#[test]
fn newer_and_corrupt_files_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("club.ursa");
    let message = |text: &str| {
        std::fs::write(&path, text).unwrap();
        crate::user_message(&Database::open(&path, &quick()).unwrap_err())
    };
    let name = path.display();
    assert_eq!(
        message(r#"{"format": "ursa", "version": 2, "statements": []}"#),
        format!(
            "{name} was written by a newer version of Ursa (database format version 2; this \
             version of ursa reads version 1); upgrade ursa to open it"
        )
    );
    assert!(message("not json").starts_with(&format!(
        "{name} is not a valid Ursa database: it is not JSON"
    )));
    assert_eq!(
        message(r#"{"format": "duck", "version": 1, "statements": []}"#),
        format!("{name} is not a valid Ursa database: its \"format\" is not \"ursa\"")
    );
    assert_eq!(
        message(r#"{"format": "ursa", "version": 1, "statements": [1]}"#),
        format!("{name} is not a valid Ursa database: an entry of \"statements\" is not a string")
    );
    // An empty file is an empty database.
    std::fs::write(&path, "").unwrap();
    assert!(Database::open(&path, &quick())
        .unwrap()
        .statements()
        .is_empty());
}

#[test]
fn relative_paths_resolve_against_the_database_directory() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("club.ursa"), &quick()).unwrap();
    let base = std::path::absolute(dir.path()).unwrap();
    assert_eq!(
        db.resolve("data/edges.csv"),
        base.join("data/edges.csv").to_string_lossy()
    );
    assert_eq!(db.resolve("*.csv"), base.join("*.csv").to_string_lossy());
    for unchanged in [
        "/abs/edges.csv",
        "s3://bucket/edges.parquet",
        "file:///tmp/x.csv",
        "~/edges.csv",
    ] {
        assert_eq!(db.resolve(unchanged), unchanged);
    }
}

#[test]
fn a_session_reaches_its_database_through_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = crate::new_session();
    assert!(database(&ctx.state()).is_none());
    assert_eq!(resolve_location(&ctx.state(), "x.csv"), "x.csv");
    let db = Arc::new(Database::open(dir.path().join("club.ursa"), &quick()).unwrap());
    slot(&ctx.state()).set(Arc::clone(&db)).unwrap();
    let state = ctx.state();
    let found = database_of_config(state.config()).unwrap();
    assert!(Arc::ptr_eq(&found, &db));
    assert!(slot(&state).set(db).is_err());
}
