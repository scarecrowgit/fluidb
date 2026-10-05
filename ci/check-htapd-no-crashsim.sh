#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

if ! tree_output="$(cargo tree --locked -e normal,features -p htapd -i htap-common)"; then
    echo "error: failed to inspect htapd features with cargo tree" >&2
    exit 1
fi

if grep -q 'feature "crashsim"' <<<"$tree_output"; then
    echo "error: htapd enables the htap-common crashsim feature" >&2
    exit 1
fi

exit 0
