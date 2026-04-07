#!/usr/bin/env bash
#
# compare_bench.sh -- 3-way S3 benchmark: AbixIO vs RustFS vs MinIO
#
# Usage:
#   ABIXIO_BIN=./target/release/abixio RUSTFS_BIN=rustfs MINIO_BIN=minio bash tests/compare_bench.sh
#
# Any server binary can be omitted -- that column shows "skip".
# All servers run single-node, single-disk, same NTFS volume, no EC.

set -uo pipefail

ABIXIO_BIN="${ABIXIO_BIN:-}"
RUSTFS_BIN="${RUSTFS_BIN:-}"
MINIO_BIN="${MINIO_BIN:-}"
MC="${MC:-mc}"
ITERS="${ITERS:-5}"
SIZES="${SIZES:-1024 1048576 10485760 104857600}"  # 1KB 1MB 10MB 100MB

ABIXIO_PORT=11000
RUSTFS_PORT=11001
RUSTFS_CONSOLE_PORT=11002
MINIO_PORT=11003
MINIO_CONSOLE_PORT=11004

# -- state --
TMPDIR_BASE=""
ABIXIO_PID=""
RUSTFS_PID=""
MINIO_PID=""
HAS_ABIXIO=false
HAS_RUSTFS=false
HAS_MINIO=false

# server list: name, alias, active flag
SERVERS=()

cleanup() {
    set +e
    [[ -n "$ABIXIO_PID" ]] && kill "$ABIXIO_PID" 2>/dev/null && wait "$ABIXIO_PID" 2>/dev/null
    [[ -n "$RUSTFS_PID" ]] && kill "$RUSTFS_PID" 2>/dev/null && wait "$RUSTFS_PID" 2>/dev/null
    [[ -n "$MINIO_PID" ]]  && kill "$MINIO_PID"  2>/dev/null && wait "$MINIO_PID"  2>/dev/null
    [[ -n "$TMPDIR_BASE" ]] && rm -rf "$TMPDIR_BASE"
    $MC alias rm abixio-bench 2>/dev/null || true
    $MC alias rm rustfs-bench 2>/dev/null || true
    $MC alias rm minio-bench  2>/dev/null || true
}
trap cleanup EXIT

# -- check mc --
if ! command -v "$MC" &>/dev/null && [[ ! -x "$MC" ]]; then
    echo "ERROR: mc (MinIO client) not found" >&2
    exit 1
fi

# -- detect available servers --
bin_available() {
    local bin=$1
    [[ -n "$bin" ]] && { command -v "$bin" &>/dev/null || [[ -x "$bin" ]] || [[ -f "$bin" ]]; }
}

if bin_available "$ABIXIO_BIN"; then HAS_ABIXIO=true; fi
if bin_available "$RUSTFS_BIN"; then HAS_RUSTFS=true; fi
if bin_available "$MINIO_BIN";  then HAS_MINIO=true;  fi

if ! $HAS_ABIXIO && ! $HAS_RUSTFS && ! $HAS_MINIO; then
    echo "ERROR: no server binaries found. set ABIXIO_BIN, RUSTFS_BIN, or MINIO_BIN" >&2
    exit 1
fi

echo "servers: abixio=$HAS_ABIXIO rustfs=$HAS_RUSTFS minio=$HAS_MINIO"

# -- temp dirs (all on same NTFS volume) --
TMPDIR_BASE=$(mktemp -d)
ABIXIO_DATA="$TMPDIR_BASE/abixio"
RUSTFS_DATA="$TMPDIR_BASE/rustfs"
MINIO_DATA="$TMPDIR_BASE/minio"
TESTDATA="$TMPDIR_BASE/testdata"
mkdir -p "$ABIXIO_DATA" "$RUSTFS_DATA" "$MINIO_DATA" "$TESTDATA"

# -- generate test files --
for sz in $SIZES; do
    if [[ $sz -ge 1048576 ]]; then
        label="$((sz / 1048576))MB"
    else
        label="$((sz / 1024))KB"
    fi
    dd if=/dev/urandom of="$TESTDATA/$label" bs=$sz count=1 2>/dev/null
done

# -- wait for port --
wait_for_port() {
    local port=$1 name=$2 tries=30
    while ! bash -c "echo >/dev/tcp/127.0.0.1/$port" 2>/dev/null; do
        tries=$((tries - 1))
        if [[ $tries -le 0 ]]; then
            echo "ERROR: $name did not start on port $port" >&2
            exit 1
        fi
        sleep 0.2
    done
    echo "$name ready on :$port"
}

