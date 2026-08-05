#!/usr/bin/env bash
#
# Memory-limiter stress test driver.
#
# Mounts mount-s3 against a real bucket with a configured --memory-target,
# runs a sequence of varied, overlapping real-filesystem workloads for at
# least an hour, samples RSS and mountpoint's own pool gauges throughout,
# then tears down cleanly (force-unmounting if the mount has wedged) and
# emits a run directory that plot.py / report.py turn into a report.
#
# The run is fully determined by config.env plus the mountpoint-s3 commit,
# so a breaking configuration can be re-run verbatim.
#
# Usage:
#   ./run.sh [config.env] [run-label]
#
# Environment overrides (take precedence over config.env):
#   MOUNT_S3_BIN   path to the mount-s3 binary (default: search target/)
#   OUT_ROOT       parent dir for run output (default: ./runs)

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONFIG="${1:-$HERE/config.env}"
RUN_LABEL="${2:-run}"

# shellcheck disable=SC1090
source "$CONFIG"

# --- Locate the mount-s3 binary --------------------------------------------
REPO_ROOT="$(cd "$HERE/.." && pwd)"
if [[ -z "${MOUNT_S3_BIN:-}" ]]; then
    for cand in \
        "$REPO_ROOT/target/release/mount-s3" \
        "$REPO_ROOT/target/debug/mount-s3"; do
        [[ -x "$cand" ]] && MOUNT_S3_BIN="$cand" && break
    done
fi
if [[ -z "${MOUNT_S3_BIN:-}" || ! -x "$MOUNT_S3_BIN" ]]; then
    echo "ERROR: mount-s3 binary not found. Build it or set MOUNT_S3_BIN." >&2
    exit 1
fi

command -v fio >/dev/null || { echo "ERROR: fio not installed" >&2; exit 1; }

# --- Run directory ---------------------------------------------------------
OUT_ROOT="${OUT_ROOT:-$HERE/runs}"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUN_DIR="$OUT_ROOT/${TS}-${RUN_LABEL}-${MEMORY_TARGET_MIB}MiB"
mkdir -p "$RUN_DIR"
MOUNT_DIR="$RUN_DIR/mnt"
LOG_DIR="$RUN_DIR/mp_logs"
PHASE_FILE="$RUN_DIR/phase"
METRICS_CSV="$RUN_DIR/metrics.csv"
mkdir -p "$MOUNT_DIR" "$LOG_DIR"
echo "startup" > "$PHASE_FILE"

echo "=== Memory-limiter stress test ==="
echo "run dir:       $RUN_DIR"
echo "binary:        $MOUNT_S3_BIN"
echo "memory target: ${MEMORY_TARGET_MIB} MiB"
echo "bucket/prefix: s3://$S3_BUCKET/$S3_PREFIX"

# --- Global hard deadline & cleanup ----------------------------------------
MOUNT_PID=""
MONITOR_PID=""

log_phase() { echo "$1" > "$PHASE_FILE"; echo "--- phase: $1 ($(date -u +%H:%M:%SZ)) ---"; }

force_unmount() {
    # Try graceful, then lazy, then FUSE-abort so a wedged mount never blocks us.
    fusermount -u "$MOUNT_DIR" 2>/dev/null && return
    fusermount -u -z "$MOUNT_DIR" 2>/dev/null
    # FUSE-abort: fail all in-flight ops with EIO to unblock stuck threads.
    local minor
    minor=$(awk -v mp="$MOUNT_DIR" '$5==mp {split($3,a,":"); print a[2]}' /proc/self/mountinfo 2>/dev/null | head -1)
    if [[ -n "$minor" && -w "/sys/fs/fuse/connections/$minor/abort" ]]; then
        echo 1 > "/sys/fs/fuse/connections/$minor/abort" 2>/dev/null
    fi
}

cleanup() {
    log_phase "teardown"
    [[ -n "$MONITOR_PID" ]] && kill "$MONITOR_PID" 2>/dev/null
    # Kill any lingering fio.
    pkill -P $$ fio 2>/dev/null
    pkill -f "fio .*$MOUNT_DIR" 2>/dev/null
    if [[ -n "$MOUNT_PID" ]] && kill -0 "$MOUNT_PID" 2>/dev/null; then
        force_unmount
        # Give mount-s3 a moment to exit after unmount.
        for _ in $(seq 1 50); do kill -0 "$MOUNT_PID" 2>/dev/null || break; sleep 0.1; done
        kill -0 "$MOUNT_PID" 2>/dev/null && kill "$MOUNT_PID" 2>/dev/null
    fi
    [[ -n "$MONITOR_PID" ]] && wait "$MONITOR_PID" 2>/dev/null
    echo "=== teardown complete; run dir: $RUN_DIR ==="
}
trap cleanup EXIT INT TERM

