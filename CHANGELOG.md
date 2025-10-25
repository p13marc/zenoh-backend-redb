# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial implementation of zenoh-backend-redb
- Core backend and storage management
- redb database integration with ACID compliance
- Support for CRUD operations (put, get, delete)
- Wildcard query support (`*` and `**` patterns)
- Prefix-based queries for efficient filtering
- Configurable storage options (cache size, fsync, etc.)
- Read-only storage mode
- Prefix stripping for efficient key storage
- Comprehensive error handling with custom error types
- Unit tests with 23+ test cases
- Basic usage example
- Complete API documentation
- README with installation and usage instructions

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

## [0.1.0] - TBD

### Added
- Initial alpha release
- Basic storage backend functionality
- Core features stabilized and tested
- Documentation and examples ready for community feedback

### Known Limitations
- Zenoh plugin trait integration not yet complete
- No benchmarks available yet
- Missing some advanced features (compression, backup/restore)

### Future Plans
- Complete Zenoh plugin system integration
- Add performance benchmarks
- Implement compression support
- Add backup/restore utilities
- Prometheus metrics export
- Production hardening and testing

---

## Version History

- **0.1.0** - Initial alpha release (planned)
- **Unreleased** - Active development

## Contributing

See [TODO.md](TODO.md) for the development roadmap and contribution opportunities.

## Links

- [Repository](https://github.com/yourusername/zenoh-backend-redb)
- [Issue Tracker](https://github.com/yourusername/zenoh-backend-redb/issues)
- [Zenoh Documentation](https://zenoh.io/docs/)
- [redb Documentation](https://docs.rs/redb/)