# /// script
# requires-python = ">=3.9"
# dependencies = []
# ///
"""Summarize a multi-cursor FUSE run: read amplification, cursor count, and throughput.

The amplification and cursor metrics come from the Mountpoint prefetch TRACE log (produced by
mounting with mountpoint.prefetch_trace=true, which sets MOUNTPOINT_LOG=...prefetch=trace on the
--log-directory file). Throughput and elapsed time come from the fio JSON output.

Usage:
  uv run analyze_multi_cursor.py --trace <mp_log> --object-size <bytes> [--fio-json <fio.json>]
"""

import argparse
import json
import re
import sys
from collections import namedtuple

_PART_LEN_RE = re.compile(r"received GetObject part.*?length=(\d+)")
_CREATED_RE = re.compile(r"created new cursor")
_SWITCHED_RE = re.compile(r"switched active cursor")

# Memory-subsystem metrics emitted by `--log-metrics` (target mountpoint_s3_fs::metrics). The metrics
# tracing layer renders each metric as "<name>[label=value,...]: <value>" (plus a " (n=N)" suffix for
# counters aggregated over more than one increment in a publish window). The three metrics of interest
# (pool.bytes_in_use, mem.bytes_reserved, mem.cursor_resets) carry no unit, so there is no "(unit)"
# suffix between the name and the label block.
_METRIC_RE = re.compile(
    r"mountpoint_s3_fs::metrics:\s+"
    r"(?P<name>[\w.]+)"  # metric name, e.g. pool.bytes_in_use
    r"(?:\((?P<unit>[^)]*)\))?"  # optional (unit) suffix, e.g. (By) if the metric registers a unit
    r"(?:\[(?P<labels>[^\]]*)\])?"  # optional [key=value,...] label block
    r":\s+"
    r"(?P<value>-?\d+(?:\.\d+)?)"  # numeric value, before any " (n=N)" counter suffix
)

# Peak gauge values (bytes) and the cumulative counter total. Each field is None when the metric never
# appeared in the log (e.g. Mountpoint ran without --log-metrics), so callers can skip absent metrics.
MemoryMetrics = namedtuple(
    "MemoryMetrics",
    ["pool_bytes_in_use_peak", "prefetch_reserved_peak", "cursor_resets_total"],
)


def _parse_labels(labels):
    """Parse a "key=value,key=value" metric label block into a dict (empty when there are none)."""
    parsed = {}
    if labels:
        for part in labels.split(","):
            key, sep, value = part.partition("=")
            if sep:
                parsed[key] = value
    return parsed


def scan_metrics(path):
    """Return a MemoryMetrics from the memory metrics in a `--log-metrics` Mountpoint log.

    Gauges (pool.bytes_in_use, mem.bytes_reserved[area=prefetch]) report their peak observed value;
    the mem.cursor_resets counter is emitted as a per-publish-window delta, so its run total is the
    sum across all lines. Any metric that never appears stays None so it can be reported as absent
    rather than as zero.
    """
    pool_bytes_in_use_peak = None
    prefetch_reserved_peak = None
    cursor_resets_total = None
    with open(path, "r", errors="replace") as f:
        for line in f:
            m = _METRIC_RE.search(line)
            if not m:
                continue
            name = m.group("name")
            value = float(m.group("value"))
            labels = _parse_labels(m.group("labels"))
            if name == "pool.bytes_in_use":
                pool_bytes_in_use_peak = value if pool_bytes_in_use_peak is None else max(pool_bytes_in_use_peak, value)
            elif name == "mem.bytes_reserved" and labels.get("area") == "prefetch":
                prefetch_reserved_peak = value if prefetch_reserved_peak is None else max(prefetch_reserved_peak, value)
            elif name == "mem.cursor_resets":
                cursor_resets_total = value if cursor_resets_total is None else cursor_resets_total + value
    return MemoryMetrics(pool_bytes_in_use_peak, prefetch_reserved_peak, cursor_resets_total)


def _fmt_count(value):
    """Format a metric value as an int when it is integral (byte counts always are), else as a float."""
    return int(value) if float(value).is_integer() else value


