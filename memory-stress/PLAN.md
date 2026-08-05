# Memory-limiter stress test — plan

## Goal

Determine whether the `feature/memory-limit` branch keeps mountpoint-s3's
memory usage under the configured `--memory-target` under a sustained,
varied, real-filesystem workload. Produce a reproducible test, a
time-series of memory usage, and a report answering: does the limiter hold,
and if not, under what conditions does it fail?

## What the limiter actually governs (from the code)

Read from `mountpoint-s3-fs/src/memory/limiter.rs` and `mountpoint-s3/src/cli.rs`:

- CLI flag: `--memory-target <MiB>` (min 512). Default: 95% of total system
  memory, min 512 MiB.
- The limiter splits the target into:
  - `additional_mem_reserved = max(mem_limit / 8, 128 MiB)` — headroom for
    metadata and internal structures, *not* tracked by the pool.
  - `data_buffer_budget = mem_limit - additional_mem_reserved` — the hard
    cap on pooled data buffers (prefetch/read + upload).
- The pool gauges `pool.allocated_bytes` and `pool.bytes_in_use` are
  hard-bounded by `data_buffer_budget`. The branch's own stress invariants
  (`tests/stress/harness/invariants.rs`) assert this.
- RSS (`process.memory_usage`) versus `mem_limit` is currently only logged,
  not asserted — the code carries a `TODO: promote from error to assert once
  production accounting is tight enough`. **This is the gap this test
  targets:** does real RSS stay under `--memory-target`, or does untracked
  allocation (metadata, CRT, fragmentation, glibc arenas) push it over?

Consequence for interpretation: we expect `pool.bytes_in_use` to stay under
`data_buffer_budget` by construction. The open question is total process
RSS. We therefore graph both and compare each against its own ceiling
(`data_buffer_budget` for the pool gauge, `mem_limit` for RSS).

## Approach: real out-of-process mount

We drive the **real `mount-s3` binary** out-of-process (as `benchmark/`
does), not the in-process Rust stress harness, because:

- The Rust harness runs mountpoint *in the test process*, so its RSS is
  contaminated by the workload driver's own buffers, and it only captures
  *peak* RSS for a pass/fail assertion — not a time series to graph.
- An out-of-process mount gives a clean, production-representative RSS
  signal for the exact process the limiter governs.

We sample RSS from `/proc/<pid>/status` (`VmRSS`), independent of FUSE, so
the sampler cannot block or deadlock even if the mount wedges. We parse
`pool.bytes_in_use` and the other pool gauges from the mount's
`--log-metrics` output.

The existing Rust stress harness remains the right tool for the pass/fail
CI invariant; this test is the complementary long-run, graphed,
out-of-process characterization the GOAL asks for.

## Test plan

### Memory limit

Moderate, well below unconstrained peak, well above the 512 MiB floor:
**`--memory-target 2048` (2 GiB)** as the primary limit. On a 123 GiB host
this is a realistic "constrained container" figure and is small enough that
the mixed workload's natural demand exceeds it. `data_buffer_budget` at
2 GiB = 2048 - max(256, 128) = **1792 MiB**.

We first run a **baseline** with `--memory-target` set high (e.g. 32 GiB,
effectively unconstrained for this workload) to establish the workload's
natural peak demand and prove the workload would exceed 2 GiB. If the
baseline peak does not clearly exceed 2 GiB, we increase workload
concurrency/among sizes until it does, before committing the full run.

### Workload — varied, overlapping, time-varying

Real filesystem operations against a real mount, driven by `fio` and small
shell/`ls` loops. Workloads come and go over the run in **phases**, and
several overlap so the limiter faces mixed pressure:

| Phase (each ~10 min) | Concurrent workloads |
|---|---|
| P1 sequential reads | N parallel large sequential reads (fio) |
| P2 add writes | P1 + M parallel sequential writers |
| P3 random reads | many-threaded random reads over a large file set |
| P4 slow reads | readers throttled with `--rate` to hold buffers open |
| P5 readdir + metadata | repeated `ls -R` / stat over a wide/deep tree |
| P6 everything at once | seq reads + writes + random + slow + readdir |

