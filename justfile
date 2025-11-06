# justfile for zenoh-backend-redb
# Install just: cargo install just
# Usage: just <recipe>

# Default recipe (list all recipes)
default:
    @just --list

# Quick check (format and clippy)
check:
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings

# Format code
fmt:
    cargo fmt --all

# Run clippy
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Auto-fix clippy issues
clippy-fix:
    cargo clippy --all-targets --all-features --fix --allow-dirty

# Auto-fix all issues
fix: fmt clippy-fix

# Build project
build:
    cargo build --all-features

# Build release
build-release:
    cargo build --release --all-features

# Run all tests (excluding zenohd integration tests)
test:
    cargo test --all-features --verbose -- --skip test_zenohd --skip test_plugin_library_exists

# Run tests with nextest (excluding zenohd integration tests)
nextest:
    cargo nextest run --all-features -E 'not test(test_zenohd) and not test(test_plugin_library_exists)'

# Run specific test
test-one TEST:
    cargo test --all-features {{TEST}}

# Watch and run tests on changes
test-watch:
    cargo watch -x test

# Generate code coverage
coverage:
    cargo tarpaulin --all-features --workspace --timeout 300 --out Html --out Xml

# Open coverage report
coverage-open: coverage
    @if [ -f tarpaulin-report.html ]; then \
        open tarpaulin-report.html || xdg-open tarpaulin-report.html || start tarpaulin-report.html; \
    fi

# Security audit
# Ignoring known upstream vulnerabilities from Zenoh dependencies
audit:
    cargo audit --ignore RUSTSEC-2023-0071 --ignore RUSTSEC-2024-0436

# Check for unused dependencies (nightly required)
udeps:
    cargo +nightly udeps --all-targets

# Check for unused dependencies with machete
machete:
    cargo machete

# Check licenses and advisories
deny:
    cargo deny check

# Check for outdated dependencies
outdated:
    cargo outdated

# Update dependencies
update:
    cargo update

# Generate documentation
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps

# Generate and open documentation
doc-open:
    cargo doc --all-features --no-deps --open

# Build all examples
examples:
    cargo build --examples

# Run specific example
example EXAMPLE:
    cargo run --example {{EXAMPLE}}

# Run basic_usage example
run-example:
    cargo run --example basic_usage

# Clean build artifacts
clean:
    cargo clean

# Full clean including cache
clean-all: clean
    rm -rf target/
    rm -f Cargo.lock
    rm -rf example_databases/
    rm -rf zenoh_redb_backend/
    rm -rf *.redb

# Run all pre-commit checks
pre-commit: check test doc audit

# Simulate CI pipeline locally (full checks)
ci: check test doc audit deny

# Run CI checks locally (matches GitHub Actions)
ci-local:
    ./ci-local.sh

# Run CI using act (Docker-based GitHub Actions simulation)
ci-act:
    ./bin/act push -j ci

# Run comprehensive quality checks
quality: check test coverage doc audit udeps machete deny outdated

# Build plugin for release
plugin:
    cargo build --release --features plugin

# Install plugin to ~/.zenoh/lib/
install-plugin: plugin
    #!/usr/bin/env bash
    mkdir -p ~/.zenoh/lib
    if [ -f target/release/libzenoh_backend_redb.so ]; then
        cp target/release/libzenoh_backend_redb.so ~/.zenoh/lib/
        echo "Plugin installed: ~/.zenoh/lib/libzenoh_backend_redb.so"
    elif [ -f target/release/libzenoh_backend_redb.dylib ]; then
        cp target/release/libzenoh_backend_redb.dylib ~/.zenoh/lib/
        echo "Plugin installed: ~/.zenoh/lib/libzenoh_backend_redb.dylib"
    elif [ -f target/release/zenoh_backend_redb.dll ]; then
        cp target/release/zenoh_backend_redb.dll ~/.zenoh/lib/
        echo "Plugin installed: ~/.zenoh/lib/zenoh_backend_redb.dll"
    else
        echo "Error: Plugin library not found"
        exit 1
    fi

# Watch and rebuild on changes
watch:
    cargo watch -x build

