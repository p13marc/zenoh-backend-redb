//! Zenoh plugin implementation for the redb backend.
//!
//! This module provides the integration between the redb storage backend and
//! Zenoh's plugin system, implementing the required traits for Volume and Storage.

use crate::config::{HistoryMode, RedbStorageConfig, RetentionPolicy};

use crate::storage::{RedbStorage, RetentionPass, StorageUsage, StoredValue, WriteOutcome};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tracing::{debug, info, warn};
use zenoh::{
    Result as ZResult,
    bytes::{Encoding, ZBytes},
    internal::{bail, zenoh_home, zerror},
    key_expr::OwnedKeyExpr,
    query::Parameters,
    time::Timestamp,
    try_init_log_from_env,
};
use zenoh_backend_traits::{
    Capability, History, Persistence, Storage, StorageInsertionResult, StoredData, Volume,
    config::{StorageConfig, VolumeConfig},
};
use zenoh_plugin_trait::{Plugin, plugin_long_version, plugin_version};
use zenoh_util::ffi::JsonValue;
use zenoh_util::time_range::{TimeExpr, TimeRange};

/// The environment variable used to configure the root directory for all redb storages.
pub const SCOPE_ENV_VAR: &str = "ZENOH_BACKEND_REDB_ROOT";

/// The default root directory (within zenoh's home directory) if ZENOH_BACKEND_REDB_ROOT is not specified.
pub const DEFAULT_ROOT_DIR: &str = "zenoh_backend_redb";

// Storage configuration properties
pub const PROP_STORAGE_DIR: &str = "dir";
pub const PROP_STORAGE_DB_FILE: &str = "db_file";
pub const PROP_STORAGE_CREATE_DB: &str = "create_db";
pub const PROP_STORAGE_READ_ONLY: &str = "read_only";
pub const PROP_STORAGE_CACHE_SIZE: &str = "cache_size";
pub const PROP_STORAGE_FSYNC: &str = "fsync";
pub const PROP_STORAGE_FLUSH_INTERVAL: &str = "flush_interval_secs";
pub const PROP_STORAGE_MAX_SAMPLES_PER_GET: &str = "max_samples_per_get";
pub const PROP_STORAGE_RETENTION: &str = "retention";

// Volume configuration properties
pub const PROP_VOLUME_HISTORY: &str = "history";

/// What a sample with no key is stored under; see [`crate::storage::NO_KEY`].
pub const NONE_KEY: &str = crate::storage::NO_KEY;

/// A storage whose file is at most this big has its [`StorageUsage`] computed
/// inside the admin query that asks for it: the walk fits in the default page
/// cache and takes milliseconds. Above it, the query is served a snapshot.
const INLINE_USAGE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// How old a usage snapshot may get before an admin query asks for a new one.
const USAGE_MAX_AGE: Duration = Duration::from_secs(60);

/// How often the background thread looks at whether its storage has failed, when
/// it has nothing else on a timer. The check is one atomic load.
const HEALTH_POLL: Duration = Duration::from_secs(1);

/// The wait between attempts to reopen a failed database: this long after the
/// first, doubling up to [`RECOVERY_BACKOFF_MAX`].
const RECOVERY_BACKOFF_MIN: Duration = Duration::from_secs(1);
const RECOVERY_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// When the background thread should next wake up.
///
/// Never later than `HEALTH_POLL` from now: a storage with nothing on a timer
/// still has to notice that it has failed.
///
/// And while the storage is recovering, **only** the recovery timer counts.
/// Retention and flushing are not run then, so their deadlines are not advanced
/// either; were they still waited on, the first one to pass would make every wait
/// zero-length and the thread would spin at full CPU for as long as the disk stayed
/// full.
fn next_wake(
    now: Instant,
    recovering: bool,
    timers: [Option<Instant>; 2],
    next_recovery: Option<Instant>,
) -> Instant {
    let poll = now + HEALTH_POLL;
    if recovering {
        next_recovery.map_or(poll, |at| at.min(poll))
    } else {
        timers.into_iter().flatten().fold(poll, Instant::min)
    }
}

/// What the storage and its background thread both touch.
#[derive(Default)]
struct Shared {
    /// Result of the most recent retention pass, for the admin space.
    last_pass: Mutex<RetentionPass>,
    /// The most recent [`StorageUsage`], and when it was taken.
    usage: Mutex<Option<(StorageUsage, Instant)>>,
    /// A refresh has been asked for and not finished. Keeps a dashboard polling
    /// the admin space from queueing a tree walk per poll.
    usage_refresh_pending: AtomicBool,
    /// How many times the background thread has gone round its loop.
    #[cfg(test)]
    wakeups: std::sync::atomic::AtomicU64,
}

/// What the storage asks its background thread to do.
enum Request {
    /// Recompute the usage snapshot.
    RefreshUsage,
}

/// Everything a storage does off the request path, on one thread, until the
/// storage is dropped:
///
/// * **retention**, every `retention_interval` — a scan on the PUT path would make
///   every write pay for a policy that only needs checking occasionally;
/// * **flushing** an `fsync: false` storage, every `flush_interval`, so the last
///   writes before a quiet period reach the disk without waiting for the next one;
/// * **usage statistics**, when an admin query asks — computing them reads the
///   whole database, which is not something to do inside that query;
/// * **recovery**: when the database file has failed (a full disk), reopening it
///   until that works, so the storage comes back without a router restart.
///
/// This is a plain OS thread, deliberately. The obvious implementation —
/// `tokio::task::spawn` — panics: `Volume::create_storage` is not called from
/// inside a tokio runtime context in `zenohd`, so spawning there takes down every
/// `all`-mode storage at router startup. (Found by the conformance suite, which is
/// exactly the class of assumption unit tests cannot check: under `#[tokio::test]`
/// a runtime is always present.) A thread also suits the work, which is blocking
/// by nature — it scans tables and compacts the file.
///
/// `recv_timeout` doubles as the timer and the shutdown wait, so dropping the
/// storage (and with it the sender) stops the thread promptly instead of leaving it
/// sleeping out an interval.
fn spawn_background(
    storage: Arc<RedbStorage>,
    shared: Arc<Shared>,
    requests: Receiver<Request>,
    retention_interval: Option<Duration>,
    flush_interval: Option<Duration>,
    name: String,
) {
    std::thread::Builder::new()
        .name(format!("redb-{name}"))
        .spawn(move || {
            if let Some(interval) = retention_interval {
                info!(
                    "Retention for '{}' running every {}s",
                    name,
                    interval.as_secs()
                );
            }
            // `checked_add`, here and below: an interval too long for an `Instant`
            // to hold means "never", not a panic that ends this thread — and with
            // it retention and flushing, silently.
            let started = Instant::now();
            let mut next_retention = retention_interval.and_then(|i| started.checked_add(i));
            let mut next_flush = flush_interval.and_then(|i| started.checked_add(i));

            let mut next_recovery: Option<Instant> = None;
            let mut recovery_backoff = RECOVERY_BACKOFF_MIN;

            loop {
                #[cfg(test)]
                shared.wakeups.fetch_add(1, Ordering::Relaxed);

                let deadline = next_wake(
                    Instant::now(),
                    storage.needs_recovery(),
                    [next_retention, next_flush],
                    next_recovery,
                );
                let request =
                    requests.recv_timeout(deadline.saturating_duration_since(Instant::now()));
                match request {
                    Ok(Request::RefreshUsage) => {
                        if let Some(usage) =
                            contained(&name, "Collecting usage", || storage.usage())
                        {
                            *shared.usage.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some((usage, Instant::now()));
                        }
                        shared.usage_refresh_pending.store(false, Ordering::Release);
                    }
                    // The storage was dropped and the sender with it. There is
                    // nothing left to maintain.
                    Err(RecvTimeoutError::Disconnected) => {
                        debug!("Background thread for '{}' stopping", name);
                        return;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }

                let now = Instant::now();

                // A failed storage has one job: get its database open again.
                // Retention and flushing would only fail against it, so they wait.
                if storage.needs_recovery() {
                    if next_recovery.is_none_or(|at| now >= at) {
                        // A failed attempt is not logged above debug: the outage
                        // was announced once, `health.last_error` carries the
                        // latest reason, and a line every 30 s for as long as a
                        // disk stays full tells nobody anything new.
                        let attempt =
                            contained(
                                &name,
                                "Reopening the database",
                                || Ok(storage.try_recover()),
                            );
                        if let Some(Err(e)) = &attempt {
                            debug!("Reopening the database of '{}' failed: {}", name, e);
                        }
                        if matches!(attempt, Some(Ok(true))) {
                            next_recovery = None;
                            recovery_backoff = RECOVERY_BACKOFF_MIN;
                        } else {
                            next_recovery = Instant::now().checked_add(recovery_backoff);
                            recovery_backoff = (recovery_backoff * 2).min(RECOVERY_BACKOFF_MAX);
                        }
                    }
                    continue;
                }
                next_recovery = None;
                recovery_backoff = RECOVERY_BACKOFF_MIN;

                if let Some(interval) = flush_interval
                    && next_flush.is_some_and(|at| now >= at)
                {
                    contained(&name, "Flush", || storage.flush());
                    next_flush = Instant::now().checked_add(interval);
                }
                if let Some(interval) = retention_interval
                    && next_retention.is_some_and(|at| now >= at)
                {
                    if let Some(pass) =
                        contained(&name, "Retention pass", || storage.enforce_retention())
                    {
                        *shared.last_pass.lock().unwrap_or_else(|e| e.into_inner()) = pass;
                    }
                    next_retention = Instant::now().checked_add(interval);
                }
            }
        })
        .expect("spawn storage background thread");
}

/// Where a storage's database lives, from its `dir` or `db_file` property.
///
/// Both are names **inside the volume's root** and nothing else. They used to be
/// pushed onto the root unchecked, so an absolute `db_file` replaced the root
/// outright and a `..` walked out of it: the storage opened, or created, a file
/// anywhere the router could write. That is a footgun in a config file, and more
/// than one where the admin space is writable, since storages can be created at
/// runtime by whoever may PUT on the router's config.
fn resolve_db_path(
    root: &std::path::Path,
    volume_cfg: &serde_json::Map<String, serde_json::Value>,
    storage: &str,
) -> ZResult<PathBuf> {
    let (property, value) = match (
        volume_cfg.get(PROP_STORAGE_DIR),
        volume_cfg.get(PROP_STORAGE_DB_FILE),
    ) {
        (Some(dir), _) => (PROP_STORAGE_DIR, dir),
        (None, Some(file)) => (PROP_STORAGE_DB_FILE, file),
        (None, None) => bail!(
            "Storage '{}': one of `{}` or `{}` is required, to name its database",
            storage,
            PROP_STORAGE_DIR,
            PROP_STORAGE_DB_FILE
        ),
    };
    let Some(name) = value.as_str() else {
        bail!(
            "Storage '{}': `{}` must be a string, got {}",
            storage,
            property,
            value
        )
    };

    // Only plain path components: no root, no drive prefix, no `.` or `..`.
    let confined = !name.is_empty()
        && std::path::Path::new(name)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !confined {
        bail!(
            "Storage '{}': `{}: \"{}\"` is not a name inside the storage root {:?}. It \
             must be a relative path with no `..` in it. To keep databases somewhere \
             else, move the root with {}.",
            storage,
            property,
            name,
            root,
            SCOPE_ENV_VAR
        )
    }

    Ok(if property == PROP_STORAGE_DIR {
        db_path_for_dir(root, name, storage)
    } else {
        // `db_file` is the file name as given; `.redb` is only added to a name
        // that has no extension of its own.
        let mut path = root.join(name);
        if path.extension().is_none() {
            path.set_extension("redb");
        }
        path
    })
}

/// The database file for a storage's `dir` property: `<base>/<dir>.redb`.
///
/// `.redb` is **appended**. This used `PathBuf::set_extension`, which *replaces*
/// whatever follows the last dot, so `dir: "telemetry.v1"` and `dir: "telemetry.v2"`
/// both became `telemetry.redb` and the second storage failed to open a file the
/// first had locked.
///
/// A deployment that already ran with a dotted `dir` has its data in the truncated
/// file. That file is **not** opened in its place. It cannot be told apart from the
/// rightful file of another storage — `telemetry.redb` is exactly what
/// `dir: "telemetry"` names — so guessing would hand one storage another's
/// database. The log says what was found and the two ways to adopt it.
fn db_path_for_dir(base: &std::path::Path, dir: &str, storage: &str) -> PathBuf {
    // Somebody who wrote the extension out meant the file, not `x.redb.redb`.
    let path = if dir.ends_with(".redb") {
        base.join(dir)
    } else {
        base.join(format!("{dir}.redb"))
    };

    let mut truncated = base.join(dir);
    truncated.set_extension("redb");
    if truncated != path && truncated.exists() && !path.exists() {
        warn!(
            "Storage '{}': `dir: \"{}\"` is {:?}, which does not exist yet and will be \
             created empty. {:?} does exist: versions before 0.5.0 dropped everything \
             after the last dot in `dir`, so that file may hold this storage's data. If it \
             does, stop the router and rename it to {:?}, or point this storage at it with \
             `db_file` instead of `dir`. If it belongs to another storage, ignore this.",
            storage,
            dir,
            path,
            truncated,
            path.file_name().unwrap_or_default()
        );
    }
    path
}

/// Seconds since the Unix epoch, for a timestamp on the admin space.
fn epoch_secs(time: SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run one piece of background work, containing whatever goes wrong in it.
///
/// A failure is logged and skipped. So is a *panic*, which matters more: without
/// this the thread would be gone, the storage would keep serving, and the disk
/// would fill behind a retention policy that still shows on the admin space — or
/// an `fsync: false` storage would quietly stop being flushed.
fn contained<T>(
    name: &str,
    what: &str,
    work: impl FnOnce() -> crate::error::Result<T>,
) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(Ok(done)) => Some(done),
        Ok(Err(e)) => {
            warn!("{} on '{}' failed: {}", what, name, e);
            None
        }
        Err(panic) => {
            tracing::error!(
                "{} on '{}' panicked: {}. The storage's background thread carries on.",
                what,
                name,
                panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("(no message)")
            );
            None
        }
    }
}

