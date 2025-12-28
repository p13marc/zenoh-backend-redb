# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
