"""Tests for the trace log parser."""

from pathlib import Path

from prefetch_visualizer.parser import parse_log

# A minimal but representative slice of a real prefetcher trace log. Mixes an
# ANSI-free line (as written to --log-directory) with a colourised console line
# to exercise the ANSI stripping.
ANSI = "\x1b[2m"
RESET = "\x1b[0m"

SAMPLE = f"""\
2026-07-08T16:42:31.250517Z TRACE mountpoint_s3_fs::prefetch: read offset=0 length=131072
2026-07-08T16:42:31.269756Z TRACE mountpoint_s3_fs::prefetch: created new cursor cursor_id=1 key="hello" offset=0 tracked_cursors=1
2026-07-08T16:42:31.269830Z TRACE prefetch{{range=0..16777216 out of 16777216}}: mountpoint_s3_fs::prefetch::part_stream: received GetObject part offset=1048576 length=131072
2026-07-08T16:42:31.272200Z TRACE mountpoint_s3_fs::prefetch::cursor: out-of-order read for this cursor cursor_id=1 expected=131072 actual=8388608
2026-07-08T16:42:31.272852Z TRACE mountpoint_s3_fs::prefetch: created new cursor cursor_id=2 key="hello" offset=8388608 tracked_cursors=2
{ANSI}2026-07-08T16:42:31.275234Z{RESET} TRACE mountpoint_s3_fs::prefetch::cursor: cursor serving read cursor_id=1 offset=131072 length=131072 available_offset=1179648 read_window_end_offset=1179648
2026-07-08T16:42:31.275264Z TRACE mountpoint_s3_fs::prefetch: switched active cursor cursor_id=1 key="hello" offset=131072
this line is unrelated noise and should be ignored
"""


def _write(tmp_path: Path) -> Path:
    log = tmp_path / "trace.log"
    log.write_text(SAMPLE)
    return log


def test_parses_all_event_types(tmp_path):
    trace = parse_log(_write(tmp_path))

    assert trace.object_key == "hello"
    assert trace.cursor_ids == [1, 2]

    assert len(trace.served_reads) == 1
    read = trace.served_reads[0]
    assert read.cursor_id == 1
    assert read.offset == 131072
    assert read.available_offset == 1179648
    assert read.read_window_end_offset == 1179648

    created = [c for c in trace.lifecycle if c.kind == "created"]
    switched = [c for c in trace.lifecycle if c.kind == "switched"]
    assert len(created) == 2
    assert len(switched) == 1

    assert len(trace.out_of_order) == 1
    assert trace.out_of_order[0].expected == 131072
    assert trace.out_of_order[0].actual == 8388608

    assert len(trace.getobject_parts) == 1
    assert trace.getobject_parts[0].offset == 1048576


def test_relative_timestamps(tmp_path):
    trace = parse_log(_write(tmp_path))
    # Times are relative to the first timestamped line in the log (here a `read`
    # event, which is not itself stored), so all stored events are non-negative
    # and the trace spans a positive duration.
    assert all(c.t >= 0.0 for c in trace.lifecycle)
    assert trace.duration > 0.0


def test_empty_log(tmp_path):
    log = tmp_path / "empty.log"
    log.write_text("nothing to see here\n")
    trace = parse_log(log)
    assert trace.is_empty()
