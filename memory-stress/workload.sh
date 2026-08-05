# Workload definitions for the memory-limiter stress test.
#
# Sourced by run.sh. Provides:
#   prepare_datasets <mount_dir> <run_dir>
#   run_phases       <mount_dir> <run_dir>
#
# Phases are varied and overlapping so the limiter faces changing pressure
# over the run. Each phase runs for $PHASE_SECS. All file I/O goes through
# the real mount, so this exercises the FUSE + prefetch + upload paths the
# limiter governs.
#
# fio jobs are bounded by `runtime`+`time_based` and additionally hard-capped
# by a `timeout` wrapper, so a wedged mount cannot hang a phase past its
# budget: on timeout the phase is marked and the run proceeds to teardown.

# Directory under the mount holding the shared read pool and readdir tree.
DATASET_DIR_NAME="dataset"
READDIR_DIR_NAME="tree"

# --- fio helpers -----------------------------------------------------------

# Run fio with a hard timeout. Args: <phase_label> <run_dir> <fio-args...>
# The timeout is PHASE_SECS plus slack; if fio overruns (mount wedged), we
# SIGKILL it and record a stall marker rather than blocking forever.
run_fio() {
    local label="$1" run_dir="$2"; shift 2
    local hard=$(( PHASE_SECS + STALL_TIMEOUT_SECS ))
    local out="$run_dir/fio_${label}.json"
    timeout --signal=KILL "${hard}s" \
        fio --output-format=json --output="$out" \
            --eta=never "$@"
    local rc=$?
    if [[ $rc -eq 137 ]]; then
        echo "WARN: fio phase '$label' hit hard timeout (${hard}s) -- possible stall" \
            | tee -a "$run_dir/stalls.log"
    elif [[ $rc -ne 0 ]]; then
        echo "WARN: fio phase '$label' exited rc=$rc" | tee -a "$run_dir/stalls.log"
    fi
    return 0   # never abort the run on a single phase failure
}

# --- Dataset preparation ---------------------------------------------------

# Concurrency for dataset creation (uploads run in parallel through the mount).
PREPARE_JOBS="${PREPARE_JOBS:-16}"

