#!/usr/bin/env python3
"""Generate an fio version-2 iolog that reproduces the multi-cursor prefetch read patterns.

Two patterns:

* ``multi_cursor``: the object is split into ``num_cursors`` equal, contiguous regions (the last
  region absorbs any remainder). Each round visits every region, reading ``visit`` bytes from it in
  ``read_size`` blocks, until all regions are exhausted. Total bytes read equals the object size.
* ``overlapping_cursors``: ``num_cursors`` evenly-spaced start offsets, every cursor reads to EOF.
  Total bytes read exceeds the object size (this is the overlap).
"""

from __future__ import annotations

import argparse
import sys
from typing import List, TextIO, Tuple


def build_regions(pattern: str, size: int, num_cursors: int) -> List[Tuple[int, int]]:
    """Return the ``[start, end)`` range for each cursor, matching ``executor.rs``.

    ``multi_cursor`` partitions the object into contiguous regions with the last cursor absorbing
    the remainder; ``overlapping_cursors`` places evenly-spaced starts that all run to EOF.
    """
    if pattern == "multi_cursor":
        region_size = size // num_cursors
        regions = []
        for i in range(num_cursors):
            start = i * region_size
            end = size if i == num_cursors - 1 else start + region_size
            regions.append((start, end))
        return regions
    if pattern == "overlapping_cursors":
        spacing = size // num_cursors
        return [(i * spacing, size) for i in range(num_cursors)]
    raise ValueError(f"unknown pattern: {pattern}")


def build_reads(regions: List[Tuple[int, int]], visit: int, read_size: int) -> List[Tuple[int, int]]:
    """Round-robin through ``regions``, emitting ``(offset, length)`` reads.

    Each round reads up to ``visit`` bytes from every still-live region in ``read_size`` blocks,
    then advances to the next region, so the cursors stay concurrently live and interleaved -- the
    same visiting scheme as ``run_interleaved_cursors``.
    """
    offsets = [start for start, _ in regions]
    reads: List[Tuple[int, int]] = []
    remaining = len(regions)
    while remaining > 0:
        for i, (_, end) in enumerate(regions):
            if offsets[i] >= end:
                continue
            visit_end = min(offsets[i] + visit, end)
            while offsets[i] < visit_end:
                length = min(read_size, visit_end - offsets[i])
                reads.append((offsets[i], length))
                offsets[i] += length
                if offsets[i] >= end:
                    remaining -= 1
                    break
    return reads


def write_iolog(out: TextIO, name: str, reads: List[Tuple[int, int]]) -> None:
    """Write a version-2 fio iolog for a single file replaying ``reads`` in order."""
    out.write("fio version 2 iolog\n")
    out.write(f"{name} add\n")
    out.write(f"{name} open\n")
    for offset, length in reads:
        out.write(f"{name} read {offset} {length}\n")
    out.write(f"{name} close\n")


def parse_args(argv: List[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--object-name", required=True, help="File name, relative to fio --directory (the S3 key)")
    parser.add_argument("--object-size", type=int, required=True, help="Object size in bytes")
    parser.add_argument("--num-cursors", type=int, required=True, help="Number of interleaved cursors/regions")
    parser.add_argument("--visit", type=int, default=128 * 1024, help="Bytes read per region per round (default 128 KiB)")
    parser.add_argument("--read-size", type=int, default=64 * 1024, help="Bytes per individual read (default 64 KiB)")
    parser.add_argument(
        "--pattern",
        choices=["multi_cursor", "overlapping_cursors"],
        default="multi_cursor",
        help="Access pattern to reproduce (default multi_cursor)",
    )
    parser.add_argument("-o", "--output", help="Output iolog path (default stdout)")
    args = parser.parse_args(argv)

    if args.num_cursors < 1:
        parser.error("--num-cursors must be >= 1")
    if args.read_size < 1:
        parser.error("--read-size must be >= 1")
    if args.visit < args.read_size:
        parser.error(f"--visit ({args.visit}) must be >= --read-size ({args.read_size})")
    if args.object_size < args.num_cursors:
        parser.error(f"--object-size ({args.object_size}) must be >= --num-cursors ({args.num_cursors})")
    return args


def main(argv: List[str]) -> int:
    args = parse_args(argv)
    regions = build_regions(args.pattern, args.object_size, args.num_cursors)
    reads = build_reads(regions, args.visit, args.read_size)

    if args.output:
        with open(args.output, "w") as f:
            write_iolog(f, args.object_name, reads)
    else:
        write_iolog(sys.stdout, args.object_name, reads)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
