# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
  creating one.
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

- [Repository](https://github.com/p13marc/zenoh-backend-redb)
- [Issue Tracker](https://github.com/p13marc/zenoh-backend-redb/issues)
- [Zenoh Documentation](https://zenoh.io/docs/)
- [redb Documentation](https://docs.rs/redb/)
