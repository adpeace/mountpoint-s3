#!/usr/bin/env python3
"""Plot RSS and pool memory over time from a run's metrics.csv.

Produces two PNGs in the run directory:

  * rss.png             -- OS-reported RSS over time, with the --memory-target
                           ceiling drawn as a horizontal line and phase bands
                           shaded/annotated.
  * pool.png            -- pool.bytes_in_use (total) and pool.allocated_bytes
                           over time, with the data_buffer_budget ceiling.

Requires matplotlib. Run under a venv, e.g.:
    uv run --with matplotlib plot.py <run_dir>
"""

import argparse
import csv
import json
import os

import matplotlib

# Select the non-interactive Agg backend before importing pyplot so the script
# renders PNGs on a headless host. This must precede the pyplot import.
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.ticker import FuncFormatter

MIB = 1024 * 1024


def load(run_dir):
    with open(os.path.join(run_dir, "run_meta.json")) as f:
        meta = json.load(f)
    with open(os.path.join(run_dir, "metrics.csv")) as f:
        rows = list(csv.DictReader(f))
    return meta, rows


def phase_spans(rows):
    """Yield (phase_name, t_start, t_end) contiguous spans."""
    spans = []
    cur = None
    for r in rows:
        phase = r["phase"]
        t = float(r["t"])
        if cur is None or phase != cur[0]:
            if cur is not None:
                spans.append((cur[0], cur[1], t))
            cur = (phase, t)
    if cur is not None:
        spans.append((cur[0], cur[1], float(rows[-1]["t"])))
    return spans


def shade_phases(ax, spans):
    colors = plt.cm.tab20.colors
    seen = {}
    for i, (name, t0, t1) in enumerate(spans):
        c = colors[hash(name) % len(colors)]
        ax.axvspan(t0, t1, color=c, alpha=0.08)
        # Label once per phase name, near the top.
        if name not in seen:
            seen[name] = True
            ax.annotate(
                name, xy=((t0 + t1) / 2, 1.0), xycoords=("data", "axes fraction"),
                ha="center", va="bottom", fontsize=6, rotation=30,
            )


def mib_formatter():
    return FuncFormatter(lambda v, _: f"{v / MIB:.0f}")


def plot_rss(run_dir, meta, rows, spans):
    t = [float(r["t"]) for r in rows]
    rss = [float(r["rss_bytes"]) for r in rows]
    target = meta["memory_target_mib"] * MIB

    fig, ax = plt.subplots(figsize=(12, 5))
    ax.plot(t, rss, color="tab:blue", lw=1.2, label="RSS (VmRSS)")
    ax.axhline(target, color="tab:red", ls="--", lw=1.5,
               label=f"--memory-target ({meta['memory_target_mib']} MiB)")
    shade_phases(ax, spans)
    ax.set_xlabel("time (s)")
    ax.set_ylabel("memory (MiB)")
    ax.yaxis.set_major_formatter(mib_formatter())
    ax.set_ylim(bottom=0)
    ax.set_title(f"Process RSS vs memory target  (commit {meta['git_commit'][:10]})")
    ax.legend(loc="upper left", fontsize=8)
    ax.grid(True, alpha=0.3)
    out = os.path.join(run_dir, "rss.png")
    fig.tight_layout()
    fig.savefig(out, dpi=120)
    plt.close(fig)
    return out


def plot_pool(run_dir, meta, rows, spans):
    t = [float(r["t"]) for r in rows]
    in_use = [float(r["pool_bytes_in_use"]) for r in rows]
    allocated = [float(r["pool_allocated_bytes"]) for r in rows]
    budget = meta["data_buffer_budget_mib"] * MIB

    fig, ax = plt.subplots(figsize=(12, 5))
    ax.plot(t, allocated, color="tab:orange", lw=1.0, label="pool.allocated_bytes")
    ax.plot(t, in_use, color="tab:green", lw=1.2, label="pool.bytes_in_use (all kinds)")
    ax.axhline(budget, color="tab:red", ls="--", lw=1.5,
               label=f"data_buffer_budget ({meta['data_buffer_budget_mib']} MiB)")
    shade_phases(ax, spans)
    ax.set_xlabel("time (s)")
    ax.set_ylabel("memory (MiB)")
    ax.yaxis.set_major_formatter(mib_formatter())
    ax.set_ylim(bottom=0)
    ax.set_title("Mountpoint pool memory vs data-buffer budget")
    ax.legend(loc="upper left", fontsize=8)
    ax.grid(True, alpha=0.3)
    out = os.path.join(run_dir, "pool.png")
    fig.tight_layout()
    fig.savefig(out, dpi=120)
    plt.close(fig)
    return out


def main():
    p = argparse.ArgumentParser()
    p.add_argument("run_dir")
    args = p.parse_args()
    meta, rows = load(args.run_dir)
    if not rows:
        raise SystemExit("no rows in metrics.csv")
    spans = phase_spans(rows)
    r = plot_rss(args.run_dir, meta, rows, spans)
    q = plot_pool(args.run_dir, meta, rows, spans)
    print(f"wrote {r}")
    print(f"wrote {q}")


if __name__ == "__main__":
    main()