/// The selector parameter Zenoh reserves for a time range.
pub const PARAM_TIME: &str = "_time";

/// Pull a resolved [`TimeRange`] out of a selector's query parameters.
///
/// Returns `Ok(None)` when no `_time` was given, which is the common case and must
/// stay cheap. A malformed `_time` is an error rather than a silent full-history
/// reply: answering the wrong window is worse than refusing.
fn parse_time_range(parameters: &str) -> ZResult<Option<TimeRange<SystemTime>>> {
    if parameters.is_empty() {
        return Ok(None);
    }

    let Some(spec) = Parameters::from(parameters)
        .get(PARAM_TIME)
        .map(str::to_owned)
    else {
        return Ok(None);
    };

    let range: TimeRange<TimeExpr> = spec
        .parse()
        .map_err(|e| zerror!("Invalid `{}={}` in selector: {:?}", PARAM_TIME, spec, e))?;

    // Resolve `now(...)` against a single instant, so both bounds of one query
    // agree on when "now" was.
    Ok(Some(range.resolve_at(SystemTime::now())))
}

/// The redb backend plugin.
pub struct RedbBackendPlugin {}

#[cfg(feature = "dynamic_plugin")]
zenoh_plugin_trait::declare_plugin!(RedbBackendPlugin);

impl Plugin for RedbBackendPlugin {
    type StartArgs = VolumeConfig;
    type Instance = Box<dyn Volume>;

    const DEFAULT_NAME: &'static str = "redb_backend";
    const PLUGIN_VERSION: &'static str = plugin_version!();
    const PLUGIN_LONG_VERSION: &'static str = plugin_long_version!();

    fn start(name: &str, config: &Self::StartArgs) -> ZResult<Self::Instance> {
        try_init_log_from_env();
        info!("redb backend {}", Self::PLUGIN_LONG_VERSION);

        // History is a *volume*-level choice; see `HistoryMode` for why it cannot be
        // per-storage. One plugin serves both: declare a second volume that names
        // this backend and sets `history: "all"`.
        let rest: serde_json::Map<String, serde_json::Value> = (&config.rest).into();
        let history = match rest.get(PROP_VOLUME_HISTORY) {
            None => HistoryMode::Latest,
            Some(serde_json::Value::String(mode)) => HistoryMode::parse(mode).ok_or_else(|| {
                zerror!(
                    "Volume '{}': `{}` must be \"latest\" or \"all\", got \"{}\"",
                    name,
                    PROP_VOLUME_HISTORY,
                    mode
                )
            })?,
            Some(other) => {
                bail!(
                    "Volume '{}': `{}` must be a string, got {}",
                    name,
                    PROP_VOLUME_HISTORY,
                    other
                )
            }
        };
        info!("redb volume '{}' history mode: {}", name, history.as_str());

        // Determine root directory
        let root = if let Some(dir) = std::env::var_os(SCOPE_ENV_VAR) {
            PathBuf::from(dir)
        } else {
            let mut dir = PathBuf::from(zenoh_home());
            dir.push(DEFAULT_ROOT_DIR);
            dir
        };

        std::fs::create_dir_all(&root).map_err(|e| {
            zerror!(
                "Volume '{}': cannot create the storage root {:?}: {}",
                name,
                root,
                e
            )
        })?;

        // Prepare admin status
        let mut properties = HashMap::new();
        properties.insert("root".to_string(), root.to_string_lossy().to_string());
        properties.insert("version".to_string(), Self::PLUGIN_VERSION.to_string());
        properties.insert("history".to_string(), history.as_str().to_string());

        let admin_status: serde_json::Value = properties
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect();

        Ok(Box::new(RedbVolume {
            admin_status,
            root,
            history,
        }))
    }
}

/// Volume implementation for redb backend.
pub struct RedbVolume {
    admin_status: serde_json::Value,
    /// The directory every storage of this volume keeps its database in.
    root: PathBuf,
    /// The history mode every storage on this volume inherits.
    history: HistoryMode,
}

#[async_trait]
impl Volume for RedbVolume {
    fn get_admin_status(&self) -> JsonValue {
        (&self.admin_status).into()
    }

    /// Report what this volume can do.
    ///
    /// The storage manager makes two decisions from this that a storage cannot
    /// override, which is why the mode is configured per volume:
    ///
    /// * a storage declaring `replication` refuses to start unless this says
    ///   `History::Latest`;
    /// * in `Latest` mode the manager drops outdated samples before they reach the
    ///   backend, and in `All` mode it forwards every sample.
    fn get_capability(&self) -> Capability {
        Capability {
            persistence: Persistence::Durable,
            history: match self.history {
                HistoryMode::Latest => History::Latest,
                HistoryMode::All => History::All,
            },
        }
    }

