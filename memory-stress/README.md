# Memory-limiter stress test

A long-running, reproducible stress test that checks whether mountpoint-s3
keeps its memory usage under the configured `--memory-target` under a
varied, sustained, real-filesystem workload.

Unlike the in-process Rust stress harness (`mountpoint-s3-fs/tests/stress/`,
which asserts peak-memory invariants in CI), this test drives the **real
`mount-s3` binary out-of-process** so the sampled RSS is the exact process
the limiter governs, uncontaminated by a test driver's own buffers, and
produces a **time series** of RSS and mountpoint's pool gauges suitable for
graphing over an hour-plus run.

## What it does

1. Mounts `mount-s3` against a real bucket with a configured
   `--memory-target` and `--log-metrics`.
2. Runs six workload phases (each `PHASE_SECS` long) that come and go and
   overlap: sequential reads; reads + writes; random reads; slow
   (rate-limited) reads; readdir/metadata; then everything at once. Real
   filesystem operations via `fio` and `dd`/`ls` loops.
3. Samples every second into `metrics.csv`:
   - `rss_bytes` from `/proc/<pid>/status` (never touches the mount, so it
     keeps sampling even if the mount deadlocks),
   - `pool.bytes_in_use`, `pool.allocated_bytes`, `mem.bytes_reserved`
     parsed from the mount's metrics log.
4. Tears down cleanly, force-unmounting (lazy unmount, then FUSE-abort) if
   the mount has wedged, so a deadlock never hangs the test past its budget.
5. `plot.py` + `report.py` turn a run directory into `REPORT.md` with graphs.

## Requirements

- A built `mount-s3` binary (release recommended). Set `MOUNT_S3_BIN` or let
  `run.sh` find it under `../target/`.
- `fio`, `python3`, and AWS credentials with access to the bucket.
- For plotting: `matplotlib` (use `uv run --with matplotlib ...`).

## Usage

```bash
# 1. Establish the unconstrained baseline (prove the workload exceeds the limit):
MEMORY_TARGET_MIB=32768 ./run.sh config.env baseline   # or edit config.env

# 2. Constrained run (the real test), >= 1 hour:
./run.sh config.env constrained

# 3. Plot + report (needs matplotlib):
uv run --with matplotlib plot.py   runs/<run-dir>
uv run --with matplotlib report.py runs/<run-dir>
```

Every knob lives in `config.env`; a run is fully determined by that file
plus the mountpoint-s3 commit. To reproduce a breaking run, keep its
`config.env` and re-invoke `run.sh`.

## Files

| File | Purpose |
|---|---|
| `config.env` | All parameters. The reproducible unit. |
| `run.sh` | Orchestrates mount, sampling, phases, teardown. |
| `workload.sh` | Dataset prep + the phase sequence (sourced by `run.sh`). |
| `monitor.py` | Samples RSS + pool gauges into `metrics.csv`. |
| `plot.py` | RSS and pool graphs from a run directory. |
| `report.py` | Assembles `REPORT.md` with graphs and peak tables. |
| `PLAN.md` | Test/measurement/report plan and rationale. |
| `RESULTS.md` | Findings from the baseline + full 1-hour constrained run. |
| `runs/<run>/REPORT.md` | Auto-generated per-run report with graphs. |

## Interpreting the results

The limiter splits `--memory-target` into a **data-buffer budget**
(`mem_limit - max(mem_limit/8, 128 MiB)`) that hard-bounds the pool
(`pool.bytes_in_use`, `pool.allocated_bytes`), plus an overhead reserve for
metadata and internal structures that the pool does **not** track. So:

- `pool.bytes_in_use` staying under the data-buffer budget confirms the pool
  accounting works.
- **RSS** staying under `--memory-target` is the stronger, end-to-end
  property. If the pool is within budget but RSS is not, the gap is untracked
  memory (metadata, CRT, allocator fragmentation) and the overhead reserve
  was too small for the workload. The report calls this out explicitly.
