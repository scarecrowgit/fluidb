#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

dockerfile="${1:-${DOCKERFILE:-./Dockerfile}}"

if [[ ! -f "$dockerfile" ]]; then
    echo "error: Dockerfile not found: $dockerfile" >&2
    exit 1
fi

rust_channel="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' rust-toolchain.toml)"
rust_image="$(sed -n 's/^ARG RUST_IMAGE=\([^[:space:]]*\)$/\1/p' "$dockerfile")"

if [[ -z "$rust_channel" ]]; then
    echo "error: could not read the Rust channel from rust-toolchain.toml" >&2
    exit 1
fi
if [[ -z "$rust_image" ]]; then
    echo "error: could not read ARG RUST_IMAGE from Dockerfile" >&2
    exit 1
fi
if [[ "$rust_image" != rust:"$rust_channel"-* ]]; then
    echo "error: Dockerfile RUST_IMAGE does not match rust-toolchain.toml channel $rust_channel: $rust_image" >&2
    exit 1
fi

for pattern in target .git secrets/ '*.pem' '*.key'; do
    if ! grep -Fxq "$pattern" .dockerignore; then
        echo "error: .dockerignore must exclude $pattern" >&2
        exit 1
    fi
done

while IFS= read -r image_arg; do
    if [[ ! "$image_arg" =~ @sha256:[0-9a-f]{64}$ ]]; then
        echo "error: Dockerfile base image ARG lacks a sha256 digest: $image_arg" >&2
        exit 1
    fi
done < <(sed -n 's/^ARG [A-Z_]*_IMAGE=\([^[:space:]]*\)$/\1/p' "$dockerfile")

declare -A image_args=()
declare -A image_arg_used=()
declare -A stage_names=()

while IFS= read -r image_arg_name; do
    image_args["$image_arg_name"]=1
done < <(sed -n 's/^ARG \([A-Z_]*_IMAGE\)=.*/\1/p' "$dockerfile")

while IFS= read -r from_line; do
    from_spec="${from_line#FROM }"

    # Remove supported FROM options before extracting the image reference.
    while [[ "$from_spec" == --* ]]; do
        from_spec="${from_spec#* }"
    done

    from_image="${from_spec%%[[:space:]]*}"
    from_rest="${from_spec#"$from_image"}"

    if [[ "$from_image" =~ ^\$\{([A-Z_][A-Z0-9_]*)\}$ ]]; then
        from_arg="${BASH_REMATCH[1]}"

        if [[ -z "${image_args[$from_arg]:-}" ]]; then
            echo "error: FROM uses undefined or non-image ARG: $from_image" >&2
            exit 1
        fi

        from_arg_value="$(sed -n "s/^ARG $from_arg=\\([^[:space:]]*\\)$/\\1/p" "$dockerfile")"
        if [[ ! "$from_arg_value" =~ @sha256:[0-9a-f]{64}$ ]]; then
            echo "error: FROM uses an unpinned ARG: $from_image" >&2
            exit 1
        fi

        image_arg_used["$from_arg"]=1
    elif [[ "$from_image" == *[/:\@]* ]]; then
        if [[ ! "$from_image" =~ @sha256:[0-9a-f]{64}$ ]]; then
            echo "error: FROM image lacks a sha256 digest: $from_image" >&2
            exit 1
        fi
    elif [[ -z "${stage_names[$from_image]:-}" ]]; then
        echo "error: FROM references an unknown stage or unpinned image: $from_image" >&2
        exit 1
    fi

    if [[ "$from_rest" =~ [[:space:]]+[Aa][Ss][[:space:]]+([A-Za-z0-9][A-Za-z0-9_.-]*) ]]; then
        stage_names["${BASH_REMATCH[1]}"]=1
    fi
done < <(sed -n 's/^[[:space:]]*FROM[[:space:]]\+\(.*\)$/FROM \1/p' "$dockerfile")

for image_arg_name in "${!image_args[@]}"; do
    if [[ -z "${image_arg_used[$image_arg_name]:-}" ]]; then
        echo "error: Dockerfile image ARG is not used by a FROM line: $image_arg_name" >&2
        exit 1
    fi
done

if ! grep -q '^ARG [A-Z_]*_IMAGE=' "$dockerfile"; then
    echo "error: Dockerfile defines no base image ARGs" >&2
    exit 1
fi
