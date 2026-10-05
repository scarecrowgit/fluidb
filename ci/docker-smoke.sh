#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

readonly image_tag="fluidb/htapd:smoke-$(date +%s)-$$"
readonly compose_project="fluidb-smoke-$(date +%s)-$$"
readonly smoke_dir="$(mktemp -d "${TMPDIR:-/tmp}/fluidb-docker-smoke.XXXXXX")"
readonly secrets_dir="$smoke_dir/secrets"
readonly tls_dir="$smoke_dir/tls"
readonly port_override="$smoke_dir/docker-compose.port.yml"
plain_container_id=""
tls_container_id=""
plain_started_at=0

compose_plain() {
    COMPOSE_PROJECT_NAME="$compose_project" \
    HTAPD_IMAGE="$image_tag" \
    HTAPD_SECRETS_DIR="$secrets_dir" \
    docker compose -f docker-compose.yml -f "$port_override" "$@"
}

compose_tls() {
    COMPOSE_PROJECT_NAME="${compose_project}-tls" \
    HTAPD_IMAGE="$image_tag" \
    HTAPD_SECRETS_DIR="$secrets_dir" \
    HTAPD_TLS_DIR="$tls_dir" \
    docker compose -f docker-compose.yml -f docker-compose.tls.yml -f "$port_override" "$@"
}

cleanup_compose() {
    local mode="$1"
    local container_id="$2"

    if [[ -n "$container_id" ]]; then
        "$mode" ps || true
        "$mode" logs || true
        docker inspect "$container_id" || true
    fi
    "$mode" down -v --remove-orphans || true
}

cleanup() {
    local status=$?

    cleanup_compose compose_plain "$plain_container_id"
    cleanup_compose compose_tls "$tls_container_id"
    docker image rm -f "$image_tag" >/dev/null 2>&1 || true
    rm -rf "$smoke_dir"
    docker system df || true
    exit "$status"
}
trap cleanup EXIT

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "error: required command not found: $1" >&2
        exit 1
    fi
}

wait_healthy() {
    local container_id="$1"
    local deadline=$((SECONDS + 60))
    local health

    while ((SECONDS < deadline)); do
        health="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$container_id")"
        if [[ "$health" == healthy ]]; then
            return 0
        fi
        if [[ "$health" == unhealthy || "$(docker inspect --format '{{.State.Running}}' "$container_id")" != true ]]; then
            echo "error: container $container_id did not become healthy (status: $health)" >&2
            docker inspect "$container_id" >&2 || true
            return 1
        fi
        sleep 1
    done

    echo "error: timed out waiting for container $container_id to become healthy" >&2
    return 1
}