All phases run against objects under
`s3://adpeace-mountpoint-perf-test/memlimit-test/`. Object sizes and
concurrency are chosen so unconstrained demand exceeds the 2 GiB target
(confirmed by the baseline run).

Phases and their parameters are defined in a single config file
(`config.env`) so the whole run is reproducible: given a config that
breaks, we re-run the identical sequence.

### Deadlock / starvation resilience

- The RSS sampler reads `/proc`, never the mount, so it always makes
  progress.
- A **watchdog** monitors per-workload progress (fio heartbeat / file
  growth). If a workload makes no progress for `STALL_TIMEOUT` (default
  120s), the watchdog records the event, captures diagnostics
  (`/proc/<pid>/stack` of mount threads if permitted, mount log tail), and
  proceeds/aborts cleanly rather than hanging the whole test.
- Overall hard deadline: the driver force-unmounts (`fusermount -u -z`, then
  FUSE-abort via `/sys/fs/fuse/connections/<minor>/abort`) and exits so a
  wedged mount never blocks the test past its budget.
- Every phase has a timeout; a hung phase is terminated and marked, and the
  run continues to teardown + reporting so we always get a report.

## Measurement plan

Sampled every 1s into a single CSV (`metrics.csv`):

- `t` — seconds since mount start.
- `rss_bytes` — `VmRSS` from `/proc/<pid>/status` (ground truth, OS view).
- `pool_bytes_in_use` — summed across `kind` labels, from mount log.
- `pool_allocated_bytes` — from mount log.
- `mem_reserved` — `mem.bytes_reserved` if emitted.
- `phase` — active workload phase label.

Also recorded once in `run_meta.json`: memory target, `data_buffer_budget`,
part sizes, thread count, host info, mount command line, git commit,
per-phase workload parameters (quantitative), baseline peak.

## Report structure (`REPORT.md`)

1. **Summary** — does the limiter hold? One-paragraph verdict.
2. **Configuration** — memory target, derived budget, host, mount cmdline,
   commit. The exact `config.env` for reproduction.
3. **Workload** — quantitative description of each phase (files, sizes,
   concurrency, rates, durations).
4. **Results**
   - Graph: RSS over time vs `--memory-target` ceiling, phase bands shaded.
   - Graph: `pool.bytes_in_use` / `pool.allocated_bytes` vs
     `data_buffer_budget` ceiling.
   - Peak table: peak RSS, peak pool in-use, per phase.
5. **Findings** — does RSS stay under target? Under what phase/config does
   it breach, if any? Any stalls/deadlocks observed (stretch).
6. **Reproduction** — exact command to re-run the breaking config.
7. **Appendix** — raw peak numbers, any anomalies.

## Deliverables in the repo (checkin-ready)

Under `memory-stress/`:
- `run.sh` — orchestrates mount, phases, sampling, teardown. Reads
  `config.env`.
- `config.env` — all knobs (limit, phase params, durations). The reproducible
  unit.
- `sample.sh` (or inlined) — RSS + log-metrics sampler → `metrics.csv`.
- `plot.py` — reads `metrics.csv` + `run_meta.json`, emits the graphs.
- `report.py` — assembles `REPORT.md` with graphs and peak tables.
- `README.md` — how to run.

## Execution order (5h budget)

1. Build branch on `mpc7gn` in `~/memlimit-test` (fresh dir; leave
   `~/mp-work` untouched). [build is the long pole — already started]
2. Write harness; short **shakedown** (~3 min/phase) to validate config,
   sampler, teardown, and that workload exceeds 2 GiB.
3. **Baseline** (high limit) run to establish natural peak.
4. **Full constrained** run: >= 1h, `--memory-target 2048`, all phases.
5. Plot + write report. Report status at the 5h mark regardless.