# Run the shell function/command in $1... as bounded-concurrency background jobs.
# We track only the PIDs we spawn and wait on them explicitly -- a bare `wait`
# would also block on the long-lived monitor process (a sibling child of the
# same shell), hanging preparation forever. Reaps in FIFO order to cap the
# number of concurrent jobs at PREPARE_JOBS.
PREP_PIDS=()
prep_throttle() {
    if (( ${#PREP_PIDS[@]} >= PREPARE_JOBS )); then
        wait "${PREP_PIDS[0]}" 2>/dev/null
        PREP_PIDS=("${PREP_PIDS[@]:1}")
    fi
}
prep_wait_all() {
    [[ ${#PREP_PIDS[@]} -gt 0 ]] && wait "${PREP_PIDS[@]}" 2>/dev/null
    PREP_PIDS=()
}

prepare_datasets() {
    local mnt="$1" run_dir="$2"
    local ds="$mnt/$DATASET_DIR_NAME"
    local tree="$mnt/$READDIR_DIR_NAME"
    log_phase "prepare_datasets"

    # Read pool: READ_POOL_COUNT objects of READ_OBJECT_SIZE_MIB each. Created
    # once and reused; skip any that already exist at the right size so reruns
    # are cheap. Created concurrently (bounded to PREPARE_JOBS) so preparing a
    # multi-GiB pool doesn't dominate the run.
    mkdir -p "$ds"
    local bytes=$(( READ_OBJECT_SIZE_MIB * 1024 * 1024 ))
    echo "preparing read pool: $READ_POOL_COUNT x ${READ_OBJECT_SIZE_MIB} MiB (jobs=$PREPARE_JOBS)"
    PREP_PIDS=()
    for i in $(seq 0 $((READ_POOL_COUNT - 1))); do
        local f="$ds/read_${i}.bin"
        if [[ "$(stat -c %s "$f" 2>/dev/null || echo 0)" == "$bytes" ]]; then
            continue
        fi
        dd if=/dev/zero of="$f" bs=8M count=$(( READ_OBJECT_SIZE_MIB / 8 )) \
            conv=fsync status=none 2>>"$run_dir/prepare.log" \
            || echo "WARN: failed to create $f" | tee -a "$run_dir/stalls.log" &
        PREP_PIDS+=($!)
        prep_throttle
    done
    prep_wait_all
    echo "read pool ready ($(ls "$ds"/read_*.bin 2>/dev/null | wc -l) objects)"

    # readdir tree: wide/deep tree of small files to exercise metadata memory.
    # The ready-marker encodes the tree dimensions, so a run with different
    # dimensions regenerates the tree rather than reusing a stale one left by a
    # previous run under the same prefix. Each directory is populated by its own
    # background job (bounded to PREPARE_JOBS) since many tiny uploads are
    # latency-bound.
    local marker="$tree/.ready_${READDIR_DIRS}x${READDIR_FILES_PER_DIR}"
    echo "preparing readdir tree: $READDIR_DIRS dirs x $READDIR_FILES_PER_DIR files (jobs=$PREPARE_JOBS)"
    if [[ ! -f "$marker" ]]; then
        PREP_PIDS=()
        for d in $(seq 0 $((READDIR_DIRS - 1))); do
            (
                dir="$tree/dir_${d}"
                mkdir -p "$dir"
                for fdx in $(seq 0 $((READDIR_FILES_PER_DIR - 1))); do
                    head -c "$READDIR_FILE_SIZE_BYTES" /dev/zero > "$dir/file_${fdx}" 2>/dev/null
                done
            ) &
            PREP_PIDS+=($!)
            prep_throttle
        done
        prep_wait_all
        touch "$marker" 2>/dev/null
    fi
    echo "datasets ready"
}

# --- Phases ----------------------------------------------------------------

# A background readdir/stat loop that keeps listing the tree until a sentinel
# file is removed. Used to overlay metadata pressure on other phases. Sets
# READDIR_LOOP_PID to the backgrounded loop so the caller can wait on it (we
# background directly here rather than echoing a PID from a command
# substitution, whose subshell PID would not be a child of the caller).
READDIR_LOOP_PID=""
start_readdir_loop() {
    local tree="$1" sentinel="$2"
    (
        while [[ -f "$sentinel" ]]; do
            ls -R "$tree" > /dev/null 2>&1
            find "$tree" -type f -exec stat {} + > /dev/null 2>&1
        done
    ) &
    READDIR_LOOP_PID=$!
}

# A background pool of sequential writers that run until a sentinel is removed.
# Mountpoint only supports sequential writes (no backward seek), so each writer
# writes a *fresh* file end-to-end with dd, syncs, deletes it, and repeats --
# real upload traffic without ever seeking backward on an open handle. Writes
# concurrent uploads that pressure the upload side of the buffer budget.
#
# Args: <write_dir> <sentinel> <n_writers> <object_size_mib> <part_size_bytes>
# Appends each writer's PID to the WRITER_PIDS array. The caller removes the
# sentinel to stop the writers, then calls wait_writers to reap them (we wait
# on specific PIDs rather than a bare `wait`, which would also block on the
# long-lived monitor process).
WRITER_PIDS=()
start_writer_loop() {
    local wdir="$1" sentinel="$2" n="$3" size_mib="$4" part="$5"
    local blocks=$(( size_mib / 8 ))
    WRITER_PIDS=()
    for w in $(seq 0 $((n - 1))); do
        (
            local iter=0
            while [[ -f "$sentinel" ]]; do
                local f="$wdir/w_${w}_${iter}.bin"
                dd if=/dev/zero of="$f" bs=8M count="$blocks" conv=fsync \
                    status=none 2>/dev/null || true
                rm -f "$f" 2>/dev/null
                iter=$((iter + 1))
            done
        ) &
        WRITER_PIDS+=($!)
    done
}

wait_writers() {
    [[ ${#WRITER_PIDS[@]} -gt 0 ]] && wait "${WRITER_PIDS[@]}" 2>/dev/null
    WRITER_PIDS=()
}

run_phases() {
    local mnt="$1" run_dir="$2"
    local ds="$mnt/$DATASET_DIR_NAME"
    local tree="$mnt/$READDIR_DIR_NAME"
    local write_dir="$mnt/writes"
    mkdir -p "$write_dir"

    # Each fio reader job N opens read_N.bin (via filename_format='read_$jobnum.bin'),
    # so READ_POOL_COUNT must be >= the largest per-phase job count.

    # P1: sequential reads -- many parallel large sequential streams.
    log_phase "P1_sequential_reads"
    run_fio "P1_seq" "$run_dir" \
        --name=seqread --directory="$ds" \
        --rw=read --bs="$READ_PART_SIZE" --size="${READ_OBJECT_SIZE_MIB}Mi" \
        --numjobs="$SEQ_READERS" --nrfiles=1 \
        --filename_format='read_$jobnum.bin' \
        --ioengine=psync --thread --group_reporting \
        --time_based --runtime="${PHASE_SECS}s" --ramp_time=0

    # P2: sequential reads + concurrent writers.
    log_phase "P2_reads_plus_writes"
    local sentinel2="$run_dir/.p2"
    touch "$sentinel2"
    start_writer_loop "$write_dir" "$sentinel2" "$WRITERS" "$READ_OBJECT_SIZE_MIB" "$WRITE_PART_SIZE"
    run_fio "P2_seq" "$run_dir" \
        --name=seqread --directory="$ds" \
        --rw=read --bs="$READ_PART_SIZE" --size="${READ_OBJECT_SIZE_MIB}Mi" \
        --numjobs="$SEQ_READERS" --nrfiles=1 \
        --filename_format='read_$jobnum.bin' \
        --ioengine=psync --thread --group_reporting \
        --time_based --runtime="${PHASE_SECS}s" --ramp_time=0
    rm -f "$sentinel2"
    wait_writers
    rm -f "$write_dir"/w_*.bin 2>/dev/null

    # P3: random reads across the whole pool -- stresses prefetch churn.
    log_phase "P3_random_reads"
    run_fio "P3_rand" "$run_dir" \
        --name=randread --directory="$ds" \
        --rw=randread --bs="$READ_PART_SIZE" --size="${READ_OBJECT_SIZE_MIB}Mi" \
        --numjobs="$RAND_READERS" --nrfiles=1 \
        --filename_format='read_$jobnum.bin' \
        --ioengine=psync --thread --group_reporting \
        --time_based --runtime="${PHASE_SECS}s" --ramp_time=0

    # P4: slow reads -- rate-limited so each reader holds buffers open longer,
    # keeping many read windows resident simultaneously.
    log_phase "P4_slow_reads"
    run_fio "P4_slow" "$run_dir" \
        --name=slowread --directory="$ds" \
        --rw=read --bs="$READ_PART_SIZE" --size="${READ_OBJECT_SIZE_MIB}Mi" \
        --rate="$SLOW_READ_RATE" \
        --numjobs="$SLOW_READERS" --nrfiles=1 \
        --filename_format='read_$jobnum.bin' \
        --ioengine=psync --thread --group_reporting \
        --time_based --runtime="${PHASE_SECS}s" --ramp_time=0

    # P5: readdir + metadata pressure.
    log_phase "P5_readdir"
    local sentinel="$run_dir/.p5"
    touch "$sentinel"
    start_readdir_loop "$tree" "$sentinel"
    local loop_pid=$READDIR_LOOP_PID
    sleep "$PHASE_SECS"
    rm -f "$sentinel"
    wait "$loop_pid" 2>/dev/null

    # P6: everything at once -- seq reads + writes + random reads + readdir.
    log_phase "P6_all_workloads"
    local sentinel6="$run_dir/.p6"
    touch "$sentinel6"
    start_readdir_loop "$tree" "$sentinel6"
    local loop6=$READDIR_LOOP_PID
    start_writer_loop "$write_dir" "$sentinel6" "$WRITERS" "$READ_OBJECT_SIZE_MIB" "$WRITE_PART_SIZE"
    local writer_pids6=("${WRITER_PIDS[@]}")
    ( run_fio "P6_seq" "$run_dir" \
        --name=seqread --directory="$ds" --rw=read --bs="$READ_PART_SIZE" \
        --size="${READ_OBJECT_SIZE_MIB}Mi" --numjobs="$SEQ_READERS" --nrfiles=1 \
        --filename_format='read_$jobnum.bin' --ioengine=psync --thread \
        --group_reporting --time_based --runtime="${PHASE_SECS}s" --ramp_time=0 ) &
    local s6=$!
    ( run_fio "P6_rand" "$run_dir" \
        --name=randread --directory="$ds" --rw=randread --bs="$READ_PART_SIZE" \
        --size="${READ_OBJECT_SIZE_MIB}Mi" --numjobs="$RAND_READERS" --nrfiles=1 \
        --filename_format='read_$jobnum.bin' --ioengine=psync --thread \
        --group_reporting --time_based --runtime="${PHASE_SECS}s" --ramp_time=0 ) &
    local r6=$!
    wait $s6 $r6 2>/dev/null
    rm -f "$sentinel6"
    wait "${writer_pids6[@]}" 2>/dev/null
    wait "$loop6" 2>/dev/null
    rm -f "$write_dir"/w_*.bin 2>/dev/null
}
