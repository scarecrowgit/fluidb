#!/bin/sh
set -eu

if [ "${HTAPD_PASSWORD+x}" = x ] && [ "${HTAPD_PASSWORD_FILE+x}" = x ]; then
    echo "error: set only one of HTAPD_PASSWORD or HTAPD_PASSWORD_FILE" >&2
    exit 1
fi

if [ "${HTAPD_PASSWORD_FILE+x}" = x ]; then
    if [ ! -r "$HTAPD_PASSWORD_FILE" ] || [ ! -s "$HTAPD_PASSWORD_FILE" ]; then
        echo "error: HTAPD_PASSWORD_FILE must name a readable, non-empty file" >&2
        exit 1
    fi

    # Command substitution intentionally strips trailing newlines from the secret.
    HTAPD_PASSWORD="$(cat "$HTAPD_PASSWORD_FILE")"
    export HTAPD_PASSWORD
    unset HTAPD_PASSWORD_FILE
fi

if [ -z "${HTAPD_PASSWORD:-}" ] && [ "${HTAPD_ALLOW_EMPTY_PASSWORD:-}" != "1" ]; then
    echo "error: set HTAPD_PASSWORD_FILE or HTAPD_PASSWORD, or set HTAPD_ALLOW_EMPTY_PASSWORD=1" >&2
    exit 1
fi

exec "$@"
