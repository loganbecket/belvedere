//! The database as a file on disk: created on first open, untouched by a
//! second open.

use belvedere_core::db::Db;

#[test]
fn first_open_creates_the_file_and_parent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("belvedere").join("belvedere.db");
    assert!(!path.exists());

    let db = Db::open(&path).unwrap();
    assert!(path.is_file());
    assert_eq!(db.schema_version().unwrap(), 2);
}

#[test]
fn second_open_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("belvedere.db");

    {
        let db = Db::open(&path).unwrap();
        db.set_setting("sweep_minutes", "5").unwrap();
    }
    // Dropping the connection checkpoints the WAL, leaving one settled file.
    let before = std::fs::read(&path).unwrap();
    let before_version;

    {
        let db = Db::open(&path).unwrap();
        before_version = db.schema_version().unwrap();
        assert_eq!(
            db.get_setting("sweep_minutes").unwrap().as_deref(),
            Some("5")
        );
    }

    let after = std::fs::read(&path).unwrap();
    assert_eq!(before_version, 2);
    assert_eq!(before, after, "reopening must not modify the database file");
}

#[test]
fn default_path_respects_xdg_data_home() {
    // Serialize env access across the test binary's threads.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap();

    let old = std::env::var_os("XDG_DATA_HOME");
    std::env::set_var("XDG_DATA_HOME", "/tmp/xdg-test");
    let path = Db::default_path().unwrap();
    match old {
        Some(v) => std::env::set_var("XDG_DATA_HOME", v),
        None => std::env::remove_var("XDG_DATA_HOME"),
    }
    assert_eq!(
        path,
        std::path::Path::new("/tmp/xdg-test/belvedere/belvedere.db")
    );
}
