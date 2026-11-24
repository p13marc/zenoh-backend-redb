//! Configuration structures for the zenoh-backend-redb storage backend.

use serde::{Deserialize, Serialize};

/// How much of a key's history a storage keeps.
///
/// This is a **volume**-level choice, not a per-storage one, because Zenoh asks the
/// *volume* for its capability (`Volume::get_capability`) and makes two decisions
/// from the answer that a storage cannot opt out of:
///
/// * A storage that declares `replication` fails to start unless its volume reports
///   `History::Latest` (`storages_mgt/mod.rs` in zenoh 1.10 `bail!`s on it).
/// * In `Latest` mode the storage manager drops outdated samples before they reach
///   the backend; in `All` mode it passes every sample straight through.
///
/// So a volume reporting `All` cannot host any replicated storage. Declare one
/// volume per mode rather than trying to mix them:
///
/// ```json5
/// volumes: {
///   redb: {},                                              // durable · latest
///   "redb-history": { backend: "redb", history: "all" },   // durable · all
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HistoryMode {
    /// One value per key. The last writer wins and older samples are discarded.
    #[default]
    Latest,
    /// Every sample is kept, addressed by `(key, timestamp)`, and a `_time`-ranged
    /// GET returns the window.
    All,
}

impl HistoryMode {
    /// Parse the `history` volume property.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "latest" => Some(Self::Latest),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// The string this mode is written as in a config, and reported as on the admin
    /// space.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::All => "all",
        }
    }
}

/// What a storage is allowed to keep.
///
/// **Zenoh storages have no TTL.** The storage manager's `garbage_collection`
/// prunes *metadata* — its own in-memory wildcard-update and latest-value caches —
/// and never touches stored values. So retention is the backend's job, and an
/// `all`-mode storage without a policy is an unbounded disk write.
///
/// Every limit is optional and all of them apply; a sample is dropped if *any* rule
/// says so. A policy with no limits at all is rejected, because it is almost
/// certainly a mistake rather than a deliberate "keep everything forever".
///
/// Unknown fields are rejected too. A misspelled limit (`max_byte`) that parsed as
/// "no such limit" would start a storage that reports a policy on the admin space
/// and has no bound.
///
/// ```json5
/// retention: {
///   max_age_secs: 2592000,       // 30 d
///   max_bytes: 10737418240,      // 10 GiB
///   max_samples_per_key: 100000,
///   decimate: { recent_secs: 86400, bucket_secs: 300 },
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RetentionPolicy {
    /// Drop samples older than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_secs: Option<u64>,

    /// Keep the database file under this size.
    ///
    /// The limit is on the real file. A pass that finds the file over it compacts,
    /// because redb does not shrink the file when rows are removed, and packs
    /// half-empty pages if that is what it takes. It evicts the oldest samples
    /// only if the file is still over the limit after that. A file that is over
    /// the limit merely because it is loosely packed loses no samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,

    /// Bound per-key growth independently of age and total size. A single
    /// pathological key should not be able to evict every other key's history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_samples_per_key: Option<u64>,

    /// Keep full resolution recently and thin out beyond it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimate: Option<DecimationPolicy>,

    /// Also forget a **key** once everything about it is older than `max_age_secs`.
    ///
    /// The other limits prune a key's history and leave its latest value alone,
    /// for ever: a plain GET still answers it and every wildcard query still lists
    /// it. For a stable set of keys that is what a storage is for. Where keys come
    /// and go (one per process, per flow, per container), it is growth that no
    /// limit bounds. With this set, a key whose latest sample is older than
    /// `max_age_secs` is removed entirely, tombstones included; a GET for it then
    /// returns nothing. Requires `max_age_secs`. Off by default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub expire_keys: bool,

    /// Seconds between retention passes. Enforcement is periodic, not per-write:
    /// paying for it on every PUT would put a scan on the hot path.
    #[serde(default = "default_retention_interval_secs")]
    pub interval_secs: u64,
}