# -- start servers --
if $HAS_ABIXIO; then
    echo "starting abixio on :$ABIXIO_PORT ..."
    ABIXIO_ACCESS_KEY=benchuser ABIXIO_SECRET_KEY=benchpass \
        "$ABIXIO_BIN" --volumes "$ABIXIO_DATA" --listen ":$ABIXIO_PORT" \
        >/dev/null 2>&1 &
    ABIXIO_PID=$!
fi

if $HAS_RUSTFS; then
    echo "starting rustfs on :$RUSTFS_PORT ..."
    RUSTFS_ROOT_USER=benchuser RUSTFS_ROOT_PASSWORD=benchpass \
        "$RUSTFS_BIN" server "$RUSTFS_DATA" \
        --address ":$RUSTFS_PORT" --console-address ":$RUSTFS_CONSOLE_PORT" \
        >/dev/null 2>&1 &
    RUSTFS_PID=$!
fi

if $HAS_MINIO; then
    echo "starting minio on :$MINIO_PORT ..."
    MINIO_ROOT_USER=benchuser MINIO_ROOT_PASSWORD=benchpass \
        "$MINIO_BIN" server "$MINIO_DATA" \
        --address ":$MINIO_PORT" --console-address ":$MINIO_CONSOLE_PORT" \
        >/dev/null 2>&1 &
    MINIO_PID=$!
fi

# -- wait for servers --
if $HAS_ABIXIO; then wait_for_port $ABIXIO_PORT "abixio"; fi
if $HAS_RUSTFS; then wait_for_port $RUSTFS_PORT "rustfs"; fi
if $HAS_MINIO;  then wait_for_port $MINIO_PORT  "minio";  fi

# -- configure mc aliases + create buckets --
if $HAS_ABIXIO; then
    $MC alias set abixio-bench http://127.0.0.1:$ABIXIO_PORT benchuser benchpass --api S3v4 >/dev/null
    $MC mb abixio-bench/bench >/dev/null 2>&1
fi
if $HAS_RUSTFS; then
    $MC alias set rustfs-bench http://127.0.0.1:$RUSTFS_PORT benchuser benchpass --api S3v4 >/dev/null
    $MC mb rustfs-bench/bench >/dev/null 2>&1
fi
if $HAS_MINIO; then
    $MC alias set minio-bench http://127.0.0.1:$MINIO_PORT benchuser benchpass --api S3v4 >/dev/null
    $MC mb minio-bench/bench >/dev/null 2>&1
fi

# -- benchmark helpers --
human_size() {
    local sz=$1
    if [[ $sz -ge 1048576 ]]; then echo "$((sz / 1048576))MB"
    else echo "$((sz / 1024))KB"; fi
}

bench_put() {
    local alias=$1 file=$2 iters=$3
    local total_ms=0
    for i in $(seq 1 "$iters"); do
        local start_ns=$(date +%s%N)
        $MC cp "$file" "$alias/bench/obj_${i}" >/dev/null 2>&1
        local end_ns=$(date +%s%N)
        total_ms=$(( total_ms + (end_ns - start_ns) / 1000000 ))
    done
    echo "$total_ms"
}

bench_get() {
    local alias=$1 iters=$2
    local total_ms=0
    local sink="$TMPDIR_BASE/sink"
    for i in $(seq 1 "$iters"); do
        local start_ns=$(date +%s%N)
        $MC cp "$alias/bench/obj_${i}" "$sink" >/dev/null 2>&1
        local end_ns=$(date +%s%N)
        total_ms=$(( total_ms + (end_ns - start_ns) / 1000000 ))
        rm -f "$sink"
    done
    echo "$total_ms"
}

bench_head() {
    local alias=$1 iters=$2
    local total_ms=0
    for i in $(seq 1 "$iters"); do
        local start_ns=$(date +%s%N)
        $MC stat "$alias/bench/obj_1" >/dev/null 2>&1
        local end_ns=$(date +%s%N)
        total_ms=$(( total_ms + (end_ns - start_ns) / 1000000 ))
    done
    echo "$total_ms"
}

bench_list() {
    local alias=$1 iters=$2
    local total_ms=0
    for i in $(seq 1 "$iters"); do
        local start_ns=$(date +%s%N)
        $MC ls "$alias/bench/" >/dev/null 2>&1
        local end_ns=$(date +%s%N)
        total_ms=$(( total_ms + (end_ns - start_ns) / 1000000 ))
    done
    echo "$total_ms"
}