def scan_trace(path):
    """Return (bytes_fetched, cursors_created, cursor_switches) from a prefetch TRACE log."""
    fetched = 0
    created = 0
    switched = 0
    with open(path, "r", errors="replace") as f:
        for line in f:
            m = _PART_LEN_RE.search(line)
            if m:
                fetched += int(m.group(1))
            if _CREATED_RE.search(line):
                created += 1
            if _SWITCHED_RE.search(line):
                switched += 1
    return fetched, created, switched


def read_fio_throughput(path):
    """Return (read_bytes, elapsed_seconds) summed across all jobs in an fio JSON report.

    Returns None if the JSON is missing, empty, or truncated -- an aborted or crashed fio run
    leaves a 0-byte or partial file, and the trace-derived metrics are still worth reporting.
    """
    try:
        with open(path, "r") as f:
            report = json.load(f)
    except (OSError, json.JSONDecodeError):
        return None
    read_bytes = 0
    runtime_ms = 0
    for job in report.get("jobs", []):
        read = job.get("read", {})
        read_bytes += read.get("io_bytes", 0)
        runtime_ms = max(runtime_ms, read.get("runtime", 0))
    return read_bytes, runtime_ms / 1000.0


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--trace", required=True, help="Mountpoint prefetch TRACE log file")
    parser.add_argument("--object-size", type=int, required=True, help="Object size in bytes")
    parser.add_argument("--fio-json", help="fio JSON output (for throughput)")
    args = parser.parse_args(argv)

    fetched, created, switched = scan_trace(args.trace)
    memory = scan_metrics(args.trace)
    obj = args.object_size

    print(f"object          : {obj} bytes ({obj / 1073741824:.2f} GiB)")
    print(f"fetched         : {fetched} bytes ({fetched / 1073741824:.2f} GiB)")
    # read-vs-size ratio = S3 bytes fetched / object size. This is descriptive, NOT a waste measure:
    # an overlapping workload where the application re-reads regions legitimately pushes it above 1x.
    # The true "does the prefetcher fetch bytes the app never asked for" measure is over-fetch below
    # (fetched / app-requested), which needs the fio read total.
    print(f"read-vs-size ratio : {(fetched / obj if obj else 0):.4f}x")
    print(f"cursors created : {created}")
    print(f"cursor switches : {switched}")

    # Memory-subsystem metrics, present only when Mountpoint ran with --log-metrics. Each is skipped
    # when its metric never appeared in the log, so a run without metrics prints nothing extra here.
    if memory.pool_bytes_in_use_peak is not None:
        peak = memory.pool_bytes_in_use_peak
        print(f"pool in-use peak : {_fmt_count(peak)} bytes ({peak / 1073741824:.2f} GiB)")
    if memory.prefetch_reserved_peak is not None:
        peak = memory.prefetch_reserved_peak
        print(f"prefetch reserved peak : {_fmt_count(peak)} bytes ({peak / 1073741824:.2f} GiB)")
    if memory.cursor_resets_total is not None:
        print(f"cursor resets   : {_fmt_count(memory.cursor_resets_total)}")

    if args.fio_json:
        throughput = read_fio_throughput(args.fio_json)
        if throughput is None:
            print(f"fio read        : (no usable fio JSON at {args.fio_json} -- run may have aborted)")
        else:
            read_bytes, elapsed = throughput
            tput = read_bytes / elapsed / 1_000_000 if elapsed else 0
            print(f"fio read        : {read_bytes} bytes ({read_bytes / 1073741824:.2f} GiB)")
            print(f"elapsed         : {elapsed:.2f} s")
            print(f"throughput      : {tput:.1f} MB/s")
            # over-fetch = S3 bytes fetched / bytes the application actually requested (fio read
            # total). ~1.0x means the prefetcher fetched only what was asked (no waste), regardless
            # of pattern -- this is the real "no over-fetch" claim, unlike read-vs-size ratio which
            # rises with legitimate application re-reads.
            if read_bytes > 0:
                print(f"over-fetch      : {fetched / read_bytes:.4f}x (fetched / app-requested)")
            # A read total below the contiguous object size means the workload did not complete;
            # the ratios above are then over a partial run. (Overlapping patterns legitimately read
            # more than the object size, so this check only applies to contiguous/sequential.)
            if read_bytes < obj:
                print(
                    f"WARNING         : fio read {read_bytes} < object {obj} bytes -- run did not "
                    "complete; ratios above are over a partial run, not final figures"
                )

    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
