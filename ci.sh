#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"

echo "========================================="
echo "Running: cargo fmt --all -- --check"
echo "========================================="
cargo fmt --all -- --check

echo "========================================="
echo "Running: cargo clippy --workspace --all-targets -- -D warnings"
echo "========================================="
cargo clippy --workspace --all-targets -- -D warnings

# Workspace-wide durable-mutation gate; the following guard limits lint allowances to approved durability boundaries.
# The dev-only htap-crashsim crate is intentionally excluded because it materializes crash images with raw std::fs.
echo "========================================="
echo "Running: CLIPPY_CONF_DIR=ci/clippy-durability cargo clippy --workspace --exclude htap-crashsim --lib --bins -- -A clippy::all -D clippy::disallowed_methods"
echo "========================================="
CLIPPY_CONF_DIR=ci/clippy-durability cargo clippy --workspace --exclude htap-crashsim --lib --bins -- -A clippy::all -D clippy::disallowed_methods

echo "========================================="
echo "Checking: disallowed_methods annotations are limited to approved durability boundaries"
echo "========================================="
allowed_disallowed_methods_files=(
    "crates/htap-common/src/fs/dur.rs"
    "crates/htap-rowstore/src/sst.rs"
    "crates/htap-server/src/spill.rs"
    "crates/htap-server/src/ipc/owner.rs"
)

if disallowed_methods_files_output="$(grep -rl --include='*.rs' 'disallowed_methods' crates/)"; then
    disallowed_methods_grep_status=0
else
    disallowed_methods_grep_status=$?
fi
if ((disallowed_methods_grep_status != 0 && disallowed_methods_grep_status != 1)); then
    echo "error: failed to scan crates/ for disallowed_methods annotations (grep exit status $disallowed_methods_grep_status)" >&2
    exit 1
fi

disallowed_methods_files=()
if [[ -n "$disallowed_methods_files_output" ]]; then
    while IFS= read -r file; do
        disallowed_methods_files+=("$file")
    done <<<"$disallowed_methods_files_output"
fi

guard_failed=false
for allowed_file in "${allowed_disallowed_methods_files[@]}"; do
    found=false
    for file in "${disallowed_methods_files[@]}"; do
        if [[ "$file" == "$allowed_file" ]]; then
            found=true
            break
        fi
    done
    if [[ "$found" == false ]]; then
        echo "error: approved durability boundary is missing a disallowed_methods annotation: $allowed_file" >&2
        guard_failed=true
    fi
done

for file in "${disallowed_methods_files[@]}"; do
    allowed=false
    for allowed_file in "${allowed_disallowed_methods_files[@]}"; do
        if [[ "$file" == "$allowed_file" ]]; then
            allowed=true
            break
        fi
    done
    if [[ "$allowed" == false ]]; then
        echo "error: disallowed_methods annotation found outside approved durability boundaries: $file" >&2
        guard_failed=true
    fi
done

if [[ "$guard_failed" == true ]]; then
    exit 1
fi

lint_scan_paths=(crates)
if [[ -d vendor ]]; then
    lint_scan_paths+=(vendor)
fi
if lint_allow_files_output="$(grep -rEl --include='*.rs' '#!?\[[[:space:]]*allow[[:space:]]*\([^)]*(clippy::all|clippy::style|warnings)' "${lint_scan_paths[@]}")"; then
    lint_allow_grep_status=0
else
    lint_allow_grep_status=$?
fi
if ((lint_allow_grep_status != 0 && lint_allow_grep_status != 1)); then
    echo "error: failed to scan for lint-group allows (grep exit status $lint_allow_grep_status)" >&2
    exit 1
fi
if [[ -n "$lint_allow_files_output" ]]; then
    while IFS= read -r file; do
        echo "error: lint-group allow that can silence disallowed_methods found: $file" >&2
    done <<<"$lint_allow_files_output"
    exit 1
fi

lint_table_scan_paths=(Cargo.toml crates)
if [[ -d vendor ]]; then
    lint_table_scan_paths+=(vendor)
fi
if lint_table_files_output="$(grep -rEl --include='Cargo.toml' '^[[:space:]]*(\[(workspace\.)?lints(\.[^]]+)?\][[:space:]]*$|lints([.]workspace)?[[:space:]]*=[[:space:]]*(\{|true)([[:space:]]|$))' "${lint_table_scan_paths[@]}")"; then
    lint_table_grep_status=0
else
    lint_table_grep_status=$?
fi
if ((lint_table_grep_status != 0 && lint_table_grep_status != 1)); then
    echo "error: failed to scan Cargo.toml files for lints configuration (grep exit status $lint_table_grep_status)" >&2
    exit 1
fi
if [[ -n "$lint_table_files_output" ]]; then
    while IFS= read -r file; do
        echo "error: lints configuration found in workspace Cargo.toml: $file" >&2
    done <<<"$lint_table_files_output"
    exit 1
fi

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
