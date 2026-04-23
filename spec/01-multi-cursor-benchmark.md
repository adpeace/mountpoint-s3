# Spec: Multi-Cursor Sequential Read Benchmark

## Goal

Provide a benchmark that reproduces the multi-cursor sequential read
pattern observed in the customer ticket (V1872576262), where an
application reads a single large object through multiple sequential
cursors interleaved through one file handle.  The benchmark measures
prefetcher throughput for this workload and provides a baseline for
evaluating future multi-cursor prefetching improvements.

## Background

The Mountpoint prefetcher assumes a single sequential cursor per file
handle.  When an application reads from multiple sequential positions
within the same file handle (e.g. reading 0.6 MiB at 0 GiB, then
0.6 MiB at 1 GiB, then 0.6 MiB at 2 GiB, cycling back), each jump
exceeds `max_forward_seek_wait_distance` (16 MiB).  The prefetcher
resets and issues a new GetObject request at each jump, discarding
any prefetched data.  This results in throughput close to random-read
levels despite each individual cursor being sequential.

The customer workload (eCAL ADAS simulation) exhibited this pattern
on a ~115 GiB `.rrec` file: Phase 1 read ~0.6 MiB at each 1 GiB
boundary before cycling, achieving low throughput.  Phase 2 read
1 GiB per cursor position sequentially and achieved ~425 MB/s.

## Requirements

### Integration with s3io benchmark

1. The benchmark MUST be implemented as a new `AccessPattern`
   variant in the existing `s3io_benchmark` example, not as a
   standalone binary.

2. The new access pattern MUST be named `multi_cursor_sequential`
   in the TOML config (mapping to an `AccessPattern` enum variant
   `MultiCursorSequential`).

3. Two new optional config fields MUST be added to `JobConfig`:
   a. `num_cursors` (default 8) -- number of concurrent sequential
      cursors to interleave.
   b. `bytes_per_cursor_visit` (default 1048576, i.e. 1 MiB) --
      how many bytes to read from each cursor before advancing to
      the next.

4. These fields MUST follow the existing config precedence:
   job-specific overrides global defaults overrides built-in
   defaults.

5. The executor MUST implement a new method
   `execute_multi_cursor_read` dispatched from `execute_read_job`
   when `access_pattern` is `MultiCursorSequential`.

### Read pattern

6. On startup the executor MUST call `HeadObject` to obtain the
   object's ETag and size (same as existing read paths).

7. The executor MUST divide the object into `num_cursors`
   equally-spaced cursor positions.  Cursor `i` starts at offset
   `i * (object_size / num_cursors)`.

8. The executor MUST round-robin through the cursors: read
   `bytes_per_cursor_visit` bytes from cursor 0, then cursor 1,
   ..., then cursor `num_cursors - 1`, then back to cursor 0.
   Each cursor advances sequentially within its region.

9. Each cursor's reads MUST be issued as sequential `read_size`
   calls to a single `PrefetchGetObject` instance (one per
   iteration), matching the real-world constraint that FUSE
   delivers all reads for a file handle to one
   `PrefetchGetObject`.

10. A cursor is finished when it has read all bytes up to the start
    of the next cursor (or end-of-object for the last cursor).
    Finished cursors are skipped in the round-robin.  The iteration
    ends when all cursors are finished.

### Output

11. The benchmark MUST return a `JobResult` with the same fields
    as existing read jobs (`total_bytes`, `elapsed_seconds`,
    `iterations_completed`, `errors`).

12. The benchmark MUST validate that the total bytes read per
    iteration equals the object size.

### Invariants

13. The benchmark MUST NOT use more than one `PrefetchGetObject`
    per iteration.  The point is to exercise the single-handle
    multi-cursor path.

14. The `max_duration` timeout MUST be respected, consistent with
    existing read job implementations.

## Out of Scope

- Modifying the prefetcher itself.  This spec covers only the
  benchmark program.
- Comparison with a single-cursor sequential baseline within the
  same run.  Users can define separate jobs in the TOML config
  for that.

## Appendix: Example TOML Config

```toml
[global]
bucket = "my-benchmark-bucket"
region = "us-west-2"
read_size = 131072

[jobs.sequential_baseline]
workload_type = "read"
object_key = "my-large-object"
object_size = 107374182400  # 100 GiB
access_pattern = "sequential"
generate_object = false
iterations = 3

[jobs.multi_cursor]
workload_type = "read"
object_key = "my-large-object"
object_size = 107374182400
access_pattern = "multi_cursor_sequential"
num_cursors = 8
bytes_per_cursor_visit = 1048576  # 1 MiB
generate_object = false
iterations = 3
```

Running both jobs in one config file directly quantifies the
multi-cursor penalty against the sequential baseline.
