#!/usr/bin/env bash
set -euo pipefail

for command in timeout head od; do
    command -v "$command" >/dev/null 2>&1 || exit 1
done

port="${HTAPD_HEALTHCHECK_PORT:-3307}"
bytes="$(
    timeout 3 bash -c '
        exec 3<>"/dev/tcp/127.0.0.1/$1"
        head -c 5 <&3 | od -An -v -t x1
    ' bash "$port"
)" || exit 1

set -- $bytes
[[ $# -eq 5 && "$4" == "00" && "$5" == "0a" ]]
