"""Command-line entrypoint for the prefetch visualizer.

Example:

    uv run prefetch-visualizer /path/to/mountpoint-s3-*.log -o outputs/cursors.png

Produce a log by running Mountpoint (or a prefetch test) with trace logging for
the prefetch target enabled, e.g.:
    MOUNTPOINT_LOG="warn,mountpoint_s3_fs::prefetch=trace" \
        mount-s3 <BUCKET> <MNT> --log-directory /var/log/mountpoint

    RUST_LOG=mountpoint_s3_fs::prefetch=trace cargo test -p mountpoint-s3-fs --lib \
        prefetch::tests::multi_cursor -- --nocapture > trace.log 2>&1
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from .parser import parse_log
from .plot import render


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="prefetch-visualizer",
        description="Visualize Mountpoint prefetcher cursor activity from a debug/trace log file.",
    )
    parser.add_argument("log", type=Path, help="path to a Mountpoint trace/--debug log file")
    parser.add_argument(
        "-o",
        "--output",
        type=Path,
        default=Path("outputs/prefetch-cursors.png"),
        help="output image path (default: outputs/prefetch-cursors.png)",
    )
    parser.add_argument("--title", type=str, default=None, help="override the chart title")
    parser.add_argument(
        "--unit",
        choices=["MiB", "bytes"],
        default="MiB",
        help="y-axis unit for object offset (default: MiB)",
    )
    parser.add_argument(
        "--summary",
        action="store_true",
        help="print a text summary of the parsed events and exit without plotting",
    )
    args = parser.parse_args(argv)

    if not args.log.is_file():
        print(f"error: log file not found: {args.log}", file=sys.stderr)
        return 2

    trace = parse_log(args.log)

    if trace.is_empty():
        print(
            "error: no prefetch events found. Produce the log with trace logging enabled for the "
            "prefetch target, e.g. MOUNTPOINT_LOG='mountpoint_s3_fs::prefetch=trace'.",
            file=sys.stderr,
        )
        return 1

    if args.summary:
        _print_summary(trace)
        return 0

    args.output.parent.mkdir(parents=True, exist_ok=True)
    render(trace, args.output, title=args.title, y_unit=args.unit)
    print(f"wrote {args.output}")
    return 0


def _print_summary(trace) -> None:
    print(f"object key:        {trace.object_key or '<unknown>'}")
    print(f"cursors:           {trace.cursor_ids}")
    print(f"served reads:      {len(trace.served_reads)}")
    print(f"cursors created:   {sum(1 for c in trace.lifecycle if c.kind == 'created')}")
    print(f"cursor switches:   {sum(1 for c in trace.lifecycle if c.kind == 'switched')}")
    print(f"out-of-order:      {len(trace.out_of_order)}")
    print(f"GetObject parts:   {len(trace.getobject_parts)}")
    print(f"duration:          {trace.duration:.3f}s")
    print(f"max offset:        {trace.max_offset / (1024 * 1024):.2f} MiB")


if __name__ == "__main__":
    raise SystemExit(main())
