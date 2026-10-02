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

# Phase 20 durable-mutation gate; each later migration batch adds its crate to the -p list.
# The dev-only htap-crashsim crate is intentionally excluded because it materializes crash images with raw std::fs.
echo "========================================="
echo "Running: CLIPPY_CONF_DIR=ci/clippy-durability cargo clippy -p htap-common -p htap-rowstore -p htap-txn -p htap-catalog -p htap-coord --lib --bins -- -A clippy::all -D clippy::disallowed_methods"
echo "========================================="
CLIPPY_CONF_DIR=ci/clippy-durability cargo clippy -p htap-common -p htap-rowstore -p htap-txn -p htap-catalog -p htap-coord --lib --bins -- -A clippy::all -D clippy::disallowed_methods

echo "========================================="
echo "Running: cargo clippy -p htap-common --all-targets -- -D warnings"
echo "========================================="
cargo clippy -p htap-common --all-targets -- -D warnings

echo "========================================="
echo "Running: cargo test -p htap-common"
echo "========================================="
cargo test -p htap-common

echo "========================================="
echo "Checking: htapd must not enable the htap-common crashsim feature"
echo "========================================="
tree_output="$(cargo tree -e normal,features -p htapd -i htap-common)"
if grep -q 'feature "crashsim"' <<<"$tree_output"; then
    echo "error: htapd enables the htap-common crashsim feature" >&2
    exit 1
fi

echo "========================================="
echo "Running: cargo build --workspace --exclude htap-crashsim"
echo "========================================="
cargo build --workspace --exclude htap-crashsim

echo "========================================="
echo "Running: cargo test --workspace"
echo "========================================="
cargo test --workspace

echo "========================================="
echo "Running: cargo bench --workspace --exclude htap-crashsim --no-run"
echo "========================================="
cargo bench --workspace --exclude htap-crashsim --no-run

echo "========================================="
echo "CI checks completed successfully!"
echo "========================================="