# Check MSRV (Minimum Supported Rust Version)
msrv:
    #!/usr/bin/env bash
    echo "Checking MSRV (Rust 1.70)..."
    rustup toolchain install 1.70
    cargo +1.70 check --all-features

# Install all quality tools
install-tools:
    #!/usr/bin/env bash
    echo "Installing quality tools..."
    cargo install cargo-nextest
    cargo install cargo-tarpaulin
    cargo install cargo-audit
    cargo install cargo-udeps --locked
    cargo install cargo-machete
    cargo install cargo-deny
    cargo install cargo-outdated
    cargo install cargo-watch
    echo "All tools installed!"

# Prepare for release
release-prep: fmt clippy test doc audit deny
    #!/usr/bin/env bash
    echo "✅ Release preparation complete!"
    echo ""
    echo "Next steps:"
    echo "  1. Update version in Cargo.toml"
    echo "  2. Update CHANGELOG.md"
    echo "  3. Commit changes: git commit -am 'Release vX.Y.Z'"
    echo "  4. Create tag: git tag -a vX.Y.Z -m 'Release vX.Y.Z'"
    echo "  5. Push: git push && git push --tags"

# Run all benchmarks
bench:
    cargo bench --all-features

# Run storage benchmarks only
bench-storage:
    cargo bench --bench storage_benchmarks

# Run backend benchmarks only
bench-backend:
    cargo bench --bench backend_benchmarks

# Run specific benchmark
bench-one BENCH:
    cargo bench --bench {{BENCH}}

# Run benchmarks and save as baseline
bench-baseline NAME:
    cargo bench -- --save-baseline {{NAME}}

# Compare benchmarks against baseline
bench-compare BASELINE:
    cargo bench -- --baseline {{BASELINE}}

# Run quick benchmarks (less accurate, faster)
bench-quick:
    cargo bench -- --quick

# Open benchmark report
bench-report:
    #!/usr/bin/env bash
    if [ -f target/criterion/report/index.html ]; then
        open target/criterion/report/index.html || xdg-open target/criterion/report/index.html || start target/criterion/report/index.html
    else
        echo "No benchmark report found. Run 'just bench' first."
    fi

# Clean benchmark data
bench-clean:
    rm -rf target/criterion

# Show project statistics
stats:
    #!/usr/bin/env bash
    echo "Project Statistics:"
    echo "==================="
    echo ""
    echo "Source files:"
    find src -name "*.rs" | wc -l
    echo ""
    echo "Lines of code (src):"
    find src -name "*.rs" -exec cat {} \; | wc -l
    echo ""
    echo "Test files:"
    find tests -name "*.rs" 2>/dev/null | wc -l || echo "0"
    echo ""
    echo "Lines of test code:"
    find tests -name "*.rs" -exec cat {} \; 2>/dev/null | wc -l || echo "0"
    echo ""
    echo "Total dependencies:"
    cargo metadata --format-version 1 --no-deps | jq '.packages | length'

# Verify all checks pass (use before committing)
verify: fmt check test
    @echo "✅ All verifications passed!"

# Run zenohd integration tests (requires plugin build and zenohd installed)
test-zenohd:
    #!/usr/bin/env bash
    echo "Building plugin..."
    cargo build --release --features plugin
    echo "Running zenohd integration tests..."
    cargo test --test integration_zenohd -- --test-threads=1 --nocapture

# Run zenohd integration tests with Podman (ensures matching Zenoh versions)
docker-test-zenohd:
    #!/usr/bin/env bash
    echo "========================================"
    echo "Building test image with zenohd and plugin..."
    echo "Note: First build takes 10-15 minutes (compiling zenohd from source)"
    echo "========================================"
    podman-remote build --build-arg ZENOH_VERSION=1.6.2 --target test -t zenoh-backend-redb:test .
    echo ""
    echo "========================================"
    echo "Running zenohd integration tests..."
    echo "========================================"
    podman-remote run --rm -e RUST_BACKTRACE=1 -e RUST_LOG=info zenoh-backend-redb:test cargo test --test integration_zenohd -- --test-threads=1 --nocapture

