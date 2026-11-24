//! A retention pass must cost memory in proportion to what it deletes per batch,
//! not to what the storage holds.
//!
//! It did not: the age pass built the full list of doomed keys before deleting
//! any, and the `max_bytes` pass read *every* history key into a `Vec` and sorted
//! it. On a multi-gigabyte storage that is gigabytes of RAM every interval, on the
//! kind of small guest a storage like this runs on.
//!
//! This file has its own global allocator so it can measure that, and exactly one
//! test so nothing else allocates while it does.
//!
//! It writes 120 000 samples, one commit each. That is tolerable in a debug build
//! only because `Cargo.toml` optimises redb there (`profile.dev.package.redb`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zenoh::bytes::Encoding;
use zenoh::time::{NTP64, Timestamp, TimestampId};
use zenoh_backend_redb::config::{HistoryMode, RetentionPolicy};
use zenoh_backend_redb::{RedbStorage, RedbStorageConfig, StoredValue};

/// Counts live heap bytes and remembers the high-water mark.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Heap growth, above where it stood at the start, while `f` runs.
fn peak_growth<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(baseline))
}

const SAMPLES: u64 = 60_000;
/// A long key, so that holding every key in memory is unmistakable: 60 000 of
/// these is about 19 MB of keys alone.
const KEY_LEN: usize = 300;
/// redb's page cache. Part of what a pass may legitimately use.
const CACHE: usize = 2 * 1024 * 1024;
/// What a pass may add to the heap: the page cache, one batch of keys (about
/// 1.3 MB), one write transaction's dirty pages, and slack. A third of what the
/// keys alone come to.
const BUDGET: usize = 6 * 1024 * 1024;

fn key(i: u64) -> String {
    format!("v1/h-{:04}/telemetry/{}", i % 50, "x".repeat(KEY_LEN))
}

fn open(path: &std::path::Path, policy: RetentionPolicy) -> RedbStorage {
    let config = RedbStorageConfig::default()
        .with_history(HistoryMode::All)
        .with_cache_size(CACHE)
        // One fsync per sample would make this test take minutes.
        .with_fsync(false)
        .with_retention(policy);
    RedbStorage::open(path, config, "memory").unwrap()
}

fn fill(storage: &RedbStorage, age_secs: u64) {
    let id = TimestampId::rand();
    let base =
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap() - Duration::from_secs(age_secs);
    for i in 0..SAMPLES {
        // A distinct timestamp per sample, a millisecond apart.
        let time = NTP64::from(base + Duration::from_millis(i));
        storage
            .put(
                &key(i),
                StoredValue::new(
                    vec![1u8; 16],
                    Timestamp::new(time, id),
                    Encoding::ZENOH_BYTES,
                ),
            )
            .unwrap();
    }
}

#[test]
fn a_retention_pass_does_not_hold_the_history_in_memory() {
    let dir = tempfile::tempdir().unwrap();

    // Age: every sample is expired, so the old implementation's list of doomed
    // keys was the whole history.
    {
        let storage = open(
            &dir.path().join("age.redb"),
            RetentionPolicy {
                max_age_secs: Some(3600),
                ..Default::default()
            },
        );
        fill(&storage, 7200);

        let (pass, growth) = peak_growth(|| storage.enforce_retention().unwrap());
        eprintln!(
            "heap growth {growth} bytes, dropped {}",
            pass.samples_dropped
        );
        assert_eq!(pass.samples_dropped, SAMPLES);
        assert!(
            growth < BUDGET,
            "an age pass over {SAMPLES} samples grew the heap by {growth} bytes"
        );
    }

    // Size: nothing is expired, but the data is over `max_bytes`, so the oldest
    // must be found across every key. The old implementation read and sorted them
    // all, once per round.
    {
        let storage = open(
            &dir.path().join("size.redb"),
            RetentionPolicy {
                max_bytes: Some(20 * 1024 * 1024),
                ..Default::default()
            },
        );
        fill(&storage, 60);

        let (pass, growth) = peak_growth(|| storage.enforce_retention().unwrap());
        eprintln!(
            "heap growth {growth} bytes, dropped {}",
            pass.samples_dropped
        );
        assert!(
            pass.samples_evicted_for_size > 0,
            "the data must be over the limit"
        );
        assert!(pass.samples_after.unwrap() > 0, "and not all of it evicted");
        assert!(
            growth < BUDGET,
            "a size pass over {SAMPLES} samples grew the heap by {growth} bytes"
        );
    }
}
