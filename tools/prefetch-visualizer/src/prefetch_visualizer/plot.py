"""Render a parsed prefetcher :class:`~prefetch_visualizer.parser.Trace`.

The chart is a time (x) vs file-offset (y) view:

* Each cursor gets its own colour.
* Served reads are plotted as points at (time, read offset).
* For each cursor, two step lines track its frontier over time:
  the **downloaded** offset (``available_offset``) and the **backpressure read
  window** edge (``read_window_end_offset``). The band between the read position
  and the window edge is shaded to show how far ahead the prefetcher is willing
  to fetch.
* GetObject parts fetched from S3 are drawn as faint markers.
* Out-of-order reads (a read that missed a cursor's position) are marked with an
  x, since those are the events that would cause a reset in the single-cursor
  world.
"""

from __future__ import annotations

from pathlib import Path

from matplotlib import colormaps
from matplotlib.figure import Figure
from matplotlib.lines import Line2D

from .parser import Trace

_MIB = 1024 * 1024


def _color_map(cursor_ids: list[int]) -> dict[int, tuple]:
    cmap = colormaps["tab10"]
    return {cid: cmap(i % 10) for i, cid in enumerate(cursor_ids)}


def render(trace: Trace, output: Path, title: str | None = None, y_unit: str = "MiB") -> None:
    """Render ``trace`` to an image file at ``output``."""
    if trace.is_empty():
        raise ValueError("cannot render an empty trace: no prefetch events found")

    divisor = _MIB if y_unit == "MiB" else 1
    cursor_ids = trace.cursor_ids
    colors = _color_map(cursor_ids)

    fig = Figure(figsize=(14, 8))
    ax = fig.subplots()

    # Per-cursor frontier lines: downloaded offset and read-window edge.
    for cid in cursor_ids:
        reads = sorted((r for r in trace.served_reads if r.cursor_id == cid), key=lambda r: r.t)
        if not reads:
            continue
        color = colors[cid]
        times = [r.t for r in reads]
        read_pos = [r.offset / divisor for r in reads]
        downloaded = [r.available_offset / divisor for r in reads]
        window_edge = [r.read_window_end_offset / divisor for r in reads]

        # Shade the region between the current read position and the read-window
        # edge: this is the outstanding backpressure window the cursor may fetch.
        ax.fill_between(
            times,
            read_pos,
            window_edge,
            step="post",
            color=color,
            alpha=0.10,
            linewidth=0,
        )
        # Downloaded frontier (data actually in the part queue).
        ax.step(times, downloaded, where="post", color=color, alpha=0.55, linewidth=1.0, linestyle="--")
        # Backpressure read-window edge (how far S3 has been asked to send).
        ax.step(times, window_edge, where="post", color=color, alpha=0.75, linewidth=1.2, linestyle=":")
        # Served reads themselves.
        ax.scatter(times, read_pos, color=color, s=18, zorder=3, edgecolors="none")

    # Cursor-creation markers (stars). Other lifecycle events (switches) are not marked here --
    # a switch shows up as the read series continuing under a different cursor's colour.
    for event in trace.lifecycle:
        color = colors.get(event.cursor_id, "black")
        if event.kind == "created":
            ax.scatter(
                event.t,
                event.offset / divisor,
                marker="*",
                s=140,
                color=color,
                edgecolors="black",
                linewidths=0.5,
                zorder=5,
            )

    # Out-of-order reads (would be resets in the single-cursor world).
    if trace.out_of_order:
        ax.scatter(
            [o.t for o in trace.out_of_order],
            [o.actual / divisor for o in trace.out_of_order],
            marker="x",
            s=28,
            color="dimgray",
            alpha=0.5,
            zorder=2,
            linewidths=0.8,
        )

    # GetObject parts fetched from S3.
    if trace.getobject_parts:
        ax.scatter(
            [p.t for p in trace.getobject_parts],
            [p.offset / divisor for p in trace.getobject_parts],
            marker="_",
            s=40,
            color="black",
            alpha=0.25,
            zorder=1,
        )

    ax.set_xlabel("time since first prefetch event (s)")
    ax.set_ylabel(f"object offset ({y_unit})")
    heading = title or "Mountpoint prefetcher cursor activity"
    if trace.object_key:
        heading += f"  —  {trace.object_key}"
    ax.set_title(heading)
    ax.grid(True, alpha=0.2)

    _add_legend(ax, cursor_ids, colors)

    fig.tight_layout()
    fig.savefig(output, dpi=130)


def _add_legend(ax, cursor_ids: list[int], colors: dict[int, tuple]) -> None:
    handles: list[Line2D] = []
    for cid in cursor_ids:
        handles.append(
            Line2D([], [], color=colors[cid], marker="o", linestyle="", markersize=6, label=f"cursor {cid} read")
        )
    handles += [
        Line2D([], [], color="gray", linestyle="--", label="downloaded (available_offset)"),
        Line2D([], [], color="gray", linestyle=":", label="read window edge"),
        Line2D([], [], color="gray", marker="*", linestyle="", markersize=10, label="cursor created"),
        Line2D([], [], color="dimgray", marker="x", linestyle="", label="out-of-order read"),
        Line2D([], [], color="black", marker="_", linestyle="", label="GetObject part"),
    ]
    ax.legend(handles=handles, loc="upper left", fontsize=8, framealpha=0.9)
