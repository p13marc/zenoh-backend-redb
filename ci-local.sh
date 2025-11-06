#!/usr/bin/env bash

set -e

echo "=== Running CI checks locally ==="
echo ""

echo "1. Checking formatting..."
cargo fmt --all -- --check
echo "✅ Formatting OK"
echo ""

echo "2. Running cargo check..."
cargo check --all-targets
echo "✅ Check OK"
echo ""

echo "3. Building..."
cargo build
echo "✅ Build OK"
echo ""

echo "4. Running clippy..."
cargo clippy --all-targets -- -D warnings
echo "✅ Clippy OK"
echo ""

echo "5. Running tests..."
cargo test --workspace -- --skip test_zenohd --skip test_plugin_library_exists
echo "✅ Tests OK"
echo ""

echo "=== All CI checks passed! ==="
