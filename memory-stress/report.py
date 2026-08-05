#!/usr/bin/env python3
"""Assemble REPORT.md for a memory-limiter stress run.

Computes peak RSS and peak pool memory (overall and per phase), renders the
verdict (did RSS stay under --memory-target? did the pool stay under its
budget?), and writes a markdown report embedding rss.png and pool.png.

Run after plot.py:
    uv run --with matplotlib report.py <run_dir>
"""

import argparse
import csv
import json
import os

MIB = 1024 * 1024


def load(run_dir):
    with open(os.path.join(run_dir, "run_meta.json")) as f:
        meta = json.load(f)
    with open(os.path.join(run_dir, "metrics.csv")) as f:
        rows = list(csv.DictReader(f))
    return meta, rows


def mib(v):
    return f"{v / MIB:.0f} MiB"


def peaks_by_phase(rows, col):
    peaks = {}
    for r in rows:
        phase = r["phase"]
        v = float(r[col])
        if phase not in peaks or v > peaks[phase]:
            peaks[phase] = v
    return peaks


def overall_peak(rows, col):
    return max((float(r[col]) for r in rows), default=0.0)


def read_stalls(run_dir):
    path = os.path.join(run_dir, "stalls.log")
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return [ln.rstrip() for ln in f if ln.strip()]


