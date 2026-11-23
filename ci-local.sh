#!/usr/bin/env bash
# The jobs of .forgejo/workflows/ci.yml, in the same order and with the same
# flags, except `conformance` (which builds zenohd; see `just conformance`).
# If this passes and CI does not, one of the two has drifted: fix this file.

set -euo pipefail

step() { printf '\n=== %s ===\n' "$1"; }

step "fmt"
cargo fmt --all --check

step "clippy"
cargo clippy --all-targets --all-features --locked -- -D warnings

step "test"
cargo build --all-features --locked
cargo build --release --features plugin --locked
cargo test --all-features --locked -- --skip test_zenohd
cargo test --release --all-features --locked --test retention_memory

step "deny"
if command -v cargo-deny >/dev/null; then
    cargo deny check
else
    echo "cargo-deny is not installed; CI runs it. Install: cargo install cargo-deny" >&2
    exit 1
fi

step "docs"
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps --locked

printf '\nAll CI checks passed (conformance not run).\n'
