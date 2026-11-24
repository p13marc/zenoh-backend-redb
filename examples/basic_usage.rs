//! Using the storage engine directly, without `zenohd`.
//!
//! The plugin is the usual way in (see `config/zenoh-redb-example.json5`); this
//! shows what it does underneath: one redb file per storage, a latest value per key,
//! and, with `HistoryMode::All`, every sample kept and addressable by time.
//!
//! Run with: cargo run --example basic_usage

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zenoh::bytes::Encoding;
use zenoh::time::{NTP64, Timestamp, TimestampId};
use zenoh_backend_redb::{
    HistoryMode, RedbStorage, RedbStorageConfig, RetentionPolicy, StoredValue,
};
use zenoh_util::time_range::{TimeBound, TimeRange};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let id = TimestampId::rand();
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let at = |secs_ago: u64| Timestamp::new(NTP64::from(now - Duration::from_secs(secs_ago)), id);

    // A latest-value storage: one value per key, last writer wins.
    let state = RedbStorage::open(
        dir.path().join("state.redb"),
        RedbStorageConfig::new(),
        "state",
    )?;
    state.put(
        "robot/1/mode",
        StoredValue::new(b"idle".to_vec(), at(20), Encoding::TEXT_PLAIN),
    )?;
    state.put(
        "robot/1/mode",
        StoredValue::new(b"moving".to_vec(), at(10), Encoding::TEXT_PLAIN),
    )?;
    // An older sample arriving late does not replace a newer one.
    let outcome = state.put(
        "robot/1/mode",
        StoredValue::new(b"stale".to_vec(), at(15), Encoding::TEXT_PLAIN),
    )?;
    let mode = state.get("robot/1/mode")?.expect("stored");
    println!(
        "latest: robot/1/mode = {} (late sample: {outcome:?})",
        String::from_utf8_lossy(&mode.payload)
    );

    // A history storage keeps every sample. It must say how much it may keep.
    let telemetry = RedbStorage::open(
        dir.path().join("telemetry.redb"),
        RedbStorageConfig::new()
            .with_history(HistoryMode::All)
            .with_retention(RetentionPolicy {
                max_age_secs: Some(30),
                ..Default::default()
            }),
        "telemetry",
    )?;
    for secs_ago in (0..60).rev().step_by(5) {
        telemetry.put(
            "robot/1/battery",
            StoredValue::new(
                format!("{}", 100 - secs_ago).into_bytes(),
                at(secs_ago),
                Encoding::TEXT_PLAIN,
            ),
        )?;
    }

    let last_20s = TimeRange {
        start: TimeBound::Inclusive(SystemTime::now() - Duration::from_secs(20)),
        end: TimeBound::Unbounded,
    };
    let window = telemetry.get_range("robot/1/battery", &last_20s)?;
    println!("history: {} samples in the last 20s", window.len());

    // Retention normally runs on a timer inside the plugin; run one pass by hand.
    let pass = telemetry.enforce_retention()?;
    println!(
        "retention: dropped {} of {} samples older than 30s",
        pass.samples_dropped,
        pass.samples_before.unwrap_or(0)
    );

    let stats = telemetry.stats()?;
    println!(
        "stats: {} key(s), {} sample(s), {} bytes on disk",
        stats.key_count,
        stats.sample_count,
        stats.on_disk_bytes.unwrap_or(0)
    );
    Ok(())
}