bench_delete() {
    local alias=$1 iters=$2
    local total_ms=0
    for i in $(seq 1 "$iters"); do
        local start_ns=$(date +%s%N)
        $MC rm "$alias/bench/obj_${i}" >/dev/null 2>&1
        local end_ns=$(date +%s%N)
        total_ms=$(( total_ms + (end_ns - start_ns) / 1000000 ))
    done
    echo "$total_ms"
}

calc_mbps() {
    local size_bytes=$1 total_ms=$2 iters=$3
    if [[ $total_ms -eq 0 ]]; then echo "n/a"; return; fi
    echo "$size_bytes $total_ms $iters" | awk '{printf "%.1f", ($1 * $3 * 1000) / ($2 * 1048576)}'
}

calc_avg_ms() {
    local total_ms=$1 iters=$2
    echo "$total_ms $iters" | awk '{printf "%.1f", $1/$2}'
}

# format: "123.4 MB/s (56ms)" or "skip"
fmt_tp() {
    local has=$1 size_bytes=$2 total_ms=$3 iters=$4
    if ! $has; then echo "skip"; return; fi
    local mbps=$(calc_mbps "$size_bytes" "$total_ms" "$iters")
    local avg=$(calc_avg_ms "$total_ms" "$iters")
    echo "${mbps} MB/s (${avg}ms)"
}

fmt_lat() {
    local has=$1 total_ms=$2 iters=$3
    if ! $has; then echo "skip"; return; fi
    local avg=$(calc_avg_ms "$total_ms" "$iters")
    echo "${avg}ms avg"
}

# -- verify PUT/GET actually works for each server at each size --
verify_roundtrip() {
    local alias=$1 file=$2 label=$3
    local sink="$TMPDIR_BASE/verify_sink"
    $MC cp "$file" "$alias/bench/verify_obj" >/dev/null 2>&1
    if ! $MC cp "$alias/bench/verify_obj" "$sink" >/dev/null 2>&1; then
        echo "WARN: $alias failed GET for $label -- skipping this server for this size" >&2
        rm -f "$sink"
        $MC rm "$alias/bench/verify_obj" >/dev/null 2>&1 || true
        return 1
    fi
    local orig_size=$(wc -c < "$file" | tr -d ' ')
    local got_size=$(wc -c < "$sink" | tr -d ' ')
    rm -f "$sink"
    $MC rm "$alias/bench/verify_obj" >/dev/null 2>&1 || true
    if [[ "$orig_size" != "$got_size" ]]; then
        echo "WARN: $alias size mismatch for $label (put $orig_size, got $got_size) -- skipping" >&2
        return 1
    fi
    return 0
}

# -- warmup --
echo ""
echo "warming up ..."
for alias in abixio-bench rustfs-bench minio-bench; do
    $MC cp "$TESTDATA/1KB" "$alias/bench/warmup" >/dev/null 2>&1 || true
    $MC rm "$alias/bench/warmup" >/dev/null 2>&1 || true
done

# -- run benchmarks --
echo ""
echo "running benchmarks ($ITERS iterations each) ..."
echo ""

COL="%-20s"
printf "| %-10s | %-6s | $COL | $COL | $COL |\n" "Operation" "Size" "AbixIO" "RustFS" "MinIO"
printf "|%-12s|%-8s|%-22s|%-22s|%-22s|\n" "------------" "--------" "----------------------" "----------------------" "----------------------"

for sz in $SIZES; do
    label=$(human_size "$sz")
    file="$TESTDATA/$label"

    # verify round-trip before benchmarking this size
    can_abixio=$HAS_ABIXIO; can_rustfs=$HAS_RUSTFS; can_minio=$HAS_MINIO
    if $HAS_ABIXIO && ! verify_roundtrip "abixio-bench" "$file" "$label"; then can_abixio=false; fi
    if $HAS_RUSTFS && ! verify_roundtrip "rustfs-bench" "$file" "$label"; then can_rustfs=false; fi
    if $HAS_MINIO  && ! verify_roundtrip "minio-bench"  "$file" "$label"; then can_minio=false;  fi

    # PUT
    a_ms=0; r_ms=0; m_ms=0
    if $can_abixio; then a_ms=$(bench_put "abixio-bench" "$file" "$ITERS"); fi
    if $can_rustfs; then r_ms=$(bench_put "rustfs-bench" "$file" "$ITERS"); fi
    if $can_minio;  then m_ms=$(bench_put "minio-bench"  "$file" "$ITERS"); fi
    printf "| %-10s | %-6s | $COL | $COL | $COL |\n" \
        "PUT" "$label" \
        "$(fmt_tp $can_abixio "$sz" "$a_ms" "$ITERS")" \
        "$(fmt_tp $can_rustfs "$sz" "$r_ms" "$ITERS")" \
        "$(fmt_tp $can_minio  "$sz" "$m_ms" "$ITERS")"

    # GET (reads back the objects written by PUT)
    a_ms=0; r_ms=0; m_ms=0
    if $can_abixio; then a_ms=$(bench_get "abixio-bench" "$ITERS"); fi
    if $can_rustfs; then r_ms=$(bench_get "rustfs-bench" "$ITERS"); fi
    if $can_minio;  then m_ms=$(bench_get "minio-bench"  "$ITERS"); fi
    printf "| %-10s | %-6s | $COL | $COL | $COL |\n" \
        "GET" "$label" \
        "$(fmt_tp $can_abixio "$sz" "$a_ms" "$ITERS")" \
        "$(fmt_tp $can_rustfs "$sz" "$r_ms" "$ITERS")" \
        "$(fmt_tp $can_minio  "$sz" "$m_ms" "$ITERS")"

    # cleanup for next size
    for alias in abixio-bench rustfs-bench minio-bench; do
        for i in $(seq 1 "$ITERS"); do
            $MC rm "$alias/bench/obj_${i}" >/dev/null 2>&1 || true
        done
    done