    async fn create_storage(&self, config: StorageConfig) -> ZResult<Box<dyn Storage>> {
        debug!("Creating redb storage with config: {:?}", config);

        let cfg = config.volume_cfg.into_serde_value();
        let volume_cfg = match cfg.as_object() {
            Some(v) => v,
            None => bail!("redb backed storages need volume-specific configurations"),
        };

        // Parse read_only property
        let read_only = match volume_cfg.get(PROP_STORAGE_READ_ONLY) {
            None | Some(serde_json::Value::Bool(false)) => false,
            Some(serde_json::Value::Bool(true)) => true,
            _ => {
                bail!(
                    "Optional property `{}` of redb storage configurations must be a boolean",
                    PROP_STORAGE_READ_ONLY
                )
            }
        };

        // Parse create_db property
        let create_db = match volume_cfg.get(PROP_STORAGE_CREATE_DB) {
            None | Some(serde_json::Value::Bool(true)) => true,
            Some(serde_json::Value::Bool(false)) => false,
            _ => {
                bail!(
                    "Optional property `{}` of redb storage configurations must be a boolean",
                    PROP_STORAGE_CREATE_DB
                )
            }
        };

        // Parse fsync property
        let fsync = match volume_cfg.get(PROP_STORAGE_FSYNC) {
            None | Some(serde_json::Value::Bool(true)) => true,
            Some(serde_json::Value::Bool(false)) => false,
            _ => {
                bail!(
                    "Optional property `{}` of redb storage configurations must be a boolean",
                    PROP_STORAGE_FSYNC
                )
            }
        };

        // Parse flush_interval_secs property
        let flush_interval_secs = match volume_cfg.get(PROP_STORAGE_FLUSH_INTERVAL) {
            None => None,
            Some(serde_json::Value::Number(n)) if n.as_u64().is_some() => n.as_u64(),
            _ => {
                bail!(
                    "Optional property `{}` of redb storage configurations must be a \
                     non-negative whole number of seconds",
                    PROP_STORAGE_FLUSH_INTERVAL
                )
            }
        };

        // Parse max_samples_per_get property
        let max_samples_per_get = match volume_cfg.get(PROP_STORAGE_MAX_SAMPLES_PER_GET) {
            None => None,
            Some(serde_json::Value::Number(n)) if n.as_u64().is_some() => n.as_u64(),
            _ => {
                bail!(
                    "Optional property `{}` of redb storage configurations must be a \
                     non-negative whole number (0 for no limit)",
                    PROP_STORAGE_MAX_SAMPLES_PER_GET
                )
            }
        };

        // Parse cache_size property
        let cache_size = match volume_cfg.get(PROP_STORAGE_CACHE_SIZE) {
            None => None,
            Some(serde_json::Value::Number(n)) => {
                if let Some(size) = n.as_u64() {
                    Some(size as usize)
                } else {
                    bail!(
                        "Optional property `{}` of redb storage configurations must be a positive number",
                        PROP_STORAGE_CACHE_SIZE
                    )
                }
            }
            _ => {
                bail!(
                    "Optional property `{}` of redb storage configurations must be a number",
                    PROP_STORAGE_CACHE_SIZE
                )
            }
        };

        let db_path = resolve_db_path(&self.root, volume_cfg, &config.name)?;
        // A name may have directories in it (`dir: "fleet/telemetry"`); make them,
        // unless the database is not supposed to be created at all.
        if create_db
            && !read_only
            && let Some(parent) = db_path.parent()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                zerror!(
                    "Storage '{}': cannot create {:?}: {}",
                    config.name,
                    parent,
                    e
                )
            })?;
        }

        // Create storage configuration
        let mut storage_config = RedbStorageConfig::new()
            .with_create_db(create_db)
            .with_read_only(read_only)
            .with_fsync(fsync);

        if let Some(size) = cache_size {
            storage_config = storage_config.with_cache_size(size);
        }
        if let Some(secs) = flush_interval_secs {
            storage_config = storage_config.with_flush_interval_secs(secs);
        }
        if let Some(max) = max_samples_per_get {
            storage_config = storage_config.with_max_samples_per_get(max);
        }
        storage_config = storage_config.with_history(self.history);

        // Retention. Parsed from the storage's volume config, not the volume's own,
        // because how long to keep telemetry is a per-storage decision even though
        // *whether* history is kept is not.
        let retention = match volume_cfg.get(PROP_STORAGE_RETENTION) {
            None => None,
            Some(value) => {
                let policy: RetentionPolicy =
                    serde_json::from_value(value.clone()).map_err(|e| {
                        zerror!(
                            "Storage '{}': invalid `{}`: {}",
                            config.name,
                            PROP_STORAGE_RETENTION,
                            e
                        )
                    })?;
                if let Err(reason) = policy.validate() {
                    bail!(
                        "Storage '{}': `{}` {}",
                        config.name,
                        PROP_STORAGE_RETENTION,
                        reason
                    )
                }
                Some(policy)
            }
        };

        // Retention only has something to act on in `all` mode: the policy prunes
        // the history tables, which a latest-value storage never writes to. A
        // policy configured here would spawn a task, report a pass on the admin
        // space every interval, and reclaim nothing — protection that looks real
        // and is not. Refuse it rather than let it read as a bound.
        if self.history != HistoryMode::All && retention.is_some() {
            bail!(
                "Storage '{}' declares `{}` but its volume is `history: \"latest\"`, \
                 which keeps one value per key and has no history to prune. A policy \
                 here would report passes on the admin space while reclaiming \
                 nothing. Either drop it, or move this storage to a \
                 `history: \"all\"` volume.",
                config.name,
                PROP_STORAGE_RETENTION
            )
        }

        // An `all`-mode storage without retention is an unbounded disk write, and
        // Zenoh storages have no TTL to fall back on: the storage manager's
        // `garbage_collection` prunes its own metadata and never touches stored
        // values. Refuse to start rather than fill a disk quietly — a loud config
        // error is recoverable in seconds, a full pool is not.
        if self.history == HistoryMode::All && retention.is_none() {
            bail!(
                "Storage '{}' is on a `history: \"all\"` volume but declares no \
                 `{}` policy. An all-mode storage keeps every sample forever, and \
                 Zenoh has no TTL — `garbage_collection` prunes metadata, not \
                 values. Add a retention policy, e.g. \
                 `retention: {{ max_age_secs: 2592000, max_bytes: 10737418240 }}`.",
                config.name,
                PROP_STORAGE_RETENTION
            )
        }

        if let Some(policy) = retention.clone() {
            storage_config = storage_config.with_retention(policy);
        }

        let redb_storage = RedbStorage::open(&db_path, storage_config.clone(), &config.name)
            .map_err(|e| zerror!("Failed to open redb storage '{}': {}", config.name, e))?;

        Ok(Box::new(RedbStoragePlugin::new(
            config,
            Arc::new(redb_storage),
            storage_config,
        )))
    }
}

/// Storage implementation for redb backend.
struct RedbStoragePlugin {
    config: StorageConfig,
    /// The storage itself. Every `RedbStorage` method takes `&self` and redb does
    /// its own concurrency control, so nothing here needs a lock: `put` and
    /// `delete` compare timestamps and write inside one redb write transaction,
    /// and redb admits one writer at a time.
    storage: Arc<RedbStorage>,
    storage_config: RedbStorageConfig,
    /// What the background thread reports back: the last retention pass and the
    /// usage snapshot. Std mutexes inside, because `get_admin_status` is
    /// synchronous and each critical section is a struct copy.
    shared: Arc<Shared>,
    /// Requests to the background thread. Dropping it — which happens when this
    /// struct drops — is what stops the thread.
    requests: Sender<Request>,
    /// File size up to which usage is computed inside the admin query. A field
    /// only so a test can exercise the snapshot path without a 64 MiB database.
    inline_usage_max_bytes: u64,
    /// Whether the "this volume has no history" warning has been logged.
    warned_about_time: AtomicBool,
}

impl RedbStoragePlugin {
    /// Wrap an open storage and start its background thread.
    fn new(
        config: StorageConfig,
        storage: Arc<RedbStorage>,
        storage_config: RedbStorageConfig,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        let (requests, requests_rx) = std::sync::mpsc::channel();

        let writable = !storage_config.read_only;
        let retention_interval = storage_config
            .retention
            .as_ref()
            .filter(|_| writable)
            .map(|policy| Duration::from_secs(policy.interval_secs.max(1)));
        // Only an `fsync: false` storage has anything to flush. At least a second
        // between flushes: `flush_interval_secs: 0` already makes every commit
        // durable, so the timer would find nothing and need not spin.
        let flush_interval = (writable && !storage_config.fsync)
            .then(|| Duration::from_secs(storage_config.flush_interval_secs.max(1)));

        spawn_background(
            storage.clone(),
            shared.clone(),
            requests_rx,
            retention_interval,
            flush_interval,
            config.name.clone(),
        );

        Self {
            config,
            storage,
            storage_config,
            shared,
            requests,
            inline_usage_max_bytes: INLINE_USAGE_MAX_BYTES,
            warned_about_time: AtomicBool::new(false),
        }
    }

    /// The storage's [`StorageUsage`] and its age, without ever making an admin
    /// query wait for a large database to be read.
    ///
    /// A small file is walked here and now: it fits in the page cache and the
    /// answer is exact. A large one is served from the last snapshot, and a
    /// snapshot that is missing or stale is recomputed on the background thread —
    /// so the first query after startup reports no usage yet, and the next one
    /// does.
    fn usage(&self, on_disk_bytes: Option<u64>) -> Option<(StorageUsage, Duration)> {
        // A storage that is out of service cannot be read, and `health` already
        // says so; asking anyway would only log a failure per admin query.
        if self.storage.needs_recovery() {
            return None;
        }
        if on_disk_bytes.is_none_or(|bytes| bytes <= self.inline_usage_max_bytes) {
            return match self.storage.usage() {
                Ok(usage) => Some((usage, Duration::ZERO)),
                Err(e) => {
                    warn!("Failed to collect usage of '{}': {}", self.config.name, e);
                    None
                }
            };
        }

        let snapshot = self
            .shared
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .map(|(usage, taken)| (usage, taken.elapsed()));

        let stale = snapshot
            .as_ref()
            .is_none_or(|(_, age)| *age >= USAGE_MAX_AGE);
        if stale
            && !self
                .shared
                .usage_refresh_pending
                .swap(true, Ordering::AcqRel)
            && self.requests.send(Request::RefreshUsage).is_err()
        {
            // The thread is gone; do not leave the flag claiming one is coming.
            self.shared
                .usage_refresh_pending
                .store(false, Ordering::Release);
        }

        snapshot
    }
}

#[async_trait]
impl Storage for RedbStoragePlugin {
    /// Report the storage's configuration *and* what it currently costs.
    ///
    /// This is the hook Zenoh gives a backend to describe itself on the admin
    /// space, and it is already read by `zenctl storage list` and the GUI's storage
    /// panel. Reporting only the config let an operator see that a storage existed
    /// but not what it was consuming — and "something grew and nobody was watching
    /// the number" is the shape of most storage incidents.
    fn get_admin_status(&self) -> JsonValue {
        let mut status = self.config.to_json_value();

        let capability = json!({
            "persistence": "durable",
            "history": self.storage_config.history.as_str(),
        });

        let stats = match self.storage.stats() {
            Ok(stats) => {
                let mut cache = json!({
                    "size_bytes": stats.cache_size_bytes,
                    "used_bytes": stats.cache_used_bytes,
                    "read_hits": stats.cache_read_hits,
                    "read_misses": stats.cache_read_misses,
                    "evictions": stats.cache_evictions,
                });
                // Absent rather than 0.0 before any read: a ratio nobody has
                // measured yet must not look like a cache that is missing every
                // time.
                if let Some(ratio) = stats.cache_hit_ratio() {
                    cache["hit_ratio"] = json!(ratio);
                }

                // The half that needs the tables read. `null` until there is a
                // snapshot, never a number that was not measured.
                let usage = self.usage(stats.on_disk_bytes);
                let (usage, usage_age) = match &usage {
                    Some((usage, age)) => (Some(usage), Some(age.as_secs())),
                    None => (None, None),
                };

                json!({
                    "on_disk_bytes": stats.on_disk_bytes,
                    "key_count": stats.key_count,
                    "sample_count": stats.sample_count,
                    "truncated_gets": stats.truncated_gets,
                    "cache": cache,
                    "stored_bytes": usage.map(|u| u.stored_bytes),
                    "metadata_bytes": usage.map(|u| u.metadata_bytes),
                    "fragmented_bytes": usage.map(|u| u.fragmented_bytes),
                    "live_keys": usage.map(|u| u.live_keys),
                    "tombstones": usage.map(|u| u.tombstones),
                    "oldest_timestamp": usage
                        .and_then(|u| u.oldest_timestamp)
                        .map(|t| t.to_string()),
                    "newest_timestamp": usage
                        .and_then(|u| u.newest_timestamp)
                        .map(|t| t.to_string()),
                    // How old the seven fields above are. 0 when they were
                    // computed for this very query; null when not computed yet.
                    "usage_age_secs": usage_age,
                })
            }
            Err(e) => {
                // Never fail the admin space over statistics: a storage that is
                // serving correctly must still report itself. And one that is out
                // of service says so under `health`, once, not here per query.
                if !self.storage.needs_recovery() {
                    warn!("Failed to collect storage statistics: {}", e);
                }
                json!({ "error": e.to_string() })
            }
        };

        // Retention: the policy in force, and what the last pass actually did. The
        // second half is the point — a declared policy is not evidence that anything
        // is being reclaimed.
        let pass = self
            .shared
            .last_pass
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let retention = json!({
            "policy": self.storage_config.retention,
            "last_pass": pass.ran_at.map(|_| json!({
                "samples_dropped": pass.samples_dropped,
                "samples_evicted_for_size": pass.samples_evicted_for_size,
                "samples_before": pass.samples_before,
                "samples_after": pass.samples_after,
                // The file. It only shrinks when `compacted` is true: redb reuses
                // freed pages but does not return them to the filesystem.
                "bytes_before": pass.bytes_before,
                "bytes_after": pass.bytes_after,
                "compacted": pass.compacted,
                "keys_expired": pass.keys_expired,
                "duration_ms": pass.duration.map(|d| d.as_millis() as u64),
            })),
        });

        // Whether the storage is serving at all. `stats` alone cannot say: a
        // storage whose database has failed reports `stats: { error }`, which
        // reads the same as a statistics bug.
        let health = self.storage.health();
        let health = json!({
            "state": health.state.as_str(),
            "last_error": health.last_error,
            "failed_at": health.failed_at.map(epoch_secs),
            "reopens": health.reopens,
        });

        if let Some(obj) = status.as_object_mut() {
            obj.insert("health".to_string(), health);
            obj.insert("capability".to_string(), capability);
            obj.insert("retention".to_string(), retention);
            obj.insert("stats".to_string(), stats);
            obj.insert(
                "db_path".to_string(),
                json!(self.storage.path().to_string_lossy()),
            );
        }

        status.into()
    }

