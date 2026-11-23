//! What an `fsync: false` storage still has after the process is killed.
//!
//! `fsync: false` is `Durability::None`, and redb does not persist a non-durable
//! commit at all until a durable one follows. This backend used to never issue
//! one, so a kill — SIGKILL, the OOM killer, power loss — lost **everything
//! written since the storage was opened**, however long ago that was.
//!
//! A crash cannot be simulated in-process: the database has to stop without its
//! destructor running, and the file lock has to be released, which only a dead
//! process does. So each case re-runs this test binary as a child that writes and
//! then `abort()`s, and the parent looks at what is left.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use zenoh::bytes::Encoding;
use zenoh::time::{NTP64, Timestamp, TimestampId};
use zenoh_backend_redb::{RedbStorage, RedbStorageConfig, StoredValue};

const CHILD_DB: &str = "REDB_CRASH_CHILD_DB";
const CHILD_CASE: &str = "REDB_CRASH_CHILD_CASE";

fn open(path: &Path, flush_interval_secs: u64) -> RedbStorage {
    let config = RedbStorageConfig::default()
        .with_db_path(path.to_path_buf())
        .with_fsync(false)
        .with_flush_interval_secs(flush_interval_secs);
    RedbStorage::new(path, config, "crash".to_string()).unwrap()
}

fn put(storage: &RedbStorage, range: std::ops::Range<u64>) {
    let id = TimestampId::rand();
    for i in range {
        storage
            .put(
                &format!("k/{i:04}"),
                StoredValue::new(
                    vec![1u8; 16],
                    Timestamp::new(NTP64(1000 + i), id),
                    Encoding::ZENOH_BYTES,
                ),
            )
            .unwrap();
    }
}

/// The child half. Does nothing unless the parent asked for it, so it is inert in
/// an ordinary `cargo test` run.
#[test]
fn crash_child() {
    let (Ok(db), Ok(case)) = (std::env::var(CHILD_DB), std::env::var(CHILD_CASE)) else {
        return;
    };
    let path = Path::new(&db);

    match case.as_str() {
        // A flush interval that passes mid-run: the write after it is promoted to
        // durable and carries everything before it to the disk.
        "interval" => {
            let storage = open(path, 1);
            put(&storage, 0..100);
            std::thread::sleep(Duration::from_millis(1300));
            put(&storage, 100..101);
            put(&storage, 101..150);
        }
        // An interval that never passes, and an explicit flush: what the plugin's
        // background thread does when writes stop.
        "flush" => {
            let storage = open(path, 3600);
            put(&storage, 0..100);
            assert!(storage.flush().unwrap(), "there was something to flush");
            assert!(!storage.flush().unwrap(), "and then there was not");
            put(&storage, 100..150);
        }
        other => panic!("unknown case {other}"),
    }

    // No destructors, no clean close: the same as being killed.
    std::process::abort();
}

/// Run `case` in a child that aborts, and return how many keys survived.
fn keys_after_crash(case: &str) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("crash.redb");

    let status = Command::new(std::env::current_exe().unwrap())
        .args(["crash_child", "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_DB, &db)
        .env(CHILD_CASE, case)
        .status()
        .unwrap();
    assert!(!status.success(), "the child was supposed to abort");

    let storage = open(&db, 3600);
    storage.count().unwrap()
}

#[test]
fn writes_older_than_the_flush_interval_survive_a_crash() {
    let survived = keys_after_crash("interval");
    assert!(
        survived >= 101,
        "everything up to the first commit after the interval must be on disk, \
         found {survived} of 150 keys"
    );
}

#[test]
fn flushed_writes_survive_a_crash() {
    let survived = keys_after_crash("flush");
    assert!(
        survived >= 100,
        "everything before the flush must be on disk, found {survived} of 150 keys"
    );
}
