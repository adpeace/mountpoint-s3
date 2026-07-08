# Prefetch cursor visualizer

Visualize Mountpoint prefetcher cursor activity from a debug/trace log file.
Produces a time (x) vs object-offset (y) chart showing, per cursor:

- where reads are served (coloured points, one colour per cursor),
- `available_offset` - how far data has actually been fetched, drawn as a dashed
  step line,
- the backpressure read-window edge (`read_window_end_offset` - how far
  ahead S3 has been asked to send), drawn as a dotted step line, with the band
  between the read position and the window edge shaded,
- cursor creation (★) and out-of-order reads (×, the events that would cause a
  reset in the single-cursor world),
- `GetObject` parts delivered from S3.

> Note: This curerntly only works when the trace contains reads for a single
> file-descriptor, otherwise the graph produced will not make sense.

This makes the multi-cursor behaviour legible: you can see several sequential
streams advancing in parallel, each with its own prefetch window, instead of one
cursor resetting on every position switch.

## Requirements

The prefetcher emits the trace events this tool relies on at `TRACE` level under
the `mountpoint_s3_fs::prefetch` target. Capture a log with trace logging
enabled:

```bash
# Start mount-s3 with trace logging for the prefetcher:
MOUNTPOINT_LOG="warn,mountpoint_s3_fs::prefetch=trace" \
  mount-s3 <BUCKET> <MNT> --log-directory /var/log/mountpoint

# Or from a prefetch unit test (no S3 or mount needed). Unit tests read the
# `RUST_LOG` env var; keep the target narrow so the log is prefetch-only:
RUST_LOG=mountpoint_s3_fs::prefetch=trace cargo test -p mountpoint-s3-fs --lib \
    prefetch::tests::multi_cursor::two_cursor_round_robin_reuses_cursors \
    -- --nocapture > trace.log 2>&1
```

The parser accepts both the ANSI-free log-directory files and colourised console
output.

## Usage

```bash
cd tools/prefetch-visualizer
uv sync

# Render a chart
uv run prefetch-visualizer /path/to/trace.log -o outputs/cursors.png

# Or just print a text summary of what was parsed
uv run prefetch-visualizer /path/to/trace.log --summary
```

Options:

- `-o/--output` — output image path (default `outputs/prefetch-cursors.png`).
- `--unit {MiB,bytes}` — y-axis unit (default `MiB`).
- `--title` — override the chart title.
- `--summary` — print parsed-event counts and exit without plotting.

## Development

```bash
uv run pytest
```
