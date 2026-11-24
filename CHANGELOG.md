# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **redb 4.2 → 5.0** (#45). Databases written by 0.5 open as they are. redb changed
  its file locking: on Linux, do not run a 0.5 router and a 0.6 router against the
  same storage directory at the same time. Besides the new compaction mode used
  below, this picks up redb's fix for unbounded page-cache growth under write
  workloads (4.4), a third candidate for the memory growth in #27.
- A database is closed explicitly when its storage is dropped, and a failure to close
  is logged instead of ignored.
- Opening a database that was not closed cleanly logs the progress of the repair. On
  a large file that takes minutes, during which the router used to look hung.

- **Breaking (API): one way to open a storage, one copy of its path** (#52).
  `RedbStorage::open(path, config, name)` replaces `RedbStorage::new`, and the
  storage keeps the path it opened. `RedbStorageConfig::db_path` is gone: it was a
  second, optional copy that `max_bytes` and the statistics read, so a storage opened
  without it reported no size and enforced no size limit.
- **Breaking (API): removed** `RedbBackend`, `RedbBackendConfig`, and the config
  fields nothing read (`db_file`, `key_expr`, `strip_prefix`, `table_name`) with their
  builders. The plugin never used the backend layer; `RedbStorage` is the library
  entry point. Unused error variants are gone too.
- New API: `RedbStorage::try_recover`, `needs_recovery`, `health`;
  `RedbBackendError::Unavailable`, `ReadOnly` and `is_io_failure`.
- Every storage's background thread now wakes once a second (one atomic load) to see
  whether its storage has failed.
- The crate root re-exports `HistoryMode`, `RetentionPolicy`, `DecimationPolicy`,
  `RetentionPass`, `StorageStats`, `StorageUsage` and `WriteOutcome`.

- **On-disk format 2, with a version stamp** (#49). A database now says which
  layout it has. A 0.4 / 0.5 database (format 1) is **migrated in place the first time
  0.6 opens it for writing**, in one transaction. The migration is one-way, and a
  rollback fails silently rather than loudly: 0.5 has no format check, so it opens a
  migrated file and simply no longer finds the keyless sample. Copy the database
  first if you may need to roll back. From 0.6 on, a database written by a later
  version is refused instead of guessed at.

### Fixed
- **`max_bytes` evicted samples before reclaiming the free space inside pages**
  (#46). A B-tree page that has just split is half empty, and that space was counted
  as data. A pass now compacts, then packs the pages with redb 5's
  `CompactionMode::Maximum` if what is stored fits the limit, and evicts only if the
  file is still over. It also converges when values are large enough that pages can
  never be full.
- A compaction that is refused (a transaction was alive) no longer counts as "the
  file is as small as it gets".
- **`read_only: true` opened the database read-write and committed to it** (#47).
  It now opens through redb's read-only handle: the file is not modified, a file the
  router cannot write to opens, and any number of read-only storages can share one
  database. A read-only storage refuses a file that lacks this backend's tables or
  needs a repair, instead of creating or repairing it.
- **One I/O error left a storage dead until the router was restarted** (#48). After
  a failed write (a full disk), redb refuses every later operation on that database
  until it is reopened, and the backend never reopened it. A storage now takes itself
  out of service, fails fast while it is, and reopens its database from its
  background thread (1 s, 2 s, 4 s … up to every 30 s) until that works. New `health`
  object on the admin space: `state`, `last_error`, `failed_at`, `reopens`.
- **The "no key" sentinel collided with a real key** (#49). A sample whose key is
  exactly the storage's `strip_prefix` was stored under `@@none_key@@`, which is a
  valid key expression: `<prefix>/@@none_key@@` landed on the same row and was
  answered as `<prefix>`. It is now stored under the empty string.
- **`dir` and `db_file` could point outside the backend root** (#50). An absolute
  `db_file` replaced the root and a `..` walked out of it. Both are now rejected at
  storage creation. **Breaking** for a config that used an absolute `db_file` on
  purpose: move the root with `ZENOH_BACKEND_REDB_ROOT` instead. A name with
  directories in it (`dir: "fleet/telemetry"`) works, and the directories are
  created. A `dir` that is not a string is now an error even when a
  `db_file` is also given; it used to be skipped in favour of the `db_file`.

## [0.5.0] - 2026-11-23

### Fixed
- **`max_bytes` deleted samples from storages that fitted their budget** (#33). It
  compared the limit against the file length and evicted before compacting, but a
  redb file is routinely two to three times its live data. A pass now measures the
  live data, evicts only if *that* is over the limit, and compacts to shrink the
  file. If compaction cannot run, nothing is deleted on the strength of the file
  length. In the reproduction (12 MB of live data, `max_bytes` 20 MB) the old code
  kept 9 313 of 24 000 in-window samples; this keeps all of them.
- **A retention pass no longer reads the history into memory** (#34). The age pass
  built the full list of doomed keys before deleting any, and the size pass read and
  sorted every history key, once per round. A pass now holds one 4096-row batch,
  reads in short transactions, and with `max_age_secs` touches only the rows it
  deletes plus one per key. 100 000 samples: about 1.8 MB of heap, down from over
  30 MB.
- **The retention log line and admin report could not show that a pass worked**
  (#27). They reported the file size before and after, which is identical unless the
  pass compacts. They now report the sample count before and after, how many samples
  were evicted for size, and whether the pass compacted.
- **A large `max_age_secs` deleted the whole history** (#38). A window longer than
  the time since 1970 underflowed, and in a release build wrapped to a cutoff in the
  far future. Past 2^32 seconds it panicked the retention thread instead, ending
  retention for that storage without a word. Cutoffs now saturate, and a pass that
  panics is logged and does not stop the next one.

- **`fsync: false` was never made durable, and leaked memory on every write**
  (#35). It mapped every commit to `Durability::None` and nothing ever issued a
  durable one, so a kill lost everything written since the storage was opened, and
  redb's record of each non-durable commit (about 40 bytes) accumulated until
  restart. A commit is now promoted to durable every `flush_interval_secs` (new,
  default 5), and a background thread flushes the tail when writes stop. A crash
  loses at most that interval; memory is flat.
- **Every admin-space query read the entire database** (#36). `get_admin_status`
  walked every page of all four tables and scanned the metadata table, synchronously,
  per query. It now reports the free numbers (file size, key and sample counts, cache)
  directly, and the rest from a snapshot computed on a background thread once the
  file is over 64 MiB. `usage_age_secs` says how old that snapshot is.

- **A `_time` GET scanned the key's whole history** (#37) and filtered by time in
  memory, although the key layout exists to make a window one bounded range scan.
  It now reads only the rows of the window.
- **A dot in `dir` was truncated** (#39). `dir: "telemetry.v1"` and
  `dir: "telemetry.v2"` both became `telemetry.redb`, and the second storage failed
  to open a file the first had locked. `.redb` is now appended.
  **If you already run a storage with a dot in `dir`**, its data is in the truncated
  file and this version will start an empty one beside it, with a warning that names
  both. Stop the router and rename the old file, or point the storage at it with
  `db_file`. (It is not adopted automatically: `telemetry.redb` is also exactly what
  `dir: "telemetry"` names, so guessing could hand one storage another's database.)

### Removed
- `bin/act` (a 20 MB GitHub-Actions runner binary) and `just ci-act`. `ci-local.sh`
  now runs what the Forgejo CI runs, minus the conformance job.

### Deprecated
- `RedbStorageConfig::with_key_expr`, `with_strip_prefix` and `with_table_name`. The
  fields behind them were never read: the storage manager filters keys and strips
  the prefix before it calls a backend, and the tables have fixed names.

### Changed
- **Breaking (API):** `RedbStorage::stats()` is O(1) and `StorageStats` loses
  `stored_bytes`, `metadata_bytes`, `fragmented_bytes`, `live_keys`, `tombstones`,
  `oldest_timestamp` and `newest_timestamp`. They moved to `StorageUsage`, returned by
  the new `RedbStorage::usage()`, which reads the whole database. The admin-space JSON
  keeps the same keys, plus `usage_age_secs`; on a storage over 64 MiB they are `null`
  until the first snapshot.
- New: `RedbStorageConfig::flush_interval_secs` / `with_flush_interval_secs`,
  `RedbStorage::flush`, `RedbStorage::has_unflushed_writes`, and the
  `flush_interval_secs` storage property.
- The table-creation commit when a storage is opened is always durable.
- Every storage now has one background thread, not only `all`-mode ones.
- **Breaking (config):** an unknown field inside `retention` or `retention.decimate`
  is now a startup error (#38). `max_byte: ...` used to parse as "no size limit".
  `interval_secs: 0` and `decimate.bucket_secs: 0` are rejected too.
- **Breaking (API):** `RetentionPass` gains `samples_before`, `samples_after`,
  `samples_evicted_for_size` and `compacted`. `RetentionPolicy::validate` is new.
- `max_bytes` eviction now stops at about 90 % of the limit rather than at it, so
  ordinary writes do not put the data straight back over.
- Updated to Zenoh 1.10.2 (pinned `=1.10.2`, now on all five zenoh crates: `zenoh`
  itself was `"1.10"`). The plugin must be rebuilt against a 1.10.2 `zenohd`; the
  toolchain stays at 1.97.
- Lockfile refresh for advisories in transitive dependencies: `quinn-proto`,
  `quinn-udp`, `rustls`, `rustls-webpki`, `webpki-root-certs`, `tokio`, `url`.
- Renovate no longer proposes toolchain bumps. The plugin must be built with the
  rustc that built `zenohd`, so the toolchain moves only with a zenoh upgrade.

## [0.4.0] - 2026-08-29

### Changed
- **Breaking**: `RedbStorage::delete` takes the deletion's `Timestamp`. A deletion
  always has one, and `all`-mode storages need it to place the tombstone.
- **Breaking**: `RedbStorage::put` returns `WriteOutcome` rather than `()`, so the
  plugin layer can report `Inserted`/`Replaced`/`Outdated` without a second read.
  The last-writer-wins decision now happens inside the write transaction, where two
  concurrent writers cannot both conclude they are newer.
- **Breaking**: Updated to Zenoh 1.10.0 (pinned `=1.10.0`). A 1.7 plugin cannot load
  into the 1.9/1.10 routers this backend targets; the failure is silent — zenohd
  starts, logs one ERROR line, and serves no storage.
- **Breaking**: Updated redb 2.6 → 4.2. redb 3 dropped support for the file format
  this crate wrote before 0.4, so **existing `.redb` files will not open**. Delete
  them, or open them once with redb 2.6 and call `Database::upgrade()` first. The
  error now says so explicitly instead of surfacing as apparent corruption.
- **Breaking**: `cache_size` is now a plain `usize` with a default of **64 MiB**
  rather than `Option<usize>` meaning "redb's default". redb 4 defaults to 1 GiB,
  which on a small guest reads as a slow RSS climb ending at the OOM killer.
- `thiserror` 1 → 2; dev-dependencies `criterion` 0.5 → 0.8, `rand` 0.8 → 0.10.
- `RedbStorageConfig`'s `Default` is hand-written so it can no longer disagree with
  the serde defaults (it previously gave `fsync: false` and `create_db: false`).

### Added
- **A router-storage conformance suite**, run against a real `zenohd` with a redb
  volume under it (`tests/conformance_router_storage.rs`, `just conformance`, and a
  `conformance` CI job that gates merges). It asserts the standard storage-backend
  contract — a state doc outliving its publisher, a DELETE retiring it, `*` not
  reaching a `@catalog` chunk, blob chunks surviving their sensor, a fleet `@rpc`
  GET still fanning in, every event record surviving, and no config claiming
  `complete` — plus the two cases a latest-value backend cannot pass at all: a
  `_time`-ranged GET and a retention pass.
- **Retention.** A per-storage `retention` policy with `max_age_secs`, `max_bytes`,
  `max_samples_per_key` and optional `decimate` (full resolution for `recent_secs`,
  then one sample per `bucket_secs`), enforced by a background task on an interval
  rather than on every write.
  - An `all`-mode storage with **no** retention policy now **refuses to start**; a
    policy that sets no limit is rejected; and a policy on a `latest`-mode volume is
    rejected too, since there is no history there to prune and it would report
    passes while reclaiming nothing.
  - `max_bytes` is measured against the real file and enforcing it **compacts** —
    redb does not return space to the filesystem on delete, so without that the
    policy could never converge and every pass would delete more data while
    reporting no improvement.
  - Each pass is reported on the admin space, so retention is verifiable from
    outside the process.
  - The pass runs on a plain OS thread, not a tokio task: `Volume::create_storage`
    is not called from inside a tokio runtime in `zenohd`, so spawning a task there
    panicked and took down every `all`-mode storage at router startup.
- **`History::All`.** A volume configured `history: "all"` keeps every sample rather
  than one value per key, addressed by `(key, timestamp)`, and answers Zenoh's
  `_time` selector parameter with the window — both documented syntaxes, including
  relative `now(-1h)` expressions. Without `_time` the same key returns its latest
  sample. This makes the crate a pure-Rust, embedded, `durable · all` Zenoh backend.
  - History is a **volume**-level property because Zenoh asks the volume for its
    capability and makes two decisions from it a storage cannot override: a storage
    declaring `replication` refuses to start unless the volume reports
    `History::Latest`, and in `latest` mode the manager discards outdated samples
    before they reach the backend. Declare one volume per mode; one plugin serves
    both.
  - Deletions are recorded as tombstones in `all` mode and are never replied to.
    A tombstoned key stays enumerable in `all` mode, so its retained history is
    still reachable through a wildcard `_time` selector; in `latest` mode, where
    there is nothing left to reach, it is not.
  - Admin statistics account for the history tables and report `sample_count`
    alongside `key_count`.
- **Storages report their cost on the admin space.** `get_admin_status` now carries
  on-disk bytes (the real file), stored/metadata/fragmented bytes, key count, live
  keys vs tombstones, the timestamp span held, cache size/usage/hit-ratio/evictions,
  and the declared capability. An operator could previously see that a storage
  existed but not what it was consuming.
- `RedbStorage::stats` and the `StorageStats` type.
- `RedbStorage::get_all_timestamps` — every live key and timestamp without reading
  a payload. This is what the storage manager calls to resolve **every** wildcard
  query, and it previously went through `get_all`, loading the entire database into
  memory to discard all of it. On a 100k-key store that was ~30 ms and 100k payload
  copies per wildcard GET.
- Wildcard and prefix scans are now bounded range scans over the ordered table
  rather than full scans. On a 100k-key store, a prefix-heavy selector
  (`v1/h-00000/telemetry/**`) went from ~30 ms to ~249 µs. A selector that
  wildcards early (`v1/*/telemetry/x`) still scans the table, by construction.
- `cache_size` is actually passed to redb. It was parsed and stored but never
  reached the database, so every storage silently ran on redb's default cache.
- `fsync` is actually passed to redb, as `Durability::Immediate` / `Durability::None`.
  It was likewise parsed and ignored.
- `create_db: false` and `read_only: true` now open an existing database instead of
  creating one. Because that is a real behaviour change, the failure explains
  itself: a missing file reports which setting stopped it being created, rather than
  a bare "No such file or directory". The shipped example config's read-only storage
  is commented out for the same reason — it pointed at a database nothing creates,
  which the old code papered over by ignoring both flags.
- `RedbStorage::timestamp_of` — reads a key's timestamp without loading its payload.
- README: a **Version compatibility** section documenting the exact rustc/Zenoh match
  requirement and the three traps that all produce the same silent failure.

### Fixed
- **Wildcard matching now uses Zenoh's key-expression algebra** (`keyexpr::intersects`)
  instead of a hand-rolled `*`/`**` matcher. The hand-rolled one lacked two rules:
  `*`/`**` must never match a chunk beginning with `@` (the basis of the `@rpc`,
  `@media`, `@blob` and `@catalog` verbatim planes), and `$*` sub-chunk matching was
  treated as a literal, so a selector using it silently matched nothing.
- **PUT no longer overwrites newer data with older data.** `put` never compared
  timestamps and always reported `Inserted`, so a replayed or out-of-order sample
  silently won. It now returns `Outdated` / `Replaced` / `Inserted` correctly.
- **DELETE no longer discards its timestamp.** A deletion that predates the stored
  value is rejected as `Outdated` instead of removing it.
- Reads no longer serialise behind a write mutex. `RedbStoragePlugin` held the
  storage in a `tokio::sync::Mutex` although every `RedbStorage` method takes
  `&self` and redb does its own concurrency control. The mutex now guards only the
  read-then-write in `put`/`delete`, which is the part that genuinely must be
  atomic — and that is what lets the synchronous `get_admin_status` report live
  statistics instead of giving up under contention.
- `Dockerfile`: the plugin is now a real member of the zenoh workspace with
  `[patch.crates-io]` pointing at the local sources. It was copied into the tree but
  carried its own lockfile, so it built as a *separate* workspace — the exact
  configuration that produces "Incompatible Zenoh feature sets".
- `docker-compose.yml` pinned `ZENOH_VERSION 1.6.2` against a 1.7.0 plugin.
- `justfile`: `docker-test-zenohd-fast` had an empty body; `msrv` checked Rust 1.70,
  impossible under `edition = "2024"`.
- `release.yml` published the *test* image stage, because `buildah build` defaults to
  the last stage and no `--target` was given.

## [0.3.1] - 2024-12-28

### Fixed
- Fix clippy warnings in storage.rs
- Fix code formatting issues
- Simplify CI workflow for GitHub Actions

## [0.3.0] - 2024-12-28

### Changed
- **Breaking**: Updated to Zenoh 1.7.0 (pinned with exact version `=1.7.0`)
- Updated Rust toolchain to 1.91.1 for compatibility with zenohd
- Disabled default features for `zenoh_backend_traits` to avoid feature mismatch

### Added
- Comprehensive zenohd integration tests:
  - Plugin loading verification
  - PUT/GET/DELETE operations
  - Wildcard queries (`*` and `**`)
  - Data persistence across zenohd restarts
  - Multiple keys persistence
- Automatic compatibility check for storage_manager plugin
- Tests skip gracefully when plugins are unavailable or incompatible

### Fixed
- Plugin compatibility with zenohd (Rust version, Zenoh version, and feature set matching)

### Documentation
- Updated CLAUDE.md with version compatibility requirements
- Updated README.md with current Zenoh version and simplified examples
- Updated Dockerfile to use Rust 1.91.1 and Zenoh 1.7.0

## [0.2.0] - 2024-12-28

### Added
- Zenoh plugin system integration (`RedbBackendPlugin`, `RedbVolume`, `RedbStoragePlugin`)
- Dynamic plugin loading via `zenoh_plugin_trait::declare_plugin!`
- Docker/Podman support for version-matched testing
- Benchmarks for storage and backend operations

### Architecture
- Dual-table storage design (payloads + data_info tables)
- Thread-local buffers for zero-allocation PUT/GET operations
- Plugin hierarchy: RedbBackendPlugin -> RedbVolume -> RedbStoragePlugin

## [0.1.0] - 2024-12-27

### Added
- Initial implementation of zenoh-backend-redb
- Core backend and storage management
- redb database integration with ACID compliance
- Support for CRUD operations (put, get, delete)
- Wildcard query support (`*` and `**` patterns)
- Prefix-based queries for efficient filtering
- Configurable storage options (cache size, fsync, read-only mode)
- Prefix stripping for efficient key storage
- Comprehensive error handling with custom error types
- Unit tests with 22+ test cases
- Basic usage example
- Complete API documentation

### Architecture
- `RedbBackend` - Manages multiple storage instances
- `RedbStorage` - Handles CRUD operations and queries
- `RedbBackendConfig` - Backend-level configuration
- `RedbStorageConfig` - Per-storage configuration
- `StoredValue` - Value structure with payload, timestamp, and encoding

### Features
- Pure Rust implementation with zero C dependencies
- Zero-copy reads via redb's memory-mapping
- MVCC support for concurrent reads
- Flexible configuration per storage instance
- Efficient wildcard pattern matching
- Optional fsync for durability control

---

## Links

- [Repository](https://git.marcpardo.eu/marcpardo/zenoh-backend-redb)
- [Issue Tracker](https://git.marcpardo.eu/marcpardo/zenoh-backend-redb/issues)
- [Zenoh Documentation](https://zenoh.io/docs/)
- [redb Documentation](https://docs.rs/redb/)