done

# HEAD (use 1MB object)
if $HAS_ABIXIO; then $MC cp "$TESTDATA/1MB" "abixio-bench/bench/obj_1" >/dev/null 2>&1 || true; fi
if $HAS_RUSTFS; then $MC cp "$TESTDATA/1MB" "rustfs-bench/bench/obj_1" >/dev/null 2>&1 || true; fi
if $HAS_MINIO;  then $MC cp "$TESTDATA/1MB" "minio-bench/bench/obj_1"  >/dev/null 2>&1 || true; fi
a_ms=0; r_ms=0; m_ms=0
if $HAS_ABIXIO; then a_ms=$(bench_head "abixio-bench" "$ITERS"); fi
if $HAS_RUSTFS; then r_ms=$(bench_head "rustfs-bench" "$ITERS"); fi
if $HAS_MINIO;  then m_ms=$(bench_head "minio-bench"  "$ITERS"); fi
printf "| %-10s | %-6s | $COL | $COL | $COL |\n" \
    "HEAD" "1MB" \
    "$(fmt_lat $HAS_ABIXIO "$a_ms" "$ITERS")" \
    "$(fmt_lat $HAS_RUSTFS "$r_ms" "$ITERS")" \
    "$(fmt_lat $HAS_MINIO  "$m_ms" "$ITERS")"

# LIST (populate 100 objects)
echo ""
echo "populating 100 objects for LIST benchmark ..."
for alias in abixio-bench rustfs-bench minio-bench; do
    # skip aliases for servers not running
    $MC ls "$alias/" >/dev/null 2>&1 || continue
    for i in $(seq 1 100); do
        $MC cp "$TESTDATA/1KB" "$alias/bench/list_${i}" >/dev/null 2>&1 || true
    done
done
a_ms=0; r_ms=0; m_ms=0
if $HAS_ABIXIO; then a_ms=$(bench_list "abixio-bench" "$ITERS"); fi
if $HAS_RUSTFS; then r_ms=$(bench_list "rustfs-bench" "$ITERS"); fi
if $HAS_MINIO;  then m_ms=$(bench_list "minio-bench"  "$ITERS"); fi
printf "| %-10s | %-6s | $COL | $COL | $COL |\n" \
    "LIST" "100obj" \
    "$(fmt_lat $HAS_ABIXIO "$a_ms" "$ITERS")" \
    "$(fmt_lat $HAS_RUSTFS "$r_ms" "$ITERS")" \
    "$(fmt_lat $HAS_MINIO  "$m_ms" "$ITERS")"

# DELETE
a_ms=0; r_ms=0; m_ms=0
if $HAS_ABIXIO; then a_ms=$(bench_delete "abixio-bench" "$ITERS"); fi
if $HAS_RUSTFS; then r_ms=$(bench_delete "rustfs-bench" "$ITERS"); fi
if $HAS_MINIO;  then m_ms=$(bench_delete "minio-bench"  "$ITERS"); fi
printf "| %-10s | %-6s | $COL | $COL | $COL |\n" \
    "DELETE" "1obj" \
    "$(fmt_lat $HAS_ABIXIO "$a_ms" "$ITERS")" \
    "$(fmt_lat $HAS_RUSTFS "$r_ms" "$ITERS")" \
    "$(fmt_lat $HAS_MINIO  "$m_ms" "$ITERS")"

echo ""
echo "done. servers stopping."
