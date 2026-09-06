#!/usr/bin/env bash
set -euo pipefail

echo "========================================="
echo "Running: cargo fmt --all -- --check"
echo "========================================="
cargo fmt --all -- --check

echo "========================================="
echo "Running: cargo clippy --workspace --all-targets -- -D warnings"
echo "========================================="
cargo clippy --workspace --all-targets -- -D warnings

echo "========================================="
echo "Running: cargo build --workspace"
echo "========================================="
cargo build --workspace

echo "========================================="
echo "Running: cargo test --workspace"
echo "========================================="
cargo test --workspace

echo "========================================="
echo "CI checks completed successfully!"
echo "========================================="