/// Thin out older samples to one per bucket.
///
/// This is what makes a year of 5-second telemetry affordable, and it is a policy
/// only the backend can apply: the router's downsampling interceptor shapes
/// *traffic*, not storage, so it cannot thin data that is already written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecimationPolicy {
    /// Keep every sample newer than this.
    pub recent_secs: u64,
    /// Beyond `recent_secs`, keep one sample per bucket of this width.
    pub bucket_secs: u64,
}

impl RetentionPolicy {
    /// Does this policy actually bound anything?
    pub fn is_bounded(&self) -> bool {
        self.max_age_secs.is_some()
            || self.max_bytes.is_some()
            || self.max_samples_per_key.is_some()
            || self.decimate.is_some()
    }

    /// Check the policy for values that parse but cannot mean what they say.
    ///
    /// Called when a storage is created, so a bad policy is a startup error rather
    /// than something a pass quietly works around an hour later.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.is_bounded() {
            return Err(
                "sets no limit. Give it at least one of max_age_secs, max_bytes, \
                 max_samples_per_key or decimate — an empty policy is almost certainly a \
                 mistake, and it reads as protection that is not there."
                    .to_string(),
            );
        }
        if self.interval_secs == 0 {
            return Err(
                "`interval_secs` is 0. A pass cannot run continuously; give the number \
                 of seconds between passes (default 3600)."
                    .to_string(),
            );
        }
        if self.expire_keys && self.max_age_secs.is_none() {
            return Err(
                "`expire_keys` needs `max_age_secs`: that is the age past which a key \
                 with no newer sample is forgotten."
                    .to_string(),
            );
        }
        if let Some(decimate) = &self.decimate
            && decimate.bucket_secs == 0
        {
            return Err(
                "`decimate.bucket_secs` is 0. It is the width of the bucket one sample \
                 is kept from, so it must be at least 1."
                    .to_string(),
            );
        }
        Ok(())
    }
}

fn default_retention_interval_secs() -> u64 {
    3600
}

/// Configuration for a single redb storage instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedbStorageConfig {
    /// Page-cache budget in bytes for redb.
    ///
    /// Defaults to [`DEFAULT_CACHE_SIZE`] (64 MiB) and is **always** set explicitly —
    /// this backend never inherits redb's own default, which is 1 GiB. On a small
    /// guest an unbounded cache reads as a slow multi-day RSS climb ending at the OOM
    /// killer, with nothing in the logs to connect it to a storage setting.
    #[serde(default = "default_cache_size")]
    pub cache_size: usize,

    /// Whether every commit reaches the disk before it returns.
    /// Default is true for data safety.
    ///
    /// `false` trades that for write throughput: commits are made durable every
    /// [`flush_interval_secs`](Self::flush_interval_secs) instead, so a crash
    /// loses at most that many seconds of writes.
    #[serde(default = "default_true")]
    pub fsync: bool,

    /// With `fsync: false`, the longest a commit stays unpersisted, in seconds.
    /// Ignored with `fsync: true`, where every commit is durable.
    ///
    /// This exists because redb has no "eventually durable" level: a non-durable
    /// commit is not persisted *at all* until a durable one follows, and redb
    /// holds memory for each one until then. Without a bound, an `fsync: false`
    /// storage lost everything since startup to a crash and grew without limit
    /// while it ran. `0` makes every commit durable, the same as `fsync: true`.
    #[serde(default = "default_flush_interval_secs")]
    pub flush_interval_secs: u64,

    /// The most samples one `_time` GET may return for one key; `0` for no limit.
    ///
    /// Zenoh's `Storage::get` returns a `Vec`, so a window is read into memory —
    /// payloads and all — before the first reply goes out. Without a bound, one
    /// query for a year of a busy key can take the router down. A window holding
    /// more than this returns the **newest** that many.
    #[serde(default = "default_max_samples_per_get")]
    pub max_samples_per_get: u64,

    /// Whether to create the database if it doesn't exist.
    #[serde(default = "default_true")]
    pub create_db: bool,

    /// Read-only mode. If true, the storage will not accept writes.
    #[serde(default)]
    pub read_only: bool,

    /// How much history to keep. Inherited from the volume this storage belongs to;
    /// see [`HistoryMode`] for why it is a volume-level choice.
    #[serde(default)]
    pub history: HistoryMode,

    /// What this storage is allowed to keep. Required for [`HistoryMode::All`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RetentionPolicy>,
}

