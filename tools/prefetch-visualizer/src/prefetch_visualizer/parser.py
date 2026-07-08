"""Parse Mountpoint prefetcher trace logs into structured cursor events.

The parser is tolerant of the two shapes a Mountpoint log can take:

* files written by ``--log-directory`` (``with_ansi(false)``), and
* console/captured output that still contains ANSI colour escapes.

It reads the ``mountpoint_s3_fs::prefetch`` family of ``TRACE`` events emitted by
the prefetcher and turns them into a :class:`Trace` of typed events keyed by
cursor id, which the plotting layer renders.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

# Strip ANSI SGR escape sequences (e.g. "\x1b[2m") so we can parse either the
# ansi-free log file or colourised console output identically.
_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")

# Leading RFC3339 timestamp emitted by the tracing fmt layer, e.g.
# "2026-07-08T16:42:31.250517Z".
_TIMESTAMP_RE = re.compile(r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)")

# The tracing fmt layer prints fields as "key=value" pairs. Values are either
# bare tokens or double-quoted strings. We capture both.
_FIELD_RE = re.compile(r'(\w+)=("(?:[^"\\]|\\.)*"|\S+)')

# Message markers we care about. These are the literal message strings passed to
# the `trace!` macros in prefetch.rs / prefetch/cursor.rs / prefetch/part_stream.rs.
_MSG_CURSOR_SERVING = "cursor serving read"
_MSG_CREATED = "created new cursor"
_MSG_SWITCHED = "switched active cursor"
_MSG_SEEK_TRY = "trying to seek"
_MSG_OUT_OF_ORDER = "out-of-order read for this cursor"
_MSG_GETOBJECT_PART = "received GetObject part"


@dataclass
class ServedRead:
    """A read actually served by a cursor, with its live frontier state."""

    t: float  # seconds since the first event in the trace
    cursor_id: int
    offset: int
    length: int
    available_offset: int  # how far data has been downloaded for this cursor
    read_window_end_offset: int  # edge of the backpressure read window


@dataclass
class CursorLifecycle:
    """A cursor was created or switched-to."""

    t: float
    cursor_id: int
    offset: int
    kind: str  # "created" | "switched"


@dataclass
class OutOfOrder:
    """A read missed a cursor's sequential position (seek attempt failed)."""

    t: float
    cursor_id: int
    expected: int
    actual: int


@dataclass
class GetObjectPart:
    """A part delivered by the underlying GetObject stream (fetched from S3)."""

    t: float
    offset: int
    length: int


@dataclass
class Trace:
    """All parsed events, grouped by type, plus derived bounds."""

    served_reads: list[ServedRead] = field(default_factory=list)
    lifecycle: list[CursorLifecycle] = field(default_factory=list)
    out_of_order: list[OutOfOrder] = field(default_factory=list)
    getobject_parts: list[GetObjectPart] = field(default_factory=list)
    object_key: str | None = None

    @property
    def cursor_ids(self) -> list[int]:
        ids = {r.cursor_id for r in self.served_reads}
        ids.update(c.cursor_id for c in self.lifecycle)
        ids.update(o.cursor_id for o in self.out_of_order)
        return sorted(ids)

    @property
    def max_offset(self) -> int:
        candidates = [0]
        candidates += [r.offset + r.length for r in self.served_reads]
        candidates += [r.read_window_end_offset for r in self.served_reads]
        candidates += [p.offset + p.length for p in self.getobject_parts]
        return max(candidates)

    @property
    def duration(self) -> float:
        times = (
            [r.t for r in self.served_reads]
            + [c.t for c in self.lifecycle]
            + [o.t for o in self.out_of_order]
            + [p.t for p in self.getobject_parts]
        )
        return max(times) if times else 0.0

    def is_empty(self) -> bool:
        return not (self.served_reads or self.lifecycle or self.getobject_parts)


def _parse_fields(rest: str) -> dict[str, str]:
    fields: dict[str, str] = {}
    for key, value in _FIELD_RE.findall(rest):
        if value.startswith('"') and value.endswith('"'):
            value = value[1:-1]
        fields[key] = value
    return fields


def _parse_timestamp(line: str) -> datetime | None:
    match = _TIMESTAMP_RE.search(line)
    if not match:
        return None
    raw = match.group(1)
    # datetime.fromisoformat handles the fractional seconds; normalise the "Z".
    return datetime.fromisoformat(raw.replace("Z", "+00:00")).astimezone(timezone.utc)


def parse_log(path: Path) -> Trace:
    """Parse a Mountpoint trace log file into a :class:`Trace`.

    Lines that are not recognised prefetch events are ignored, so the parser
    works on a full mount log as well as on filtered output.
    """
    trace = Trace()
    epoch: datetime | None = None

    with path.open("r", errors="replace") as handle:
        for raw_line in handle:
            line = _ANSI_RE.sub("", raw_line).rstrip("\n")
            if "prefetch" not in line:
                continue

            timestamp = _parse_timestamp(line)
            if timestamp is None:
                # Without a timestamp we cannot place the event on the time axis.
                continue
            if epoch is None:
                epoch = timestamp
            t = (timestamp - epoch).total_seconds()

            if _MSG_CURSOR_SERVING in line:
                f = _parse_fields(line)
                _add_served_read(trace, t, f)
            elif _MSG_CREATED in line:
                f = _parse_fields(line)
                _add_lifecycle(trace, t, f, "created")
            elif _MSG_SWITCHED in line:
                f = _parse_fields(line)
                _add_lifecycle(trace, t, f, "switched")
            elif _MSG_OUT_OF_ORDER in line:
                f = _parse_fields(line)
                _add_out_of_order(trace, t, f)
            elif _MSG_GETOBJECT_PART in line:
                f = _parse_fields(line)
                _add_getobject_part(trace, t, f)

    return trace


def _int(fields: dict[str, str], key: str) -> int | None:
    value = fields.get(key)
    if value is None:
        return None
    try:
        return int(value)
    except ValueError:
        return None


def _add_served_read(trace: Trace, t: float, f: dict[str, str]) -> None:
    cursor_id = _int(f, "cursor_id")
    offset = _int(f, "offset")
    length = _int(f, "length")
    if cursor_id is None or offset is None or length is None:
        return
    trace.served_reads.append(
        ServedRead(
            t=t,
            cursor_id=cursor_id,
            offset=offset,
            length=length,
            available_offset=_int(f, "available_offset") or offset,
            read_window_end_offset=_int(f, "read_window_end_offset") or offset,
        )
    )


def _add_lifecycle(trace: Trace, t: float, f: dict[str, str], kind: str) -> None:
    cursor_id = _int(f, "cursor_id")
    offset = _int(f, "offset")
    if cursor_id is None or offset is None:
        return
    if trace.object_key is None and "key" in f:
        trace.object_key = f["key"]
    trace.lifecycle.append(CursorLifecycle(t=t, cursor_id=cursor_id, offset=offset, kind=kind))


def _add_out_of_order(trace: Trace, t: float, f: dict[str, str]) -> None:
    cursor_id = _int(f, "cursor_id")
    expected = _int(f, "expected")
    actual = _int(f, "actual")
    if cursor_id is None or expected is None or actual is None:
        return
    trace.out_of_order.append(OutOfOrder(t=t, cursor_id=cursor_id, expected=expected, actual=actual))


def _add_getobject_part(trace: Trace, t: float, f: dict[str, str]) -> None:
    offset = _int(f, "offset")
    length = _int(f, "length")
    if offset is None or length is None:
        return
    trace.getobject_parts.append(GetObjectPart(t=t, offset=offset, length=length))