container_address() {
    local mode="$1"
    local published

    mapfile -t published < <("$mode" port htapd 3307)
    if ((${#published[@]} != 1)); then
        echo "error: expected exactly one published port mapping, got ${#published[@]}" >&2
        printf '%s\n' "${published[@]}" >&2
        return 1
    fi
    case "${published[0]}" in
        127.0.0.1:*) printf '%s\n' "${published[0]}" ;;
        *) echo "error: expected loopback published port, got: ${published[0]}" >&2; return 1 ;;
    esac
}

run_client_phase() {
    local phase="$1"
    local test_name="$2"
    local address="$3"
    shift 3

    env \
        HTAP_DOCKER_SMOKE_ADDR="$address" \
        HTAP_DOCKER_SMOKE_PASSWORD="$(<"$secrets_dir/htapd_root_password")" \
        HTAP_DOCKER_SMOKE_PHASE="$phase" \
        "$@" \
        cargo test -p htap-client --test docker_smoke "$test_name" -- --ignored --exact
}

assert_exit_code() {
    local container_id="$1"
    local expected="$2"
    local actual

    actual="$(docker inspect --format '{{.State.ExitCode}}' "$container_id")"
    if [[ "$actual" != "$expected" ]]; then
        echo "error: expected container $container_id to exit $expected, got $actual" >&2
        return 1
    fi
}

require_command docker
require_command openssl
require_command df
require_command cargo

if ! docker compose version >/dev/null; then
    echo "error: docker compose is unavailable" >&2
    exit 1
fi
if ! docker buildx version >/dev/null; then
    echo "error: docker buildx is unavailable" >&2
    exit 1
fi
if ! docker info >/dev/null; then
    echo "error: Docker daemon is unreachable" >&2
    exit 1
fi

docker_root="/mnt/storage/docker"
if [[ -d "$docker_root" ]]; then
    docker_free_kb="$(df -Pk "$docker_root" | awk 'NR == 2 { print $4 }')"
    if [[ -n "$docker_free_kb" && "$docker_free_kb" -lt 8388608 ]]; then
        echo "warning: less than 8 GB free at $docker_root; Docker smoke build may exhaust disk" >&2
    fi
else
    echo "warning: Docker root $docker_root does not exist; skipping its free-space check" >&2
fi

rust_image="$(sed -n 's/^ARG RUST_IMAGE=\([^[:space:]]*\)$/\1/p' Dockerfile)"
if [[ -z "$rust_image" ]] || ! docker buildx imagetools inspect "$rust_image" >/dev/null; then
    echo "error: Docker Hub registry is unreachable or the pinned Rust base image cannot be resolved" >&2
    exit 1
fi

mkdir -p "$secrets_dir" "$tls_dir"
chmod 0700 "$secrets_dir" "$tls_dir"

umask 077
openssl rand -base64 36 | tr -d '\n' >"$secrets_dir/htapd_root_password"
openssl req -x509 -new -nodes -sha256 -days 1 \
    -subj '/CN=fluidb-docker-smoke-ca' \
    -keyout "$tls_dir/ca.key" -out "$tls_dir/ca.crt"
openssl req -new -nodes \
    -subj '/CN=localhost' \
    -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1' \
    -keyout "$tls_dir/htapd.key" -out "$tls_dir/htapd.csr"
openssl x509 -req -sha256 -days 1 \
    -in "$tls_dir/htapd.csr" \
    -CA "$tls_dir/ca.crt" -CAkey "$tls_dir/ca.key" -CAcreateserial \
    -extfile <(printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\n') \
    -out "$tls_dir/htapd.crt"
rm -f "$tls_dir/ca.key" "$tls_dir/htapd.csr" "$tls_dir/ca.srl"
chmod 0444 "$secrets_dir/htapd_root_password" "$tls_dir/ca.crt" "$tls_dir/htapd.crt" "$tls_dir/htapd.key"

cat >"$port_override" <<'EOF'
services:
  htapd:
    ports: !override
      - "127.0.0.1::3307"
    restart: "no"
EOF

echo "========================================="
echo "Building Docker smoke image: $image_tag"
echo "========================================="
docker build --tag "$image_tag" .

if [[ "$(docker run --rm --entrypoint id "$image_tag" -u)" != 10001 ]]; then
    echo "error: image does not run as uid 10001" >&2
    exit 1
fi

image_size="$(docker image inspect "$image_tag" --format '{{.Size}}')"
image_size_budget="${HTAPD_IMAGE_SIZE_BUDGET:-43000000}"
echo "Docker smoke image size: $image_size bytes (budget: $image_size_budget bytes)"
if ((image_size > image_size_budget)); then
    echo "error: Docker smoke image size $image_size bytes exceeds budget $image_size_budget bytes" >&2
    exit 1
fi

no_password_output=""
if no_password_output="$(docker run --rm "$image_tag" true 2>&1)"; then
    echo "error: image accepted a startup without a password source" >&2
    printf '%s\n' "$no_password_output" >&2
    exit 1
fi
if [[ "$no_password_output" != *"set HTAPD_PASSWORD_FILE or HTAPD_PASSWORD, or set HTAPD_ALLOW_EMPTY_PASSWORD=1"* ]]; then
    echo "error: image did not report the expected missing-password-source message" >&2
    printf '%s\n' "$no_password_output" >&2
    exit 1
fi

password="$(<"$secrets_dir/htapd_root_password")"
# The password is generated after the build and bind-mounted, never copied into image metadata.
if docker history --no-trunc "$image_tag" | grep -Fq "$password" ||
    docker image inspect "$image_tag" | grep -Fq "$password"; then
    echo "error: generated smoke password appears in image metadata" >&2
    exit 1
fi

echo "========================================="
echo "Running plain Compose smoke scenario"
echo "========================================="
compose_plain up -d --no-build --wait --wait-timeout 60
plain_container_id="$(compose_plain ps -q htapd)"
plain_address="$(container_address compose_plain)"

run_client_phase wrong_password smoke_wrong_password_rejected "$plain_address"
run_client_phase write smoke_write "$plain_address"

plain_started_at="$(date +%s)"
compose_plain stop -t 15
stop_finished_at="$(date +%s)"
assert_exit_code "$plain_container_id" 143
if [[ "$(docker inspect --format '{{.State.OOMKilled}}' "$plain_container_id")" != false ]]; then
    echo "error: plain container was OOM-killed during SIGTERM stop" >&2
    exit 1
fi
if ((stop_finished_at - plain_started_at >= 15)); then
    echo "error: SIGTERM stop consumed the full 15-second grace period" >&2
    exit 1
fi

compose_plain start
wait_healthy "$plain_container_id"
new_container_id="$(compose_plain ps -q htapd)"
if [[ "$new_container_id" != "$plain_container_id" ]]; then
    echo "error: container ID changed across start: $plain_container_id -> $new_container_id" >&2
    exit 1
fi
plain_address="$(container_address compose_plain)"
run_client_phase verify smoke_verify "$plain_address"

compose_plain kill -s KILL
assert_exit_code "$plain_container_id" 137
compose_plain start
wait_healthy "$plain_container_id"
new_container_id="$(compose_plain ps -q htapd)"
if [[ "$new_container_id" != "$plain_container_id" ]]; then
    echo "error: container ID changed across start: $plain_container_id -> $new_container_id" >&2
    exit 1
fi
plain_address="$(container_address compose_plain)"
run_client_phase verify smoke_verify "$plain_address"
run_client_phase write smoke_write "$plain_address"

if [[ "$(compose_plain exec -T htapd id -u)" != 10001 ]]; then
    echo "error: Compose service does not run as uid 10001" >&2
    exit 1
fi
if compose_plain exec -T htapd touch /x; then
    echo "error: root filesystem is unexpectedly writable" >&2
    exit 1
fi
compose_plain exec -T htapd touch /var/lib/htapd/docker-smoke-write-check
compose_plain exec -T htapd rm /var/lib/htapd/docker-smoke-write-check

plain_logs="$(compose_plain logs)"
if ! grep -Fq 'htapd ready' <<<"$plain_logs"; then
    echo "error: plain Compose logs do not contain htapd ready" >&2
    exit 1
fi
if ! grep -Fq 'accepting plaintext connections' <<<"$plain_logs"; then
    echo "error: plain Compose logs do not contain the cleartext warning" >&2
    exit 1
fi

compose_plain down -v --remove-orphans

echo "========================================="
echo "Running TLS Compose smoke scenario"
echo "========================================="
compose_tls up -d --no-build --wait --wait-timeout 60
tls_container_id="$(compose_tls ps -q htapd)"
tls_address="$(container_address compose_tls)"

run_client_phase tls_ok smoke_tls_ok "$tls_address" \
    HTAP_DOCKER_SMOKE_CA="$tls_dir/ca.crt" \
    HTAP_DOCKER_SMOKE_SERVER_NAME=localhost
run_client_phase plaintext_rejected smoke_plaintext_rejected_when_secure_transport_required "$tls_address"
run_client_phase tls_bad_name smoke_tls_bad_server_name_rejected "$tls_address" \
    HTAP_DOCKER_SMOKE_CA="$tls_dir/ca.crt" \
    HTAP_DOCKER_SMOKE_SERVER_NAME=localhost

tls_logs="$(compose_tls logs)"
if grep -Fq 'accepting plaintext connections' <<<"$tls_logs"; then
    echo "error: TLS Compose logs contain the cleartext warning" >&2
    exit 1
fi

echo "Docker smoke checks completed successfully."
