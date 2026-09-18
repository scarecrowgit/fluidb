#!/usr/bin/env bash
# One scale, all three engines, all data under $2 (same device for every engine).
# Usage: run_scale.sh <rows> <data-dir>
set -u
S=$(cd "$(dirname "$0")" && pwd)
rows=$1
D=$2
HTAPD=${HTAPD:-/mnt/storage/projects/target/compare-build/release/htapd}
mkdir -p "$D"
[ "$rows" -ge 10000000 ] && export OLAP_REPS=3

client() {
  docker run --rm --network host --cpuset-cpus 8-11 -v fluidb-bench-pydeps:/deps -e PYTHONPATH=/deps \
    -e RESULTS=/bench/results.jsonl -e OLAP_REPS="${OLAP_REPS:-5}" -e OLTP_SECONDS=10 -e MIX_SECONDS=30 \
    -v "$S":/bench -w /bench python:3.12-bookworm python bench.py "$@"
}

wipe() { docker run --rm -v "$D":/d alpine:3.20 rm -rf "/d/$1"; }

start_fluidb() {
  wipe fluidb; mkdir -p "$D/fluidb"
  nohup taskset -c 0-7 "$HTAPD" --root "$D/fluidb" --listen 127.0.0.1:3307 --max-connections 256 \
    --disable-compression > "$D/htapd.log" 2>&1 &
  echo $! > "$D/htapd.pid"
  sleep 2
}

stop_fluidb() { kill "$(cat "$D/htapd.pid")" 2>/dev/null; sleep 2; wipe fluidb; }

start_pg() {
  docker rm -f bench-pg >/dev/null 2>&1; wipe pg
  docker run -d --name bench-pg --network host --cpuset-cpus 0-7 --memory 16g --shm-size 4g \
    -e POSTGRES_PASSWORD=bench -v "$D/pg":/var/lib/postgresql/data postgres:17 \
    -c shared_buffers=4GB -c effective_cache_size=12GB -c work_mem=64MB -c maintenance_work_mem=1GB \
    -c max_connections=300 -c max_parallel_workers_per_gather=4 -c max_wal_size=8GB >/dev/null
  until docker exec bench-pg pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1; do sleep 1; done; sleep 2
}

start_ch() {
  docker rm -f bench-ch >/dev/null 2>&1; wipe ch
  docker run -d --name bench-ch --network host --cpuset-cpus 0-7 --memory 16g --ulimit nofile=262144:262144 \
    -e CLICKHOUSE_PASSWORD=bench -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1 \
    -v "$D/ch":/var/lib/clickhouse clickhouse/clickhouse-server:25.8 >/dev/null
  until docker exec bench-ch clickhouse-client --password bench -q 'SELECT 1' >/dev/null 2>&1; do sleep 1; done
}

mem() {
  local db=$1 phase=$2 b
  if [ "$db" = fluidb ]; then
    b=$(awk '/VmHWM/ {print $2*1024}' /proc/"$(cat "$D/htapd.pid")"/status)
  else
    local name=bench-pg; [ "$db" = clickhouse ] && name=bench-ch
    b=$(cat /sys/fs/cgroup/system.slice/docker-"$(docker inspect -f '{{.Id}}' $name)".scope/memory.peak 2>/dev/null || echo null)
  fi
  echo "{\"kind\":\"mem\",\"db\":\"$db\",\"rows\":$rows,\"phase\":\"$phase\",\"peak_bytes\":$b}" >> "$S/results.jsonl"
}

for db in fluidb postgres clickhouse; do
  echo "=== $db $rows $(date +%T) data=$D"
  case $db in
    fluidb) docker rm -f bench-pg bench-ch >/dev/null 2>&1; start_fluidb ;;
    postgres) start_pg ;;
    clickhouse) start_ch ;;
  esac
  client load "$db" "$rows"
  if [ "$db" = fluidb ]; then
    echo "{\"kind\":\"size\",\"db\":\"fluidb\",\"rows\":$rows,\"bytes\":$(du -sb "$D/fluidb" | cut -f1)}" >> "$S/results.jsonl"
  else
    client size "$db"
  fi
  mem "$db" load
  client olap "$db" "$rows"
  mem "$db" olap
  client oltp "$db" "$rows" 1,8,32
  client mixed "$db" "$rows"
  mem "$db" all
  case $db in
    fluidb) stop_fluidb ;;
    postgres) docker rm -f bench-pg >/dev/null; wipe pg ;;
    clickhouse) docker rm -f bench-ch >/dev/null; wipe ch ;;
  esac
done
echo "=== scale $rows done $(date +%T)"