# Run zenohd integration tests without cache (clean rebuild)
docker-test-zenohd-no-cache:
    #!/usr/bin/env bash
    echo "========================================"
    echo "Building test image WITHOUT CACHE..."
    echo "Note: This will take 20-40 minutes (full rebuild)"
    echo "========================================"
    podman-remote build --no-cache --build-arg ZENOH_VERSION=1.6.2 --target test -t zenoh-backend-redb:test .
    echo ""
    echo "========================================"
    echo "Running zenohd integration tests..."
    echo "========================================"
    podman-remote run --rm -e RUST_BACKTRACE=1 -e RUST_LOG=info zenoh-backend-redb:test cargo test --test integration_zenohd -- --test-threads=1 --nocapture

# Run zenohd integration tests with Podman using fast build (pre-built Zenoh image)
docker-test-zenohd-fast:
    #!/usr/bin/env

# Build Podman image
docker-build:
    podman-remote build --target runtime -t zenoh-backend-redb:latest .

# Run Podman container
docker-run:
    podman-remote run -d --name zenoh-redb -p 7447:7447 -p 8000:8000 -v zenoh-redb-data:/var/lib/zenoh/redb zenoh-backend-redb:latest

# Stop and remove Podman container
docker-stop:
    podman-remote stop zenoh-redb || true
    podman-remote rm zenoh-redb || true

# Clean Podman images and volumes
docker-clean:
    podman-remote rmi zenoh-backend-redb:test zenoh-backend-redb:latest || true
    podman-remote volume rm zenoh-redb-data || true

# Show help
help:
    @echo "Available recipes:"
    @echo ""
    @echo "Development:"
    @echo "  check          - Quick check (format + clippy)"
    @echo "  fmt            - Format code"
    @echo "  clippy         - Run clippy"
    @echo "  fix            - Auto-fix all issues"
    @echo "  build          - Build project"
    @echo "  test           - Run tests"
    @echo "  nextest        - Run tests with nextest"
    @echo "  watch          - Watch and rebuild"
    @echo ""
    @echo "Quality:"
    @echo "  coverage       - Generate code coverage"
    @echo "  audit          - Security audit"
    @echo "  udeps          - Check unused dependencies"
    @echo "  machete        - Check unused dependencies (alt)"
    @echo "  deny           - Check licenses"
    @echo "  outdated       - Check outdated deps"
    @echo "  quality        - Run all quality checks"
    @echo ""
    @echo "Benchmarks:"
    @echo "  bench          - Run all benchmarks"
    @echo "  bench-storage  - Run storage benchmarks only"
    @echo "  bench-backend  - Run backend benchmarks only"
    @echo "  bench-quick    - Run quick benchmarks (faster)"
    @echo "  bench-baseline - Save benchmark baseline"
    @echo "  bench-compare  - Compare against baseline"
    @echo "  bench-report   - Open benchmark HTML report"
    @echo "  bench-clean    - Clean benchmark data"
    @echo ""
    @echo "Documentation:"
    @echo "  doc            - Generate documentation"
    @echo "  doc-open       - Generate and open docs"
    @echo ""
    @echo "Plugin:"
    @echo "  plugin         - Build plugin"
    @echo "  install-plugin - Install plugin locally"
    @echo ""
    @echo "CI/CD:"
    @echo "  ci                  - Simulate CI pipeline (full)"
    @echo "  ci-local            - Run CI checks (matches GitHub)"
    @echo "  ci-act              - Run CI with act (Docker)"
    @echo "  pre-commit          - Pre-commit checks"
    @echo "  verify              - Verify before commit"
    @echo "  test-zenohd              - Run zenohd integration tests (local)"
    @echo "  docker-test-zenohd       - Run zenohd tests in Docker (recommended)"
    @echo "  docker-test-zenohd-no-cache - Run zenohd tests without cache (clean rebuild)"
    @echo "  release-prep             - Prepare for release"
    @echo ""
    @echo "Podman:"
    @echo "  docker-build   - Build Podman image"
    @echo "  docker-run     - Run Podman container"
    @echo "  docker-stop    - Stop and remove container"
    @echo "  docker-clean   - Clean images and volumes"
    @echo ""
    @echo "Utilities:"
    @echo "  clean          - Clean build artifacts"
    @echo "  install-tools  - Install quality tools"
    @echo "  stats          - Show project statistics"
    @echo "  help           - Show this help"
    @echo ""
    @echo "Usage: just <recipe>"