impl RedbStorageConfig {
    /// Create a new storage configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the cache size in bytes.
    pub fn with_cache_size(mut self, cache_size: usize) -> Self {
        self.cache_size = cache_size;
        self
    }

    /// Set what this storage is allowed to keep.
    pub fn with_retention(mut self, retention: RetentionPolicy) -> Self {
        self.retention = Some(retention);
        self
    }

    /// Set how much history this storage keeps.
    pub fn with_history(mut self, history: HistoryMode) -> Self {
        self.history = history;
        self
    }

    /// Set whether to enable fsync.
    pub fn with_fsync(mut self, fsync: bool) -> Self {
        self.fsync = fsync;
        self
    }

    /// Set how long a commit may stay unpersisted with `fsync: false`.
    pub fn with_flush_interval_secs(mut self, secs: u64) -> Self {
        self.flush_interval_secs = secs;
        self
    }

    /// Set the most samples one `_time` GET may return for one key (`0`: no limit).
    pub fn with_max_samples_per_get(mut self, max: u64) -> Self {
        self.max_samples_per_get = max;
        self
    }

    /// Set whether to create the database if it doesn't exist.
    pub fn with_create_db(mut self, create_db: bool) -> Self {
        self.create_db = create_db;
        self
    }

    /// Set read-only mode.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }
}

/// Default redb page-cache budget: 64 MiB.
///
/// Deliberately not redb's default (1 GiB). See [`RedbStorageConfig::cache_size`].
pub const DEFAULT_CACHE_SIZE: usize = 64 * 1024 * 1024;

/// `Default` is written out rather than derived so that it cannot drift from the
/// serde defaults above. A derived `Default` gave `fsync: false` and
/// `create_db: false`, neither of which matches what parsing the same config from
/// JSON produces.
impl Default for RedbStorageConfig {
    fn default() -> Self {
        Self {
            cache_size: default_cache_size(),
            fsync: default_true(),
            flush_interval_secs: default_flush_interval_secs(),
            max_samples_per_get: default_max_samples_per_get(),
            create_db: true,
            read_only: false,
            history: HistoryMode::Latest,
            retention: None,
        }
    }
}

// Default value functions for serde
fn default_cache_size() -> usize {
    DEFAULT_CACHE_SIZE
}

/// Default cap on the samples one `_time` GET returns for one key.
pub const DEFAULT_MAX_SAMPLES_PER_GET: u64 = 100_000;

fn default_max_samples_per_get() -> u64 {
    DEFAULT_MAX_SAMPLES_PER_GET
}

/// Default bound on unpersisted writes with `fsync: false`: 5 seconds.
pub const DEFAULT_FLUSH_INTERVAL_SECS: u64 = 5;

