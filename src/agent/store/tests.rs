use std::os::unix::fs::PermissionsExt as _;

use super::{Enrollment, Status, Store, Stored};
use crate::agent::{scratch::Scratch, wire::KeyPair};

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_key_is_kept_private_and_reads_back() {
    let scratch = Scratch::new();
    let dir = scratch.path().join("var/lib/microscope/agent");
    let store = Store::open(&dir).unwrap();
    assert_eq!(mode(&dir), 0o700);
    assert!(matches!(store.key().unwrap(), Stored::Missing));

    let key = KeyPair::from_seed([7; 32]).unwrap();
    store.save_key(&key).unwrap();
    assert_eq!(mode(&dir.join("key")), 0o600);
    let Stored::Found(read) = store.key().unwrap() else {
        panic!("the key did not read back");
    };
    assert_eq!(read.seed(), key.seed());
    assert!(!dir.join(".key.new").exists());

    let enrollment = Enrollment {
        agent_id: 81723,
        server: "https://a.test".into(),
    };
    store.save_enrollment(&enrollment).unwrap();
    assert_eq!(store.enrollment().unwrap(), Stored::Found(enrollment));

    store.forget().unwrap();
    assert!(matches!(store.key().unwrap(), Stored::Missing));
    assert_eq!(store.enrollment().unwrap(), Stored::Missing);
    store.forget().unwrap();
}

#[test]
fn a_damaged_file_is_told_apart_from_a_missing_one() {
    let scratch = Scratch::new();
    let store = Store::open(scratch.path()).unwrap();
    scratch.file("key", "ed25519 short\n");
    scratch.file("agent.json", "{\"agent_id\":");
    scratch.file("status.json", "");
    assert!(matches!(store.key().unwrap(), Stored::Damaged));
    assert_eq!(store.enrollment().unwrap(), Stored::Damaged);
    assert_eq!(store.status().unwrap(), Stored::Damaged);
}

#[test]
fn a_directory_others_can_write_is_refused() {
    let scratch = Scratch::new();
    std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let error = Store::open(scratch.path()).unwrap_err();
    assert!(error.to_string().contains("writable by others"), "{error}");
}

#[test]
fn the_run_marker_holds_the_boot_and_goes_away_on_a_clean_stop() {
    let scratch = Scratch::new();
    let store = Store::open(scratch.path()).unwrap();
    assert_eq!(store.marker().unwrap(), None);
    store
        .mark_running("0c4f8e57-2b8a-4d3e-9f61-0d2c5a1b7e93")
        .unwrap();
    assert_eq!(
        store.marker().unwrap().as_deref(),
        Some("0c4f8e57-2b8a-4d3e-9f61-0d2c5a1b7e93")
    );
    store.mark_stopped().unwrap();
    assert_eq!(store.marker().unwrap(), None);
    store.mark_stopped().unwrap();

    let status = Status {
        server: "https://a.test".into(),
        state: "enrolled".into(),
        pid: 1,
        ..Status::default()
    };
    store.save_status(&status).unwrap();
    assert_eq!(store.status().unwrap(), Stored::Found(status));
}