# --- Mount -----------------------------------------------------------------
MOUNT_CMD=(
    "$MOUNT_S3_BIN" "$S3_BUCKET" "$MOUNT_DIR"
    --prefix "$S3_PREFIX"
    --memory-target "$MEMORY_TARGET_MIB"
    --read-part-size "$READ_PART_SIZE"
    --write-part-size "$WRITE_PART_SIZE"
    --max-threads "$MAX_THREADS"
    --allow-overwrite --allow-delete
    --log-metrics --log-directory "$LOG_DIR"
)
echo "${MOUNT_CMD[*]}" > "$RUN_DIR/mount_cmd.txt"

PID_FILE="$RUN_DIR/mount.pid"
UNSTABLE_MOUNTPOINT_PID_FILE="$PID_FILE" "${MOUNT_CMD[@]}"
if [[ ! -f "$PID_FILE" ]]; then
    echo "ERROR: mount-s3 did not write a pid file; mount failed." >&2
    exit 1
fi
MOUNT_PID="$(cat "$PID_FILE")"
echo "mount-s3 pid: $MOUNT_PID"

# Wait for the mount to be usable.
for _ in $(seq 1 50); do
    mountpoint -q "$MOUNT_DIR" && break
    sleep 0.2
done
mountpoint -q "$MOUNT_DIR" || { echo "ERROR: $MOUNT_DIR is not a mountpoint" >&2; exit 1; }

# --- Metadata for the report -----------------------------------------------
GIT_COMMIT="$(cd "$REPO_ROOT" && git rev-parse HEAD 2>/dev/null || echo unknown)"
DATA_BUDGET_MIB=$(( MEMORY_TARGET_MIB - (MEMORY_TARGET_MIB/8 < 128 ? 128 : MEMORY_TARGET_MIB/8) ))
cat > "$RUN_DIR/run_meta.json" <<EOF
{
  "run_label": "$RUN_LABEL",
  "timestamp": "$TS",
  "git_commit": "$GIT_COMMIT",
  "memory_target_mib": $MEMORY_TARGET_MIB,
  "data_buffer_budget_mib": $DATA_BUDGET_MIB,
  "read_part_size": $READ_PART_SIZE,
  "write_part_size": $WRITE_PART_SIZE,
  "max_threads": $MAX_THREADS,
  "read_pool_count": $READ_POOL_COUNT,
  "read_object_size_mib": $READ_OBJECT_SIZE_MIB,
  "readdir_dirs": $READDIR_DIRS,
  "readdir_files_per_dir": $READDIR_FILES_PER_DIR,
  "phase_secs": $PHASE_SECS,
  "seq_readers": $SEQ_READERS,
  "rand_readers": $RAND_READERS,
  "writers": $WRITERS,
  "slow_readers": $SLOW_READERS,
  "slow_read_rate": "$SLOW_READ_RATE",
  "host": "$(hostname)",
  "nproc": $(nproc),
  "mount_cmd": "${MOUNT_CMD[*]}"
}
EOF

# --- Start the sampler -----------------------------------------------------
# Pass a glob; monitor.py resolves the newest matching log once it appears, so
# the mount's random log-file suffix need not be known here.
MP_LOG_GLOB="$LOG_DIR/mountpoint-s3-*.log"
python3 "$HERE/monitor.py" \
    --pid "$MOUNT_PID" --log "$MP_LOG_GLOB" --out "$METRICS_CSV" \
    --interval "$SAMPLE_INTERVAL_SECS" --phase-file "$PHASE_FILE" \
    > "$RUN_DIR/monitor.out" 2>&1 &
MONITOR_PID=$!
echo "monitor pid:  $MONITOR_PID -> $METRICS_CSV"

# --- Datasets and workload -------------------------------------------------
# workload.sh owns dataset creation and the phase sequence so the driver stays
# focused on lifecycle. It is given the mount dir, run dir, and phase file.
source "$HERE/workload.sh"

prepare_datasets "$MOUNT_DIR" "$RUN_DIR"
run_phases "$MOUNT_DIR" "$RUN_DIR"

log_phase "done"
echo "=== workload complete ==="
# cleanup runs via trap