    async fn put(
        &mut self,
        key: Option<OwnedKeyExpr>,
        payload: ZBytes,
        encoding: Encoding,
        timestamp: Timestamp,
    ) -> ZResult<StorageInsertionResult> {
        let storage = &self.storage;

        if self.storage_config.read_only {
            warn!("Received PUT for read-only DB on {:?} - ignored", key);
            return Err("Received update for read-only DB".into());
        }

        let key_str = match key {
            Some(k) => k.to_string(),
            None => NONE_KEY.to_string(),
        };

        debug!("Storing key: {} with timestamp: {}", key_str, timestamp);

        // Convert ZBytes to Vec<u8>
        let payload_bytes = payload.to_bytes().to_vec();

        // Create stored value with native Zenoh timestamp (preserves both time and ID)
        let value = StoredValue::new(payload_bytes, timestamp, encoding);

        // Last-writer-wins is decided inside the write transaction, not trusted from
        // upstream. The storage manager does filter outdated samples before calling
        // us, but against an in-memory cache seeded from `get_all_entries` at startup
        // (`storages_mgt/service.rs`, `guard_cache_if_latest`) — per-process and
        // per-storage, so replay, alignment and a restarted manager all bypass it.
        let outcome = storage
            .put(&key_str, value)
            .map_err(|e| zerror!("Failed to put key '{}': {}", key_str, e))?;

        Ok(match outcome {
            WriteOutcome::Inserted => StorageInsertionResult::Inserted,
            WriteOutcome::Replaced => StorageInsertionResult::Replaced,
            WriteOutcome::Outdated => {
                debug!("Ignored outdated PUT for {} at {}", key_str, timestamp);
                StorageInsertionResult::Outdated
            }
        })
    }

    async fn delete(
        &mut self,
        key: Option<OwnedKeyExpr>,
        timestamp: Timestamp,
    ) -> ZResult<StorageInsertionResult> {
        let storage = &self.storage;

        if self.storage_config.read_only {
            warn!("Received DELETE for read-only DB on {:?} - ignored", key);
            return Err("Received update for read-only DB".into());
        }

        let key_str = match key {
            Some(k) => k.to_string(),
            None => NONE_KEY.to_string(),
        };

        debug!("Deleting key: {} with timestamp: {}", key_str, timestamp);

        // Last-writer-wins is decided inside the write transaction, the same way
        // `put` decides it — a DELETE that predates the value we hold must not
        // remove it. In `all` mode the tombstone is still appended to the history
        // either way, because "this key was deleted at t" is a fact about t
        // regardless of what arrived afterwards.
        //
        // Deleting an absent key is not an error: the storage manager replays
        // deletions during alignment and expects them to be idempotent.
        let outcome = storage
            .delete(&key_str, timestamp)
            .map_err(|e| zerror!("Failed to delete key '{}': {}", key_str, e))?;

        Ok(match outcome {
            WriteOutcome::Outdated => {
                debug!("Ignored outdated DELETE for {} at {}", key_str, timestamp);
                StorageInsertionResult::Outdated
            }
            _ => StorageInsertionResult::Deleted,
        })
    }

    async fn get(
        &mut self,
        key: Option<OwnedKeyExpr>,
        parameters: &str,
    ) -> ZResult<Vec<StoredData>> {
        let storage = &self.storage;

        let key_str = match key {
            Some(k) => k.to_string(),
            None => NONE_KEY.to_string(),
        };

        debug!("Getting key: {}", key_str);

        // A `_time`-ranged GET is the whole point of `history: "all"`. The selector's
        // query parameters reach a backend verbatim, and Zenoh ships the parser, so
        // both documented syntaxes work: `[start..end]` and `[start;duration]`, with
        // `now(-1h)`-style relative expressions.
        if let Some(range) = parse_time_range(parameters)? {
            if self.storage_config.history != HistoryMode::All {
                // Answering a time range from a latest-only storage would silently
                // return one sample and look like "there was no other data". Said
                // once per storage: a dashboard that sends `_time` everywhere would
                // otherwise write this line per key per refresh.
                if !self.warned_about_time.swap(true, Ordering::Relaxed) {
                    warn!(
                        "Ignoring `_time` on storage '{}': its volume is history: \"latest\", \
                         which keeps one value per key. Configure a volume with \
                         history: \"all\" to serve time ranges. (Logged once.)",
                        self.config.name
                    );
                }
            } else {
                let samples = storage
                    .get_range(&key_str, &range)
                    .map_err(|e| zerror!("Failed to range-get key '{}': {}", key_str, e))?;

                return Ok(samples
                    .into_iter()
                    .map(|v| StoredData {
                        payload: ZBytes::from(v.payload),
                        encoding: v.encoding,
                        timestamp: v.timestamp,
                    })
                    .collect());
            }
        }

        match storage
            .get(&key_str)
            .map_err(|e| zerror!("Failed to get key '{}': {}", key_str, e))?
        {
            Some(stored_value) => {
                // Convert back to Zenoh types
                let payload = ZBytes::from(stored_value.payload);
                let encoding = stored_value.encoding.clone();
                let timestamp = stored_value.timestamp;

                Ok(vec![StoredData {
                    payload,
                    encoding,
                    timestamp,
                }])
            }
            None => Ok(vec![]),
        }
    }

    async fn get_all_entries(&self) -> ZResult<Vec<(Option<OwnedKeyExpr>, Timestamp)>> {
        let storage = &self.storage;

        debug!("Getting all entries");

        // Metadata only. The storage manager calls this to resolve *every* wildcard
        // query (it intersects the selector itself, then issues per-key GETs), so
        // reading payloads here would mean loading the whole database into memory
        // and throwing it away on each one.
        let entries = storage
            .get_all_timestamps()
            .map_err(|e| zerror!("Failed to get all entries: {}", e))?;

        let mut result = Vec::new();
        for (key_str, timestamp) in entries {
            // Convert key string back to OwnedKeyExpr
            let key_expr = if key_str == NONE_KEY {
                None
            } else {
                match OwnedKeyExpr::new(key_str.as_str()) {
                    Ok(ke) => Some(ke),
                    Err(e) => {
                        warn!("Invalid key in database: '{}' - {}", key_str, e);
                        continue;
                    }
                }
            };

            result.push((key_expr, timestamp));
        }

        debug!("Retrieved {} entries", result.len());
        Ok(result)
    }
}

impl Drop for RedbStoragePlugin {
    fn drop(&mut self) {
        // The background thread stops when `requests` drops with this struct.
        // Without that it would outlive the storage and go on compacting a database
        // nobody is serving from.
        debug!("Dropping redb storage plugin '{}'", self.config.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zenoh::time::NTP64;

    #[test]
    fn test_plugin_constants() {
        assert_eq!(RedbBackendPlugin::DEFAULT_NAME, "redb_backend");
        assert!(!RedbBackendPlugin::PLUGIN_VERSION.is_empty());
        assert!(!RedbBackendPlugin::PLUGIN_LONG_VERSION.is_empty());
    }

    #[test]
    fn test_default_root_dir() {
        assert_eq!(DEFAULT_ROOT_DIR, "zenoh_backend_redb");
    }

    #[test]
    fn test_environment_variable_constant() {
        assert_eq!(SCOPE_ENV_VAR, "ZENOH_BACKEND_REDB_ROOT");
    }

    #[test]
    fn test_property_constants() {
        assert_eq!(PROP_STORAGE_DIR, "dir");
        assert_eq!(PROP_STORAGE_DB_FILE, "db_file");
        assert_eq!(PROP_STORAGE_CREATE_DB, "create_db");
        assert_eq!(PROP_STORAGE_READ_ONLY, "read_only");
        assert_eq!(PROP_STORAGE_CACHE_SIZE, "cache_size");
        assert_eq!(PROP_STORAGE_FSYNC, "fsync");
        assert_eq!(PROP_STORAGE_FLUSH_INTERVAL, "flush_interval_secs");
        assert_eq!(PROP_STORAGE_MAX_SAMPLES_PER_GET, "max_samples_per_get");
    }

    #[test]
    fn test_redb_volume_structure() {
        let dir = TempDir::new().unwrap();

        let cap = test_volume(&dir, HistoryMode::Latest).get_capability();
        assert_eq!(cap.persistence, Persistence::Durable);
        assert_eq!(cap.history, History::Latest);

        let cap = test_volume(&dir, HistoryMode::All).get_capability();
        assert_eq!(cap.history, History::All);
    }

    #[tokio::test]
    async fn test_storage_plugin_drop() {
        // Test that RedbStoragePlugin drop doesn't panic
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new().with_create_db(true);

        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let storage_plugin = RedbStoragePlugin::new(
            test_storage_config("test"),
            Arc::new(redb_storage),
            storage_config,
        );

        // Drop should work without panic
        drop(storage_plugin);
    }

    #[tokio::test]
    async fn test_storage_plugin_admin_status() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new().with_create_db(true);

        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let storage_plugin = RedbStoragePlugin::new(
            test_storage_config("test_storage"),
            Arc::new(redb_storage),
            storage_config,
        );

        let admin_status = storage_plugin.get_admin_status();

        // Just verify we can get admin status without panicking
        // The actual structure is implementation detail
        let _ = admin_status;
    }

    #[tokio::test]
    async fn test_storage_put_and_get() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new().with_create_db(true);

        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin::new(
            test_storage_config("test"),
            Arc::new(redb_storage),
            storage_config,
        );

        // Put data
        let key = OwnedKeyExpr::new("test/key1").unwrap();
        let payload = ZBytes::from("test_value");
        let encoding = Encoding::ZENOH_STRING;
        let timestamp = Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand());

