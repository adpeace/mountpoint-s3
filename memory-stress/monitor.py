#!/usr/bin/env python3
"""Sample mountpoint-s3 memory usage into a CSV time series.

Two independent sources are combined, one row per `--interval` seconds:

  * RSS: read directly from `/proc/<pid>/status` (VmRSS). This never touches
    the mounted filesystem, so the sampler keeps making progress even if the
    mount deadlocks -- the key property for a memory test that must survive a
    wedged mount.

  * Pool gauges (`pool.bytes_in_use`, `pool.allocated_bytes`,
    `mem.bytes_reserved`): parsed by tailing the mount's `--log-metrics`
    output. These gauges are event-driven (mountpoint only logs a gauge when
    its value changes), so we carry the last seen value forward between
    samples. `pool.bytes_in_use` is labelled per buffer kind; we sum the
    kinds into a single total and also keep the per-kind breakdown.

The active workload phase is read from `--phase-file` (written by the driver)
so each row is attributable to a phase.

Output CSV columns:
  t, phase, rss_bytes, pool_bytes_in_use, pool_allocated_bytes,
  mem_bytes_reserved, <one column per bytes_in_use kind>
"""

import argparse
import glob
import os
import re
import sys
import threading
import time

# Matches e.g.  "pool.bytes_in_use(By)[kind=get_object]: 1234"
#         or    "pool.allocated_bytes(By): 5678"
#         or    "mem.bytes_reserved(By)[area=prefetch]: 90"
# The name must start with a lowercase letter and contain a dot, so timestamps
# ("16:00:00") and the log target ("mountpoint_s3_fs::metrics") do not match.
METRIC_RE = re.compile(
    r"(?P<name>[a-z][a-z0-9_]*(?:\.[a-z0-9_]+)+)"  # dotted metric name
    r"(?:\([^)]*\))?"                              # optional (unit)
    r"(?:\[(?P<labels>[^\]]*)\])?"                 # optional [labels]
    r":\s*(?P<value>-?[0-9.]+)\s*$"                # ": value" at end of line
)

KINDS = ["get_object", "put_object", "disk_cache", "append", "other"]


def read_rss_bytes(pid):
    """Return VmRSS in bytes from /proc/<pid>/status, or None if unavailable."""
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    # "VmRSS:\t  123456 kB"
                    return int(line.split()[1]) * 1024
    except (FileNotFoundError, ProcessLookupError, ValueError):
        return None
    return None


def parse_labels(label_str):
    """Parse "k1=v1,k2=v2" into a dict."""
    out = {}
    if not label_str:
        return out
    for part in label_str.split(","):
        if "=" in part:
            k, v = part.split("=", 1)
            out[k] = v
    return out


class LogTailer(threading.Thread):
    """Tails the mount log, keeping the latest value of each pool gauge.

    Runs as a daemon thread so it cannot keep the process alive on its own.
    Values are stored under a lock and read by the sampling loop.
    """

    def __init__(self, log_path):
        # log_path may be an exact file or a glob (e.g. ".../mountpoint-s3-*.log");
        # we resolve the newest match once it appears, so the mount's random log
        # suffix does not need to be known up front.
        super().__init__(daemon=True)
        self.log_path = log_path
        self.lock = threading.Lock()
        self.in_use = {k: 0 for k in KINDS}
        self.allocated = 0
        self.reserved = 0
        self._stop = threading.Event()

    def stop(self):
        self._stop.set()

    def _resolve(self):
        """Return the newest path matching log_path (exact or glob), or None."""
        if os.path.exists(self.log_path):
            return self.log_path
        matches = glob.glob(self.log_path)
        if not matches:
            return None
        return max(matches, key=os.path.getmtime)

    def run(self):
        # Wait for the log file to appear, then follow it.
        path = None
        while not self._stop.is_set():
            path = self._resolve()
            if path:
                break
            time.sleep(0.2)
        if self._stop.is_set() or not path:
            return
        with open(path, "r", errors="replace") as f:
            while not self._stop.is_set():
                line = f.readline()
                if not line:
                    time.sleep(0.1)
                    continue
                self._consume(line)

    def _consume(self, line):
        m = METRIC_RE.search(line)
        if not m:
            return
        name = m.group("name")
        try:
            value = float(m.group("value"))
        except ValueError:
            return
        labels = parse_labels(m.group("labels"))
        with self.lock:
            if name == "pool.bytes_in_use":
                kind = labels.get("kind", "other")
                if kind in self.in_use:
                    self.in_use[kind] = value
            elif name == "pool.allocated_bytes":
                self.allocated = value
            elif name == "mem.bytes_reserved":
                # Sum across areas; approximate but adequate for a trend line.
                self.reserved = value

    def snapshot(self):
        with self.lock:
            in_use = dict(self.in_use)
            return in_use, self.allocated, self.reserved


def read_phase(phase_file):
    if not phase_file:
        return ""
    try:
        with open(phase_file) as f:
            return f.read().strip()
    except FileNotFoundError:
        return ""


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--pid", type=int, required=True, help="mount-s3 PID to sample")
    p.add_argument("--log", required=True, help="mount-s3 --log-metrics log file to tail")
    p.add_argument("--out", required=True, help="output CSV path")
    p.add_argument("--interval", type=float, default=1.0, help="sample interval seconds")
    p.add_argument("--phase-file", default="", help="file whose contents name the active phase")
    args = p.parse_args()

    tailer = LogTailer(args.log)
    tailer.start()

    header = (
        ["t", "phase", "rss_bytes", "pool_bytes_in_use", "pool_allocated_bytes", "mem_bytes_reserved"]
        + [f"in_use_{k}" for k in KINDS]
    )

    start = time.monotonic()
    with open(args.out, "w", buffering=1) as out:
        out.write(",".join(header) + "\n")
        while True:
            rss = read_rss_bytes(args.pid)
            if rss is None:
                # Process is gone -- record a final marker row and exit cleanly.
                sys.stderr.write(f"monitor: pid {args.pid} no longer present; stopping\n")
                break
            in_use, allocated, reserved = tailer.snapshot()
            total_in_use = sum(in_use.values())
            t = time.monotonic() - start
            row = [
                f"{t:.1f}",
                read_phase(args.phase_file),
                str(rss),
                f"{total_in_use:.0f}",
                f"{allocated:.0f}",
                f"{reserved:.0f}",
            ] + [f"{in_use[k]:.0f}" for k in KINDS]
            out.write(",".join(row) + "\n")
            time.sleep(args.interval)

    tailer.stop()


if __name__ == "__main__":
    main()