def main():
    p = argparse.ArgumentParser()
    p.add_argument("run_dir")
    args = p.parse_args()
    run_dir = args.run_dir
    meta, rows = load(run_dir)
    if not rows:
        raise SystemExit("no rows in metrics.csv")

    target = meta["memory_target_mib"] * MIB
    budget = meta["data_buffer_budget_mib"] * MIB

    peak_rss = overall_peak(rows, "rss_bytes")
    peak_in_use = overall_peak(rows, "pool_bytes_in_use")
    peak_alloc = overall_peak(rows, "pool_allocated_bytes")

    rss_ok = peak_rss <= target
    pool_ok = peak_in_use <= budget and peak_alloc <= budget
    stalls = read_stalls(run_dir)

    rss_phase = peaks_by_phase(rows, "rss_bytes")
    inuse_phase = peaks_by_phase(rows, "pool_bytes_in_use")
    duration = float(rows[-1]["t"])

    verdict = "HELD" if rss_ok else "EXCEEDED"

    lines = []
    lines.append(f"# Memory-limiter stress test report — {meta['run_label']}")
    lines.append("")
    lines.append("## Summary")
    lines.append("")
    lines.append(
        f"With `--memory-target {meta['memory_target_mib']}` MiB, over a "
        f"{duration/60:.0f}-minute mixed-workload run, the process RSS **{verdict}** "
        f"the target: peak RSS was **{mib(peak_rss)}** "
        f"({peak_rss/target*100:.0f}% of target). "
        f"Pool `bytes_in_use` peaked at **{mib(peak_in_use)}** vs the "
        f"data-buffer budget of {mib(budget)} "
        f"({'within budget' if pool_ok else 'OVER BUDGET'})."
    )
    if stalls:
        lines.append("")
        lines.append(f"**{len(stalls)} stall/anomaly event(s) were recorded** (see Findings).")
    lines.append("")

    lines.append("## Configuration")
    lines.append("")
    lines.append(f"- Commit: `{meta['git_commit']}`")
    lines.append(f"- Host: `{meta['host']}` ({meta['nproc']} cores)")
    lines.append(f"- `--memory-target`: {meta['memory_target_mib']} MiB")
    lines.append(
        f"- Derived data-buffer budget: {meta['data_buffer_budget_mib']} MiB "
        f"(`mem_limit - max(mem_limit/8, 128 MiB)`)"
    )
    lines.append(f"- Read part size: {meta['read_part_size']}; write part size: {meta['write_part_size']}")
    lines.append(f"- Max threads: {meta['max_threads']}")
    lines.append(f"- Mount command: `{meta['mount_cmd']}`")
    lines.append("")

    lines.append("## Workload")
    lines.append("")
    lines.append(
        f"Read pool: {meta['read_pool_count']} objects x {meta['read_object_size_mib']} MiB "
        f"= {meta['read_pool_count'] * meta['read_object_size_mib'] / 1024:.0f} GiB total. "
        f"readdir tree: {meta['readdir_dirs']} dirs x {meta['readdir_files_per_dir']} files. "
        f"Each phase ran for {meta['phase_secs']}s."
    )
    lines.append("")
    lines.append("| Phase | Workload | Concurrency |")
    lines.append("|---|---|---|")
    lines.append(f"| P1 | sequential reads | {meta['seq_readers']} readers |")
    lines.append(f"| P2 | sequential reads + writers | {meta['seq_readers']} readers + {meta['writers']} writers |")
    lines.append(f"| P3 | random reads | {meta['rand_readers']} readers |")
    lines.append(f"| P4 | slow (rate-limited) reads | {meta['slow_readers']} readers @ {meta['slow_read_rate']} |")
    lines.append("| P5 | readdir + stat loop | 1 recursive lister |")
    lines.append(f"| P6 | all at once | {meta['seq_readers']}+{meta['rand_readers']} readers + {meta['writers']} writers + readdir |")
    lines.append("")

    lines.append("## Results")
    lines.append("")
    lines.append("### RSS vs memory target")
    lines.append("")
    lines.append("![RSS over time](rss.png)")
    lines.append("")
    lines.append("### Pool memory vs data-buffer budget")
    lines.append("")
    lines.append("![Pool memory over time](pool.png)")
    lines.append("")
    lines.append("### Peak memory by phase")
    lines.append("")
    lines.append("| Phase | Peak RSS | Peak pool in-use |")
    lines.append("|---|---|---|")
    for phase in [p for p in rss_phase if p not in ("startup", "prepare_datasets", "teardown", "done")]:
        lines.append(f"| {phase} | {mib(rss_phase[phase])} | {mib(inuse_phase.get(phase, 0))} |")
    lines.append(f"| **overall** | **{mib(peak_rss)}** | **{mib(peak_in_use)}** |")
    lines.append("")

    lines.append("## Findings")
    lines.append("")
    if rss_ok:
        lines.append(
            f"- RSS stayed under `--memory-target` throughout: peak {mib(peak_rss)} "
            f"<= {mib(target)}. The limiter held under this workload."
        )
    else:
        over_phases = [p for p, v in rss_phase.items() if v > target]
        lines.append(
            f"- **RSS exceeded `--memory-target`.** Peak {mib(peak_rss)} > {mib(target)} "
            f"({peak_rss/target*100:.0f}%). Breach occurred during phase(s): {', '.join(over_phases)}."
        )
    if pool_ok:
        lines.append(
            f"- Pool-tracked memory stayed within the data-buffer budget "
            f"(in-use peak {mib(peak_in_use)}, allocated peak {mib(peak_alloc)} <= {mib(budget)})."
        )
    else:
        lines.append(
            f"- **Pool-tracked memory exceeded the data-buffer budget** "
            f"(in-use peak {mib(peak_in_use)}, allocated peak {mib(peak_alloc)} vs {mib(budget)}). "
            f"This indicates the pool accounting itself let allocations past the budget."
        )
    if not rss_ok and pool_ok:
        lines.append(
            f"- The pool stayed within budget but RSS did not: the overshoot "
            f"({mib(peak_rss - budget)} above the budget, {mib(peak_rss - target)} above target) "
            f"is untracked memory (metadata, CRT, allocator fragmentation) not governed by the pool. "
            f"The 12.5%/128 MiB overhead reserve was insufficient for this workload."
        )
    if stalls:
        lines.append("- Recorded stall/anomaly events:")
        for s in stalls:
            lines.append(f"  - `{s}`")
    else:
        lines.append("- No stalls or deadlocks were detected (all phases progressed and unmount succeeded).")
    lines.append("")

    lines.append("## Reproduction")
    lines.append("")
    lines.append("This run is reproducible from the config in this directory:")
    lines.append("")
    lines.append("```")
    lines.append(f"./run.sh config.env {meta['run_label']}")
    lines.append("```")
    lines.append("")
    lines.append(f"against commit `{meta['git_commit']}`. The exact mount command and all")
    lines.append("workload parameters are recorded in `run_meta.json` alongside this report.")
    lines.append("")

    out = os.path.join(run_dir, "REPORT.md")
    with open(out, "w") as f:
        f.write("\n".join(lines) + "\n")
    print(f"wrote {out}")
    print(f"verdict: RSS {verdict} (peak {mib(peak_rss)} vs target {mib(target)})")


if __name__ == "__main__":
    main()