fn default_flush_interval_secs() -> u64 {
    DEFAULT_FLUSH_INTERVAL_SECS
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache budget must never fall back to redb's own default (1 GiB).
    /// An unbounded page cache on a small guest is a slow RSS climb that ends at the
    /// OOM killer, and nothing in the logs points back at a storage setting.
    #[test]
    fn cache_size_defaults_to_64_mib_not_redbs_default() {
        assert_eq!(DEFAULT_CACHE_SIZE, 64 * 1024 * 1024);
        assert_eq!(RedbStorageConfig::default().cache_size, DEFAULT_CACHE_SIZE);

        // ...and a config parsed from JSON that omits the field agrees.
        let parsed: RedbStorageConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.cache_size, DEFAULT_CACHE_SIZE);
    }

    /// `Default` is hand-written precisely so it cannot drift from the serde
    /// defaults; a derived one gave `fsync: false` and `create_db: false`.
    #[test]
    fn derived_and_parsed_defaults_agree() {
        let parsed: RedbStorageConfig = serde_json::from_str("{}").unwrap();
        let default = RedbStorageConfig::default();

        assert_eq!(parsed.fsync, default.fsync);
        assert_eq!(parsed.create_db, default.create_db);
        assert_eq!(parsed.read_only, default.read_only);
        assert_eq!(parsed.flush_interval_secs, default.flush_interval_secs);
        assert_eq!(parsed.max_samples_per_get, default.max_samples_per_get);
        assert_eq!(default.max_samples_per_get, DEFAULT_MAX_SAMPLES_PER_GET);
        assert_eq!(default.flush_interval_secs, DEFAULT_FLUSH_INTERVAL_SECS);
        assert!(default.fsync, "fsync must default to on");
        assert!(default.create_db, "create_db must default to on");
    }

    /// A misspelled limit must fail to parse, not parse as "no such limit".
    #[test]
    fn a_misspelled_retention_limit_is_rejected() {
        let err = serde_json::from_str::<RetentionPolicy>(
            r#"{ "max_age_secs": 86400, "max_byte": 10737418240 }"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("max_byte"), "{err}");

        let err = serde_json::from_str::<RetentionPolicy>(
            r#"{ "decimate": { "recent_secs": 60, "bucket_sec": 5 } }"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("bucket_sec"), "{err}");

        // The documented shape still parses.
        let policy: RetentionPolicy = serde_json::from_str(
            r#"{ "max_age_secs": 2592000, "max_bytes": 10737418240,
                 "max_samples_per_key": 100000,
                 "decimate": { "recent_secs": 86400, "bucket_secs": 300 },
                 "interval_secs": 3600 }"#,
        )
        .unwrap();
        assert!(policy.validate().is_ok());
    }

    /// Values that parse but cannot mean what they say are startup errors.
    #[test]
    fn a_policy_with_impossible_values_does_not_validate() {
        let empty = RetentionPolicy::default();
        assert!(empty.validate().unwrap_err().contains("no limit"));

        let no_interval = RetentionPolicy {
            max_age_secs: Some(60),
            interval_secs: 0,
            ..Default::default()
        };
        assert!(
            no_interval
                .validate()
                .unwrap_err()
                .contains("interval_secs")
        );

        let no_age = RetentionPolicy {
            max_bytes: Some(1 << 30),
            expire_keys: true,
            interval_secs: 60,
            ..Default::default()
        };
        assert!(no_age.validate().unwrap_err().contains("expire_keys"));
        let with_age = RetentionPolicy {
            max_age_secs: Some(60),
            expire_keys: true,
            interval_secs: 60,
            ..Default::default()
        };
        assert!(with_age.validate().is_ok());

        let no_bucket = RetentionPolicy {
            decimate: Some(DecimationPolicy {
                recent_secs: 60,
                bucket_secs: 0,
            }),
            interval_secs: 60,
            ..Default::default()
        };
        assert!(no_bucket.validate().unwrap_err().contains("bucket_secs"));
    }

    #[test]
    fn test_storage_config_builder() {
        let config = RedbStorageConfig::new()
            .with_cache_size(1024 * 1024)
            .with_fsync(false);

        assert_eq!(config.cache_size, 1024 * 1024);
        assert!(!config.fsync);
    }

    #[test]
    fn test_serde_roundtrip() {
        let config = RedbStorageConfig::new()
            .with_cache_size(1024)
            .with_fsync(false)
            .with_flush_interval_secs(30);

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: RedbStorageConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.cache_size, deserialized.cache_size);
        assert_eq!(config.fsync, deserialized.fsync);
        assert_eq!(config.flush_interval_secs, deserialized.flush_interval_secs);
    }
}