        let result = storage_plugin
            .put(Some(key.clone()), payload.clone(), encoding, timestamp)
            .await;
        assert!(result.is_ok());

        // Get data
        let result = storage_plugin.get(Some(key), "").await;
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].payload.to_bytes(), payload.to_bytes());
    }

    /// The storage-manager side of a storage's config, for a plugin built by hand.
    fn test_storage_config(name: &str) -> StorageConfig {
        StorageConfig {
            name: name.to_string(),
            key_expr: "test/**".parse().unwrap(),
            strip_prefix: None,
            volume_cfg: serde_json::Value::Object(Default::default()).into(),
            volume_id: "test_volume".to_string(),
            complete: false,
            garbage_collection_config: Default::default(),
            replication: None,
        }
    }

    /// A volume rooted in `dir`, as `RedbBackendPlugin::start` would build it.
    fn test_volume(dir: &TempDir, history: HistoryMode) -> RedbVolume {
        RedbVolume {
            history,
            admin_status: serde_json::Value::Object(Default::default()),
            root: dir.path().to_path_buf(),
        }
    }

    fn storage_config_for(volume_cfg: serde_json::Value, _dir: &TempDir) -> StorageConfig {
        StorageConfig {
            name: "s".to_string(),
            key_expr: "test/**".parse().unwrap(),
            strip_prefix: None,
            volume_cfg: volume_cfg.into(),
            volume_id: "redb".to_string(),
            complete: false,
            garbage_collection_config: Default::default(),
            replication: None,
        }
    }

    /// An `all`-mode storage with no retention policy must refuse to start.
    ///
    /// Zenoh storages have no TTL — the manager's `garbage_collection` prunes its
    /// own metadata, never stored values — so an unbounded all-mode storage fills a
    /// disk quietly. A loud config error is recoverable in seconds; a full pool is
    /// not.
    #[tokio::test]
    async fn test_all_mode_without_retention_refuses_to_start() {
        let dir = TempDir::new().unwrap();
        let volume = test_volume(&dir, HistoryMode::All);

        let Err(err) = volume
            .create_storage(storage_config_for(json!({ "dir": "unbounded" }), &dir))
            .await
        else {
            panic!("an all-mode storage with no retention must not start");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("retention"),
            "the error must name the missing policy, got: {msg}"
        );

        // With a policy, it starts.
        volume
            .create_storage(storage_config_for(
                json!({ "dir": "bounded", "retention": { "max_age_secs": 3600 } }),
                &dir,
            ))
            .await
            .map(|_| ())
            .expect("an all-mode storage with a policy must start");
    }

    /// A retention policy on a latest-mode volume must be rejected, not silently
    /// ignored: it would spawn a task that reports passes on the admin space while
    /// reclaiming nothing, which reads as a bound that is not there.
    #[tokio::test]
    async fn test_retention_on_a_latest_volume_is_rejected() {
        let dir = TempDir::new().unwrap();
        let volume = test_volume(&dir, HistoryMode::Latest);

        let Err(err) = volume
            .create_storage(storage_config_for(
                json!({ "dir": "bounded-latest", "retention": { "max_age_secs": 60 } }),
                &dir,
            ))
            .await
        else {
            panic!("a retention policy on a latest-mode volume must be rejected");
        };
        assert!(err.to_string().contains("latest"), "{err}");
    }

    /// Background work that panics must not take the thread with it: the next
    /// pass has to run, or retention ends silently for that storage.
    #[test]
    fn test_panicking_background_work_is_contained() {
        let panicked: Option<RetentionPass> =
            contained("s", "Retention pass", || panic!("a bug in one pass"));
        assert!(panicked.is_none());

        let failed: Option<RetentionPass> = contained("s", "Retention pass", || {
            Err(crate::error::RedbBackendError::other("io"))
        });
        assert!(failed.is_none());

        // ...and the thread is still here to run the next one.
        let ok = contained("s", "Retention pass", || {
            Ok(RetentionPass {
                samples_dropped: 7,
                ..Default::default()
            })
        });
        assert_eq!(ok.map(|p| p.samples_dropped), Some(7));
    }

    /// Values that parse but cannot mean what they say stop the storage starting.
    #[tokio::test]
    async fn test_a_retention_policy_with_a_typo_or_a_zero_is_rejected() {
        let dir = TempDir::new().unwrap();
        let volume = test_volume(&dir, HistoryMode::All);

        for (retention, names) in [
            (json!({ "max_age_secs": 60, "max_byte": 1000 }), "max_byte"),
            (
                json!({ "max_age_secs": 60, "interval_secs": 0 }),
                "interval_secs",
            ),
            (
                json!({ "decimate": { "recent_secs": 60, "bucket_secs": 0 } }),
                "bucket_secs",
            ),
        ] {
            let Err(err) = volume
                .create_storage(storage_config_for(
                    json!({ "dir": "bad", "retention": retention }),
                    &dir,
                ))
                .await
            else {
                panic!("a policy naming `{names}` wrongly must not start");
            };
            assert!(err.to_string().contains(names), "{err}");
        }
    }

    /// #39: `.redb` is appended to `dir`, so a dot in it survives and two storages
    /// cannot land on one file.
    #[test]
    fn test_a_dot_in_dir_is_part_of_the_name() {
        let base = TempDir::new().unwrap();
        let base = base.path();

        assert_eq!(db_path_for_dir(base, "plain", "s"), base.join("plain.redb"));
        assert_eq!(
            db_path_for_dir(base, "telemetry.v1", "s"),
            base.join("telemetry.v1.redb")
        );
        assert_ne!(
            db_path_for_dir(base, "telemetry.v1", "s"),
            db_path_for_dir(base, "telemetry.v2", "s"),
            "two dirs that differ after the dot must not share a file"
        );
        // Writing the extension out does not double it.
        assert_eq!(
            db_path_for_dir(base, "named.redb", "s"),
            base.join("named.redb")
        );

        // A file under the truncated name is never opened in a dotted dir's place:
        // it is just as likely to be the rightful file of `dir: "telemetry"`.
        std::fs::write(base.join("telemetry.redb"), b"").unwrap();
        assert_eq!(
            db_path_for_dir(base, "telemetry.v1", "s"),
            base.join("telemetry.v1.redb")
        );
        assert_eq!(
            db_path_for_dir(base, "telemetry.v2", "s"),
            base.join("telemetry.v2.redb")
        );
        assert_eq!(
            db_path_for_dir(base, "telemetry", "s"),
            base.join("telemetry.redb")
        );
    }

    /// #49: the sample whose key is the storage's whole `strip_prefix` (no key
    /// left) and a sample whose stripped key happens to be `@@none_key@@` are two
    /// different entries. Format 1 stored both on one row.
    #[tokio::test]
    async fn test_a_keyless_sample_does_not_collide_with_a_real_key() {
        let dir = TempDir::new().unwrap();
        let volume = test_volume(&dir, HistoryMode::Latest);
        let mut storage = volume
            .create_storage(storage_config_for(json!({ "dir": "keys" }), &dir))
            .await
            .map_err(|e| e.to_string())
            .unwrap();
        let id = zenoh::time::TimestampId::rand();
        let lookalike = OwnedKeyExpr::new("@@none_key@@").unwrap();

        storage
            .put(
                None,
                ZBytes::from("keyless"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(100), id),
            )
            .await
            .unwrap();
        storage
            .put(
                Some(lookalike.clone()),
                ZBytes::from("lookalike"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(200), id),
            )
            .await
            .unwrap();

        let keyless = storage.get(None, "").await.unwrap();
        assert_eq!(keyless[0].payload.to_bytes().as_ref(), b"keyless");
        let real = storage.get(Some(lookalike.clone()), "").await.unwrap();
        assert_eq!(real[0].payload.to_bytes().as_ref(), b"lookalike");

        let mut entries: Vec<Option<String>> = storage
            .get_all_entries()
            .await
            .unwrap()
            .into_iter()
            .map(|(key, _)| key.map(|k| k.to_string()))
            .collect();
        entries.sort();
        assert_eq!(entries, vec![None, Some(lookalike.to_string())]);
    }

    /// #51: a `_time` GET returns at most `max_samples_per_get` samples, the newest
    /// ones, and the admin space counts how often that happened.
    #[tokio::test]
    async fn test_a_time_ranged_get_is_capped() {
        let dir = TempDir::new().unwrap();
        let volume = test_volume(&dir, HistoryMode::All);
        let mut storage = volume
            .create_storage(storage_config_for(
                json!({
                    "dir": "capped",
                    "max_samples_per_get": 10,
                    "retention": { "max_age_secs": 86400 },
                }),
                &dir,
            ))
            .await
            .map_err(|e| e.to_string())
            .unwrap();

        let key = OwnedKeyExpr::new("test/busy").unwrap();
        let id = zenoh::time::TimestampId::rand();
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        for i in 0..25u64 {
            storage
                .put(
                    Some(key.clone()),
                    ZBytes::from(format!("s{i:02}")),
                    Encoding::ZENOH_STRING,
                    Timestamp::new(NTP64::from(now - Duration::from_secs(60 - i)), id),
                )
                .await
                .unwrap();
        }

        let window = storage
            .get(Some(key.clone()), "_time=[now(-1h)..]")
            .await
            .unwrap();
        let payloads: Vec<String> = window
            .iter()
            .map(|s| String::from_utf8(s.payload.to_bytes().to_vec()).unwrap())
            .collect();
        assert_eq!(payloads.len(), 10);
        assert_eq!(payloads[0], "s15", "the newest ten, oldest of them first");
        assert_eq!(payloads[9], "s24");

        // A window that fits is not counted as cut.
        let narrow = storage
            .get(Some(key.clone()), "_time=[now(-40s)..]")
            .await
            .unwrap();
        assert!(narrow.len() <= 10);

        let status: serde_json::Value = serde_json::to_value(storage.get_admin_status()).unwrap();
        assert_eq!(status["stats"]["truncated_gets"], 1);

        // A cap that is not a whole number is a config error.
        let Err(err) = volume
            .create_storage(storage_config_for(
                json!({ "dir": "bad", "max_samples_per_get": "many",
                        "retention": { "max_age_secs": 60 } }),
                &dir,
            ))
            .await
        else {
            panic!("a non-numeric cap must be rejected");
        };
        assert!(err.to_string().contains("max_samples_per_get"), "{err}");
    }

    /// #50: `dir` and `db_file` name a database inside the root. Neither may point
    /// out of it.
    #[tokio::test]
    async fn test_a_storage_path_cannot_leave_the_root() {
        let outside = TempDir::new().unwrap();
        let dir = TempDir::new().unwrap();
        let volume = test_volume(&dir, HistoryMode::Latest);
        let absolute = outside.path().join("x.redb").to_string_lossy().to_string();

        for (property, value) in [
            ("db_file", absolute.as_str()),
            ("dir", absolute.as_str()),
            ("dir", "../escape"),
            ("db_file", "a/../../escape.redb"),
            ("dir", "./here"),
            ("dir", ""),
        ] {
            let Err(err) = volume
                .create_storage(storage_config_for(json!({ property: value }), &dir))
                .await
            else {
                panic!("`{property}: \"{value}\"` must be rejected");
            };
            let msg = err.to_string();
            assert!(msg.contains("storage root"), "{msg}");
            assert!(msg.contains(property), "{msg}");
        }
        assert_eq!(
            std::fs::read_dir(outside.path()).unwrap().count(),
            0,
            "nothing may have been created outside the root"
        );

        // A wrong type is said to be a wrong type.
        let Err(err) = volume
            .create_storage(storage_config_for(json!({ "dir": 5 }), &dir))
            .await
        else {
            panic!("a numeric dir must be rejected");
        };
        assert!(err.to_string().contains("must be a string"), "{err}");
        let Err(err) = volume
            .create_storage(storage_config_for(json!({}), &dir))
            .await
        else {
            panic!("a storage with no path must be rejected");
        };
        assert!(err.to_string().contains("is required"), "{err}");

        // Names inside the root work, directories included.
        for (property, value, file) in [
            ("dir", "plain", "plain.redb"),
            ("dir", "fleet/telemetry", "fleet/telemetry.redb"),
            ("db_file", "named.redb", "named.redb"),
            ("db_file", "bare", "bare.redb"),
        ] {
            volume
                .create_storage(storage_config_for(json!({ property: value }), &dir))
                .await
                .map(|_| ())
                .unwrap_or_else(|e| panic!("`{property}: \"{value}\"` must work: {e}"));
            assert!(dir.path().join(file).exists(), "{file}");
        }
    }

    /// #47: through the plugin, a `read_only` storage serves an existing database
    /// and creates nothing.
    #[tokio::test]
    async fn test_a_read_only_storage_serves_and_creates_nothing() {
        let dir = TempDir::new().unwrap();
        let volume = test_volume(&dir, HistoryMode::Latest);
        let key = OwnedKeyExpr::new("test/k").unwrap();

        // No database yet: a read-only storage has nothing to open, and must not
        // make one.
        let Err(err) = volume
            .create_storage(storage_config_for(
                json!({ "dir": "cfg", "read_only": true }),
                &dir,
            ))
            .await
        else {
            panic!("a read-only storage with no database must not start");
        };
        assert!(err.to_string().contains("does not exist"), "{err}");
        assert!(!dir.path().join("cfg.redb").exists());

        // Populate it writable, then serve it read-only.
        {
            let mut writable = volume
                .create_storage(storage_config_for(json!({ "dir": "cfg" }), &dir))
                .await
                .map_err(|e| e.to_string())
                .unwrap();
            writable
                .put(
                    Some(key.clone()),
                    ZBytes::from("v"),
                    Encoding::ZENOH_STRING,
                    Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand()),
                )
                .await
                .unwrap();
        }
        // The writable storage's background thread lets go of the database a
        // moment after the storage is dropped.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut storage = loop {
            match volume
                .create_storage(storage_config_for(
                    json!({ "dir": "cfg", "read_only": true }),
                    &dir,
                ))
                .await
            {
                Ok(storage) => break storage,
                // Only the one reason that goes away by waiting.
                Err(e) if e.to_string().contains("already open") => {
                    assert!(Instant::now() < deadline, "the writer never let go: {e}")
                }
                Err(e) => panic!("the read-only storage failed to open: {e}"),
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        assert_eq!(storage.get(Some(key.clone()), "").await.unwrap().len(), 1);
        assert!(
            storage
                .put(
                    Some(key),
                    ZBytes::from("w"),
                    Encoding::ZENOH_STRING,
                    Timestamp::new(NTP64(200), zenoh::time::TimestampId::rand()),
                )
                .await
                .is_err()
        );
        let status: serde_json::Value = serde_json::to_value(storage.get_admin_status()).unwrap();
        assert_eq!(status["stats"]["key_count"], 1);
    }

    /// Storages whose `dir`s differ only after a dot all start, each on its own
    /// file, including next to one whose file is the truncated name.
    #[tokio::test]
    async fn test_two_dotted_dirs_do_not_collide() {
        let dir = TempDir::new().unwrap();
        let volume = test_volume(&dir, HistoryMode::Latest);
        let cfg = |name: &str| StorageConfig {
            name: name.to_string(),
            key_expr: "test/**".parse().unwrap(),
            strip_prefix: None,
            volume_cfg: json!({ "dir": name }).into(),
            volume_id: "redb".to_string(),
            complete: false,
            garbage_collection_config: Default::default(),
            replication: None,
        };

        // An undotted storage first: its file is exactly what a dotted `dir` used
        // to be truncated to, and it must stay that storage's alone.
        let _plain = volume
            .create_storage(cfg("telemetry"))
            .await
            .map_err(|e| e.to_string())
            .expect("undotted storage");
        assert!(dir.path().join("telemetry.redb").exists());

        let _v1 = volume
            .create_storage(cfg("telemetry.v1"))
            .await
            .map_err(|e| e.to_string())
            .expect("first dotted storage must not land on the undotted one's file");
        let _v2 = volume
            .create_storage(cfg("telemetry.v2"))
            .await
            .map_err(|e| e.to_string())
            .expect("second storage must not collide with the first");

        assert!(dir.path().join("telemetry.v1.redb").exists());
        assert!(dir.path().join("telemetry.v2.redb").exists());
    }

    /// A latest-mode storage needs no policy — it already keeps one value per key.
    #[tokio::test]
    async fn test_latest_mode_needs_no_retention() {
        let dir = TempDir::new().unwrap();
        let volume = test_volume(&dir, HistoryMode::Latest);

        volume
            .create_storage(storage_config_for(json!({ "dir": "plain" }), &dir))
            .await
            .map(|_| ())
            .expect("a latest-mode storage needs no retention policy");
    }

    /// A retention block that sets no limit reads as protection that is not there.
    #[tokio::test]
    async fn test_empty_retention_policy_is_rejected() {
        let dir = TempDir::new().unwrap();
        let volume = test_volume(&dir, HistoryMode::All);

        let Err(err) = volume
            .create_storage(storage_config_for(
                json!({ "dir": "empty", "retention": {} }),
                &dir,
            ))
            .await
        else {
            panic!("a retention policy with no limits must be rejected");
        };
        assert!(err.to_string().contains("no limit"), "{err}");
    }

    /// A `_time`-ranged GET against an `all`-mode storage returns the window; the
    /// same GET without `_time` returns only the latest. That pair is the contract
    /// `History::All` exists to provide.
    #[tokio::test]
    async fn test_time_ranged_get_returns_the_window() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("history.redb");

        let storage_config = RedbStorageConfig::new()
            .with_create_db(true)
            .with_history(HistoryMode::All);

        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "history".to_string()).unwrap();

        let mut plugin = RedbStoragePlugin::new(
            test_storage_config("history"),
            Arc::new(redb_storage),
            storage_config,
        );

        let key = OwnedKeyExpr::new("test/cpu").unwrap();
        let id = zenoh::time::TimestampId::rand();

        // Five samples, one per second, ending "now" so that a relative `_time`
        // expression has something to find.
        let now = SystemTime::now();
        for i in 0..5u64 {
            let at = now - std::time::Duration::from_secs(10 - i);
            let ntp = NTP64::from(at.duration_since(std::time::UNIX_EPOCH).unwrap());
            plugin
                .put(
                    Some(key.clone()),
                    ZBytes::from(format!("sample{i}")),
                    Encoding::ZENOH_STRING,
                    Timestamp::new(ntp, id),
                )
                .await
                .unwrap();
        }

        // No `_time`: the latest value only.
        let latest = plugin.get(Some(key.clone()), "").await.unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].payload.to_bytes().as_ref(), b"sample4");

        // With `_time`: the whole window, oldest first.
        let windowed = plugin
            .get(Some(key.clone()), "_time=[now(-3600s)..now()]")
            .await
            .unwrap();
        assert_eq!(windowed.len(), 5, "every sample in the window");
        assert_eq!(windowed[0].payload.to_bytes().as_ref(), b"sample0");
        assert_eq!(windowed[4].payload.to_bytes().as_ref(), b"sample4");

        // A window that predates every sample returns nothing, rather than
        // falling back to the latest value.
        let empty = plugin
            .get(Some(key.clone()), "_time=[now(-7200s)..now(-3600s)]")
            .await
            .unwrap();
        assert!(empty.is_empty());

        // Other selector parameters must not be mistaken for a time range.
        let unrelated = plugin.get(Some(key), "foo=bar").await.unwrap();
        assert_eq!(unrelated.len(), 1);
    }

    /// A malformed `_time` must be an error, not a silent full-history reply:
    /// answering the wrong window is worse than refusing.
    #[test]
    fn test_malformed_time_range_is_rejected() {
        assert!(parse_time_range("_time=nonsense").is_err());
        assert!(parse_time_range("").unwrap().is_none());
        assert!(parse_time_range("foo=bar").unwrap().is_none());
        assert!(
            parse_time_range("_time=[now(-1h)..now()]")
                .unwrap()
                .is_some()
        );
    }

    /// The admin space must report what the storage costs, not just its config.
    #[tokio::test]
    async fn test_admin_status_reports_size_and_cache() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new()
            .with_create_db(true)
            .with_cache_size(8 * 1024 * 1024);

        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin::new(
            test_storage_config("test"),
            Arc::new(redb_storage),
            storage_config,
        );

        let id = zenoh::time::TimestampId::rand();
        for i in 0..5 {
            storage_plugin
                .put(
                    Some(OwnedKeyExpr::new(format!("test/k{i}")).unwrap()),
                    ZBytes::from(vec![0u8; 1024]),
                    Encoding::ZENOH_BYTES,
                    Timestamp::new(NTP64(100 + i), id),
                )
                .await
                .unwrap();
        }

        let status: serde_json::Value =
            serde_json::to_value(storage_plugin.get_admin_status()).unwrap();
        let stats = &status["stats"];

        assert_eq!(stats["key_count"], 5);
        assert_eq!(stats["live_keys"], 5);
        assert_eq!(stats["tombstones"], 0);

        // The size must be the real file, not an estimate summed from lengths.
        let on_disk = stats["on_disk_bytes"].as_u64().expect("on_disk_bytes");
        let actual = std::fs::metadata(&db_path).unwrap().len();
        assert_eq!(on_disk, actual);
        assert!(on_disk > 0);

        // Stored bytes must at least account for the payloads we wrote.
        assert!(
            stats["stored_bytes"].as_u64().unwrap() >= 5 * 1024,
            "stored_bytes {} should cover 5 KiB of payloads",
            stats["stored_bytes"]
        );

        assert_eq!(stats["cache"]["size_bytes"], 8 * 1024 * 1024);
        assert!(status["capability"]["persistence"] == "durable");

        // The timestamp span of what is held.
        assert!(stats["oldest_timestamp"].is_string());
        assert!(stats["newest_timestamp"].is_string());
    }

    /// #48: a storage whose disk filled up comes back by itself once there is
    /// space, without anyone restarting the router, and says what happened.
    #[tokio::test]
    async fn test_a_failed_storage_reopens_itself() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("enospc.redb");
        let disk_full = crate::storage::faults::inject(&db_path);
        let storage_config = RedbStorageConfig::new();
        let storage =
            Arc::new(RedbStorage::open(&db_path, storage_config.clone(), "enospc").unwrap());
        let mut plugin = RedbStoragePlugin::new(
            test_storage_config("enospc"),
            storage.clone(),
            storage_config,
        );

        let id = zenoh::time::TimestampId::rand();
        let key = |name: &str| Some(OwnedKeyExpr::new(format!("test/{name}")).unwrap());
        let put = |n: u64| {
            (
                ZBytes::from("v"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(n), id),
            )
        };

        let (payload, encoding, timestamp) = put(100);
        plugin
            .put(key("before"), payload, encoding, timestamp)
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::to_value(plugin.get_admin_status()).unwrap();
        assert_eq!(status["health"]["state"], "ok");
        assert_eq!(status["health"]["reopens"], 0);

        // The disk fills: writes fail, and the admin space says the storage is down
        // and why.
        disk_full.store(true, Ordering::Release);
        let (payload, encoding, timestamp) = put(200);
        assert!(
            plugin
                .put(key("during"), payload, encoding, timestamp)
                .await
                .is_err()
        );
        assert!(plugin.get(key("before"), "").await.is_err());
        let status: serde_json::Value = serde_json::to_value(plugin.get_admin_status()).unwrap();
        assert_eq!(status["health"]["state"], "recovering", "{status}");
        assert!(status["health"]["last_error"].is_string());
        assert!(status["health"]["failed_at"].as_u64().unwrap() > 0);

        // Space is freed. Nobody tells the storage; its background thread finds out.
        disk_full.store(false, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(20);
        while storage.needs_recovery() {
            assert!(Instant::now() < deadline, "the storage never came back");
            std::thread::sleep(Duration::from_millis(50));
        }

        assert_eq!(plugin.get(key("before"), "").await.unwrap().len(), 1);
        let (payload, encoding, timestamp) = put(300);
        plugin
            .put(key("after"), payload, encoding, timestamp)
            .await
            .unwrap();
        let status: serde_json::Value = serde_json::to_value(plugin.get_admin_status()).unwrap();
        assert_eq!(status["health"]["state"], "ok");
        assert_eq!(status["health"]["reopens"], 1);
        assert_eq!(status["stats"]["key_count"], 2);
    }

    /// The wake-up time never lies in the past because of a timer that is not being
    /// served: that is a thread spinning at full CPU.
    #[test]
    fn test_next_wake_ignores_retention_and_flush_while_recovering() {
        let now = Instant::now();
        let past = now - Duration::from_secs(60);
        let soon = now + Duration::from_millis(200);
        let later = now + Duration::from_secs(600);

        // In service: the earliest timer, or the health poll if that comes first.
        assert_eq!(next_wake(now, false, [Some(soon), Some(later)], None), soon);
        assert_eq!(
            next_wake(now, false, [Some(later), None], None),
            now + HEALTH_POLL
        );
        assert_eq!(next_wake(now, false, [None, None], None), now + HEALTH_POLL);
        assert_eq!(next_wake(now, false, [Some(past), None], None), past);

        // Recovering: overdue retention and flush deadlines are not waited on.
        assert_eq!(
            next_wake(now, true, [Some(past), Some(past)], Some(later)),
            now + HEALTH_POLL
        );
        assert_eq!(
            next_wake(now, true, [Some(past), Some(past)], Some(soon)),
            soon
        );
        assert_eq!(
            next_wake(now, true, [Some(past), Some(past)], None),
            now + HEALTH_POLL
        );
    }

    /// #48: an outage that lasts past a flush deadline must not turn the
    /// background thread into a busy loop. It did: the overdue deadline was still
    /// waited on, so every wait returned at once.
    #[tokio::test]
    async fn test_a_recovering_storage_does_not_spin() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("down.redb");
        let disk_full = crate::storage::faults::inject(&db_path);
        // A flush every second, and a retention pass every second: both deadlines
        // pass several times during the outage below.
        let storage_config = RedbStorageConfig::new()
            .with_history(HistoryMode::All)
            .with_fsync(false)
            .with_flush_interval_secs(1)
            .with_retention(RetentionPolicy {
                max_age_secs: Some(3600),
                interval_secs: 1,
                ..Default::default()
            });
        let storage =
            Arc::new(RedbStorage::open(&db_path, storage_config.clone(), "down").unwrap());
        let mut plugin =
            RedbStoragePlugin::new(test_storage_config("down"), storage.clone(), storage_config);

        disk_full.store(true, Ordering::Release);
        let put = plugin
            .put(
                Some(OwnedKeyExpr::new("test/k").unwrap()),
                ZBytes::from("v"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand()),
            )
            .await;
        // With `fsync: false` the write that fails may be this one or the flush
        // after it; either way the storage ends up out of service.
        let _ = put;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !storage.needs_recovery() {
            assert!(
                Instant::now() < deadline,
                "the storage never noticed the failure"
            );
            let _ = storage.flush();
            std::thread::sleep(Duration::from_millis(20));
        }

        // Stay down for three seconds: three flush deadlines, three retention
        // deadlines, and a couple of failed reopen attempts.
        let before = plugin.shared.wakeups.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(3));
        let wakeups = plugin.shared.wakeups.load(Ordering::Relaxed) - before;
        assert!(storage.needs_recovery(), "the disk is still full");
        assert!(
            wakeups < 50,
            "the background thread woke {wakeups} times in 3s of outage; a handful is \
             right, thousands is a busy loop"
        );

        // And it still recovers afterwards.
        disk_full.store(false, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(40);
        while storage.needs_recovery() {
            assert!(Instant::now() < deadline, "the storage never came back");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// #36: on a storage too big to read inside an admin query, the query must
    /// not read it. It reports what is free to know, asks the background thread
    /// for the rest, and the next query has it.
    #[tokio::test]
    async fn test_admin_status_of_a_large_storage_serves_a_snapshot() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("big.redb");
        let storage_config = RedbStorageConfig::new();
        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "big".to_string()).unwrap();

        let mut plugin = RedbStoragePlugin::new(
            test_storage_config("big"),
            Arc::new(redb_storage),
            storage_config,
        );
        // Every file is "large": nothing may be computed inline.
        plugin.inline_usage_max_bytes = 0;

        let id = zenoh::time::TimestampId::rand();
        for i in 0..5 {
            plugin
                .put(
                    Some(OwnedKeyExpr::new(format!("test/k{i}")).unwrap()),
                    ZBytes::from(vec![0u8; 1024]),
                    Encoding::ZENOH_BYTES,
                    Timestamp::new(NTP64(100 + i), id),
                )
                .await
                .unwrap();
        }

        // First query: the free numbers are there, the expensive ones are not —
        // and are `null`, not a zero that was never measured.
        let first: serde_json::Value = serde_json::to_value(plugin.get_admin_status()).unwrap();
        assert_eq!(first["stats"]["key_count"], 5);
        assert!(first["stats"]["on_disk_bytes"].as_u64().unwrap() > 0);
        assert!(first["stats"]["stored_bytes"].is_null(), "{first}");
        assert!(first["stats"]["live_keys"].is_null());
        assert!(first["stats"]["usage_age_secs"].is_null());

        // The background thread computes it; a later query serves it.
        let deadline = Instant::now() + Duration::from_secs(10);
        let stats = loop {
            let status: serde_json::Value =
                serde_json::to_value(plugin.get_admin_status()).unwrap();
            if !status["stats"]["usage_age_secs"].is_null() {
                break status["stats"].clone();
            }
            assert!(Instant::now() < deadline, "no usage snapshot after 10s");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(stats["live_keys"], 5);
        assert_eq!(stats["tombstones"], 0);
        assert!(stats["stored_bytes"].as_u64().unwrap() >= 5 * 1024);
        assert!(stats["usage_age_secs"].as_u64().unwrap() < 10);
    }

    /// Intervals too long for an `Instant` to hold mean "never". They used to
    /// overflow and panic the background thread at startup, which ended retention,
    /// flushing and usage snapshots for that storage without a word.
    #[tokio::test]
    async fn test_an_absurd_interval_does_not_kill_the_background_thread() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("forever.redb");
        let storage_config = RedbStorageConfig::new()
            .with_history(HistoryMode::All)
            .with_fsync(false)
            .with_flush_interval_secs(u64::MAX)
            .with_retention(RetentionPolicy {
                max_age_secs: Some(60),
                interval_secs: u64::MAX,
                ..Default::default()
            });
        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "forever".to_string()).unwrap();

        let mut plugin = RedbStoragePlugin::new(
            test_storage_config("forever"),
            Arc::new(redb_storage),
            storage_config,
        );
        plugin.inline_usage_max_bytes = 0;

        // The thread is alive if it still answers: a usage snapshot only ever
        // comes from it.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status: serde_json::Value =
                serde_json::to_value(plugin.get_admin_status()).unwrap();
            if !status["stats"]["usage_age_secs"].is_null() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the background thread did not answer; it probably panicked"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// #35: an `fsync: false` storage must reach the disk when writes stop, not
    /// only when the next write happens to come along.
    #[tokio::test]
    async fn test_an_idle_fsync_false_storage_is_flushed_in_the_background() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("lazy.redb");
        // The storage itself is given an interval that never passes, so no commit
        // is ever promoted to durable on the write path. Only the background
        // thread, which takes its timer from the config handed to the plugin, can
        // be what flushes it.
        let storage_config = RedbStorageConfig::new()
            .with_fsync(false)
            .with_flush_interval_secs(3600);
        let storage = Arc::new(
            RedbStorage::open(&db_path, storage_config.clone(), "lazy".to_string()).unwrap(),
        );

        let mut plugin = RedbStoragePlugin::new(
            test_storage_config("lazy"),
            storage.clone(),
            storage_config.with_flush_interval_secs(1),
        );
        plugin
            .put(
                Some(OwnedKeyExpr::new("test/k").unwrap()),
                ZBytes::from("v"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand()),
            )
            .await
            .unwrap();
        assert!(
            storage.has_unflushed_writes(),
            "an fsync: false commit inside the flush interval is not durable yet"
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        while storage.has_unflushed_writes() {
            assert!(
                Instant::now() < deadline,
                "nothing flushed an idle storage within 10s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `flush_interval_secs` must be a whole number of seconds.
    #[tokio::test]
    async fn test_flush_interval_must_be_a_number() {
        let dir = TempDir::new().unwrap();
        let volume = test_volume(&dir, HistoryMode::Latest);

        for bad in [json!("5"), json!(-1), json!(1.5)] {
            let Err(err) = volume
                .create_storage(storage_config_for(
                    json!({ "dir": "bad", "fsync": false, "flush_interval_secs": bad }),
                    &dir,
                ))
                .await
            else {
                panic!("flush_interval_secs: {bad} must be rejected");
            };
            assert!(err.to_string().contains("flush_interval_secs"), "{err}");
        }

        volume
            .create_storage(storage_config_for(
                json!({ "dir": "good", "fsync": false, "flush_interval_secs": 30 }),
                &dir,
            ))
            .await
            .map(|_| ())
            .expect("a whole number of seconds is accepted");
    }

    /// A tombstone must be visible in the counts, distinctly from a live key.
    #[tokio::test]
    async fn test_admin_status_counts_are_not_estimates() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new().with_create_db(true);

        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin::new(
            test_storage_config("test"),
            Arc::new(redb_storage),
            storage_config,
        );

        let id = zenoh::time::TimestampId::rand();
        let key = OwnedKeyExpr::new("test/gone").unwrap();
        storage_plugin
            .put(
                Some(key.clone()),
                ZBytes::from("x"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(100), id),
            )
            .await
            .unwrap();
        storage_plugin
            .delete(Some(key), Timestamp::new(NTP64(200), id))
            .await
            .unwrap();

        let status: serde_json::Value =
            serde_json::to_value(storage_plugin.get_admin_status()).unwrap();
        assert_eq!(status["stats"]["live_keys"], 0);
    }

    /// An out-of-order PUT must not clobber a newer stored value.
    ///
    /// The storage manager filters most of these upstream, but only against an
    /// in-memory cache seeded at startup — replay, alignment and a restarted manager
    /// all reach the backend directly.
    #[tokio::test]
    async fn test_storage_put_outdated_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new().with_create_db(true);

        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin::new(
            test_storage_config("test"),
            Arc::new(redb_storage),
            storage_config,
        );

        let key = OwnedKeyExpr::new("test/ordered").unwrap();
        let id = zenoh::time::TimestampId::rand();

        // First write of this key: Inserted.
        let newer = storage_plugin
            .put(
                Some(key.clone()),
                ZBytes::from("newer"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(200), id),
            )
            .await
            .unwrap();
        assert!(matches!(newer, StorageInsertionResult::Inserted));

        // An older sample arrives late: rejected, and the payload is untouched.
        let older = storage_plugin
            .put(
                Some(key.clone()),
                ZBytes::from("older"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(100), id),
            )
            .await
            .unwrap();
        assert!(matches!(older, StorageInsertionResult::Outdated));

        let data = storage_plugin.get(Some(key.clone()), "").await.unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(
            data[0].payload.to_bytes().as_ref(),
            b"newer",
            "the older PUT must not have overwritten the newer value"
        );

        // A genuinely newer sample replaces it.
        let newest = storage_plugin
            .put(
                Some(key.clone()),
                ZBytes::from("newest"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(300), id),
            )
            .await
            .unwrap();
        assert!(matches!(newest, StorageInsertionResult::Replaced));

        let data = storage_plugin.get(Some(key), "").await.unwrap();
        assert_eq!(data[0].payload.to_bytes().as_ref(), b"newest");
    }

    /// A DELETE that predates the stored value must not remove it.
    #[tokio::test]
    async fn test_storage_delete_outdated_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new().with_create_db(true);

        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin::new(
            test_storage_config("test"),
            Arc::new(redb_storage),
            storage_config,
        );

        let key = OwnedKeyExpr::new("test/tombstone").unwrap();
        let id = zenoh::time::TimestampId::rand();

        storage_plugin
            .put(
                Some(key.clone()),
                ZBytes::from("live"),
                Encoding::ZENOH_STRING,
                Timestamp::new(NTP64(200), id),
            )
            .await
            .unwrap();

        let stale = storage_plugin
            .delete(Some(key.clone()), Timestamp::new(NTP64(100), id))
            .await
            .unwrap();
        assert!(matches!(stale, StorageInsertionResult::Outdated));
        assert_eq!(
            storage_plugin
                .get(Some(key.clone()), "")
                .await
                .unwrap()
                .len(),
            1,
            "a stale DELETE must not remove a newer value"
        );

        let fresh = storage_plugin
            .delete(Some(key.clone()), Timestamp::new(NTP64(300), id))
            .await
            .unwrap();
        assert!(matches!(fresh, StorageInsertionResult::Deleted));
        assert!(storage_plugin.get(Some(key), "").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_storage_put_none_key() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new().with_create_db(true);

        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin::new(
            test_storage_config("test"),
            Arc::new(redb_storage),
            storage_config,
        );

        // Put with None key
        let payload = ZBytes::from("none_value");
        let encoding = Encoding::ZENOH_STRING;
        let timestamp = Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand());

        let result = storage_plugin
            .put(None, payload.clone(), encoding, timestamp)
            .await;
        assert!(result.is_ok());

        // Get with None key
        let result = storage_plugin.get(None, "").await;
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].payload.to_bytes(), payload.to_bytes());
    }

    #[tokio::test]
    async fn test_storage_delete() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new().with_create_db(true);

        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin::new(
            test_storage_config("test"),
            Arc::new(redb_storage),
            storage_config,
        );

        // Put data
        let key = OwnedKeyExpr::new("test/key1").unwrap();
        let payload = ZBytes::from("test_value");
        let encoding = Encoding::ZENOH_STRING;
        let timestamp = Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand());

        storage_plugin
            .put(Some(key.clone()), payload, encoding, timestamp)
            .await
            .unwrap();

        // Delete data
        let result = storage_plugin.delete(Some(key.clone()), timestamp).await;
        assert!(result.is_ok());

        // Verify it's gone
        let result = storage_plugin.get(Some(key), "").await;
        assert!(result.is_ok());
        let data = result.unwrap();
        assert_eq!(data.len(), 0);
    }

    #[tokio::test]
    async fn test_storage_read_only_rejects_put() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        // First create with write access
        let storage_config = RedbStorageConfig::new().with_create_db(true);

        let redb_storage = RedbStorage::open(&db_path, storage_config, "test".to_string()).unwrap();

        let key = OwnedKeyExpr::new("test/key1").unwrap();
        let payload = ZBytes::from("test_value");
        let encoding = Encoding::ZENOH_STRING;
        let timestamp = Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand());

        // Put some data first
        redb_storage
            .put(
                key.as_ref(),
                StoredValue::new(payload.to_bytes().to_vec(), timestamp, encoding.clone()),
            )
            .unwrap();

        drop(redb_storage);

        // Now open as read-only
        let ro_config = RedbStorageConfig::new()
            .with_create_db(false)
            .with_read_only(true);

        let ro_storage =
            RedbStorage::open(&db_path, ro_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin =
            RedbStoragePlugin::new(test_storage_config("test"), Arc::new(ro_storage), ro_config);

        // Try to put - should fail
        let new_payload = ZBytes::from("new_value");
        let result = storage_plugin
            .put(Some(key), new_payload, encoding, timestamp)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_storage_read_only_rejects_delete() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        // First create with write access
        let storage_config = RedbStorageConfig::new().with_create_db(true);

        let redb_storage = RedbStorage::open(&db_path, storage_config, "test".to_string()).unwrap();

        let key = OwnedKeyExpr::new("test/key1").unwrap();
        let payload = ZBytes::from("test_value");
        let encoding = Encoding::ZENOH_STRING;
        let timestamp = Timestamp::new(NTP64(100), zenoh::time::TimestampId::rand());

        // Put some data first
        redb_storage
            .put(
                key.as_ref(),
                StoredValue::new(payload.to_bytes().to_vec(), timestamp, encoding),
            )
            .unwrap();

        drop(redb_storage);

        // Now open as read-only
        let ro_config = RedbStorageConfig::new()
            .with_create_db(false)
            .with_read_only(true);

        let ro_storage =
            RedbStorage::open(&db_path, ro_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin =
            RedbStoragePlugin::new(test_storage_config("test"), Arc::new(ro_storage), ro_config);

        // Try to delete - should fail
        let result = storage_plugin.delete(Some(key), timestamp).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_storage_get_all_entries() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let storage_config = RedbStorageConfig::new().with_create_db(true);

        let redb_storage =
            RedbStorage::open(&db_path, storage_config.clone(), "test".to_string()).unwrap();

        let mut storage_plugin = RedbStoragePlugin::new(
            test_storage_config("test"),
            Arc::new(redb_storage),
            storage_config,
        );

        // Put multiple entries
        for i in 1..=3 {
            let key = OwnedKeyExpr::new(format!("test/key{}", i)).unwrap();
            let payload = ZBytes::from(format!("value{}", i));
            let encoding = Encoding::ZENOH_STRING;
            let timestamp = Timestamp::new(NTP64(100 + i), zenoh::time::TimestampId::rand());

            storage_plugin
                .put(Some(key), payload, encoding, timestamp)
                .await
                .unwrap();
        }

        // Get all entries
        let result = storage_plugin.get_all_entries().await;
        assert!(result.is_ok());

        let entries = result.unwrap();
        assert_eq!(entries.len(), 3);
    }
}
