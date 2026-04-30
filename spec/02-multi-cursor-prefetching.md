# Spec: Multi-Cursor Sequential Prefetching

## Goal

Enable the prefetcher to maintain multiple concurrent sequential read
cursors within a single file handle, so that applications interleaving
reads from several sequential positions (e.g. reading 1 MiB at 0 GiB,
then 1 MiB at 25 GiB, then 1 MiB at 50 GiB, cycling back) achieve
throughput close to a single sequential read rather than falling to
random-read levels.

## Background

The Mountpoint prefetcher maintains a single `RequestTask` per file
handle.  When a read arrives at an offset that doesn't match the
current sequential position, the prefetcher attempts a seek (forward
up to 16 MiB, backward up to 1 MiB).  If the seek fails, the
prefetcher resets: it drops the in-flight S3 request and spawns a new
one on the next read.

For workloads that interleave reads across N well-separated sequential
positions within one file handle, every cursor switch triggers a
reset.  Each reset pays the cost of a new GetObject request (time to
first byte ~50-100ms) and discards any prefetched data for the
previous cursor.  The result is throughput proportional to
`bytes_per_cursor_visit / (ttfb + bytes_per_cursor_visit / bandwidth)`
which for small visit sizes approaches random-read throughput.

The eCAL ADAS workload (V1872576262) exhibits this pattern on ~115 GiB
files with cursors spaced 1 GiB apart.

## Requirements

### Cursor tracking

1. The prefetcher MUST maintain up to `max_cursors` independent
   sequential cursors per file handle.  Each cursor owns its own
   `RequestTask` with independent backpressure state.

2. `max_cursors` MUST default to 8.  It MUST be configurable via
   `PrefetcherConfig` and exposed as a command-line option to the
   user.

3. When a read arrives at an offset that does not match the active
   cursor's expected sequential position, the prefetcher MUST search
   all tracked cursors for one that can serve the read.

4. A cursor can serve a read if the read offset falls within the
   cursor's seekable range: between
   `next_sequential_read_offset - max_backward_seek_distance` and
   `next_sequential_read_offset + max_forward_seek_wait_distance`.
   If multiple cursors are eligible, the prefetcher MUST select the
   one whose `next_sequential_read_offset` is nearest to the read
   offset.  This accommodates out-of-order reads issued by Linux
   readahead without misattributing reads to the wrong cursor.

5. If a matching cursor is found, the prefetcher MUST switch to that
   cursor (making it the active cursor) without resetting any
   `RequestTask`.  The normal seek logic (forward drain or backward
   seek window) then serves the read within that cursor.

### Cursor promotion and eviction

6. When a read arrives that does not match any tracked cursor, the
   prefetcher MUST create a new cursor at that offset.

7. If creating a new cursor would exceed `max_cursors`, the
   prefetcher MUST evict the least-recently-used cursor (the one
   whose last read timestamp is oldest).

8. A newly created cursor MUST spawn its `RequestTask` on the first
   read to that cursor (lazy spawning), consistent with existing
   behaviour.

### Seek behaviour within a cursor

9. Forward and backward seeks within a cursor MUST behave identically
   to the current single-cursor implementation: forward seeks up to
   `max_forward_seek_wait_distance` drain data from the task;
   backward seeks up to `max_backward_seek_distance` use the seek
   window.

10. If a seek within the active cursor fails (offset too far), the
    prefetcher MUST check other tracked cursors before creating a new
    one (per requirement 3).

### Cursor invalidation

11. When a cursor is evicted (requirement 7) or when the file handle
    is dropped, the cursor's `RequestTask` MUST be dropped, releasing
    its memory reservation and cancelling the in-flight S3 request.

12. A cursor that has reached end-of-object MUST NOT be eagerly
    removed.  Its backward seek window may still serve backward
    seeks.  It will be evicted through normal LRU eviction if not
    used again.

### Memory management

13. The total memory reserved across all cursors for a single file
    handle MUST NOT exceed `max_read_window_size`.  The read window
    budget is shared: each cursor's backpressure controller operates
    within a portion of the total budget.  When a new cursor is
    created, existing cursors' effective windows may shrink.

14. Each cursor MUST reserve its own backward seek window memory
    (as the current implementation does for the single cursor).

15. When memory pressure causes `try_reserve` to fail, the
    backpressure controller for the affected cursor MUST scale down
    its read window independently, without affecting other cursors.

### Non-regression: sequential read

16. For a purely sequential read workload (single cursor, no seeks),
    the prefetcher MUST NOT issue any additional S3 requests compared
    to the current implementation.  The cursor tracking overhead MUST
    be limited to a map lookup per read call.

17. The single-cursor sequential read path MUST NOT allocate memory
    for unused cursor slots.  Cursor state is allocated only when a
    second distinct cursor is detected.

### Non-regression: random read

18. For a random read workload (every read at a different offset with
    no repeating pattern), the prefetcher MUST NOT retain more than
    `max_cursors` `RequestTask` instances simultaneously.  Eviction
    (requirement 7) ensures bounded memory growth.

19. Random reads MUST NOT cause unbounded cursor creation.  Each
    random read that doesn't match an existing cursor evicts the LRU
    cursor, so steady-state memory usage is bounded by `max_cursors`
    cursors.

### Cursor identity and correctness

20. Each cursor MUST track its own `next_sequential_read_offset`
    independently.  A read served from cursor A MUST NOT advance
    cursor B's offset.

21. Data returned from a cursor MUST be validated against the
    object's identity (ObjectId) and the expected offset, as the
    current implementation does via `Part::into_bytes()`.

22. The backward seek window MUST be per-cursor.  A backward seek on
    cursor A MUST NOT consume data from cursor B's seek window.

23. Cursors whose seekable ranges overlap MUST NOT fetch the same
    data more than once.  Each cursor's `RequestTask` covers a
    distinct byte range starting from its own
    `next_sequential_read_offset`; the seek windows contain data
    already fetched by that cursor's own stream.

### Observability

24. The prefetcher MUST emit a metric `prefetch.cursor_switches`
    counting the number of times the active cursor changes (either
    to an existing tracked cursor or to a newly created one).

25. The existing `prefetch.out_of_order` metric MUST continue to be
    emitted when a read triggers cursor creation (not when switching
    to an existing cursor).

26. The prefetcher MUST emit a metric `prefetch.cursor_evictions`
    counting the number of times a cursor is evicted due to
    `max_cursors` being exceeded.

## Invariants

- At any point in time, at most `max_cursors` `RequestTask` instances
  exist per file handle.
- The sum of memory reserved by all cursors' backpressure controllers
  plus seek windows does not exceed `max_read_window_size` plus
  `max_cursors * seek_window_reservation`.
- Cursors with overlapping seekable ranges do not duplicate fetched
  data; each cursor's request stream covers only its own byte range.
- Dropping a `PrefetchGetObject` drops all cursor state and releases
  all memory reservations.

## Configuration

| Parameter | Default | Description |
|---|---|---|
| `max_cursors` | 8 | Maximum tracked cursors per file handle. Exposed as CLI option. |

## Out of Scope

- Predictive cursor creation (pre-spawning cursors at offsets the
  application hasn't read yet).
- Sharing cursors across file handles.
- Cursor-aware caching (the data cache operates independently).
- Adaptive `max_cursors` based on workload detection.

## Gaps

- With `max_cursors = 8` and the shared read window budget, individual
  cursors get a smaller effective read window than the single-cursor
  case.  This is acceptable because the memory limiter provides a
  global bound, and per-cursor scale-down (requirement 15) provides
  graceful degradation.  In practice, not all cursors will be actively
  prefetching simultaneously.
- Cursor matching uses the seekable range (requirement 4).  If two
  cursors have overlapping seekable ranges, the nearest-offset rule
  resolves ambiguity.  In pathological cases this could misattribute
  a read, but the data validation (requirement 21) ensures
  correctness — a misattributed read would fail validation and
  trigger a reset.

## Appendix A: Failure Modes

### A1. Cursor capture by readahead

**Scenario**: Linux readahead issues a read at offset X+128KiB while
the application is actually starting a new sequential stream at a
different offset Y.  If X+128KiB falls within an existing cursor's
seekable range, the readahead read is attributed to that cursor,
advancing it past data the application will actually read next.

**Mitigation**: The nearest-offset rule (requirement 4) minimises
misattribution by preferring the cursor closest to the read offset.
If misattribution does occur, `Part::into_bytes()` validation
(requirement 21) catches any data integrity issue.  The worst case
is a wasted forward seek within the wrong cursor, after which the
application's next read at Y creates a new cursor correctly.

**Residual risk**: A cursor that was incorrectly advanced by a
readahead read has now skipped data.  Subsequent reads to that
cursor's true sequential position will find the cursor has moved
past them, causing a backward seek or (if too far) a new cursor
creation.  This is a performance degradation, not a correctness
issue.

### A2. Cursor thrashing under near-random workloads

**Scenario**: An application reads at offsets that are just far
enough apart to exceed the seekable range but close enough that
new cursors are created and immediately evicted on the next read.
With `max_cursors = 8`, every 9th distinct offset evicts the LRU.

**Mitigation**: This is equivalent to the current single-cursor
behaviour (reset on every non-sequential read) but with up to 8
in-flight requests consuming memory.  The shared read window budget
(requirement 13) bounds total memory.  The `cursor_evictions` metric
(requirement 26) makes this pattern visible to operators.

**Residual risk**: Up to `max_cursors` initial requests (each
1 MiB + 128 KiB) may be in flight simultaneously for data that
will never be read.  This wastes bandwidth proportional to
`max_cursors * initial_request_size`.  For truly random workloads,
setting `max_cursors = 1` via CLI restores current behaviour.

### A3. Memory exhaustion from seek window reservations

**Scenario**: Each cursor reserves backward seek window memory
(1 MiB rounded up to part size).  With `max_cursors = 8` and
8 MiB part size, this is 8 * 8 MiB = 64 MiB reserved per file
handle just for seek windows, even if the cursors are idle.

**Mitigation**: The seek window reservation is made from the
global `MemoryLimiter`.  If the limiter is exhausted, new cursor
creation will still succeed (the reservation is best-effort for
the seek window), but the backpressure controller will scale down
read windows.  The total seek window cost is bounded and known
at configuration time:
`max_cursors * ceil(max_backward_seek_distance / part_size) * part_size`.

**Residual risk**: With many file handles open simultaneously
(e.g. hundreds), the aggregate seek window reservation could be
substantial.  This is the same risk as today (one seek window per
handle) scaled by `max_cursors`.  Operators can reduce
`max_cursors` if memory is constrained.

### A4. Stale cursor holding connection open

**Scenario**: A cursor is created, its `RequestTask` starts
fetching, but the application never returns to that cursor.  The
S3 connection remains open, consuming a CRT connection slot and
memory for buffered data, until the cursor is evicted by LRU.

**Mitigation**: The shared read window budget (requirement 13)
means the stale cursor's backpressure controller will eventually
block its stream (no more window increments since no reads are
consuming data).  The CRT connection will idle but not actively
transfer.  LRU eviction drops the cursor when `max_cursors` is
exceeded.

**Residual risk**: If the application uses fewer than `max_cursors`
distinct offsets, stale cursors persist until the file handle is
closed.  They hold idle connections but consume minimal bandwidth.

### A5. Misattribution corrupting a sequential stream

**Scenario**: Cursor A is at offset 100 MiB, cursor B is at
offset 116 MiB (within A's forward seek range of 16 MiB).  A read
arrives at 108 MiB.  The nearest-offset rule picks cursor A
(distance 8 MiB) over cursor B (distance 8 MiB — tie).

**Mitigation**: On a tie, a deterministic tiebreaker (e.g. lower
cursor ID or the currently active cursor) is used.  Regardless of
which cursor is chosen, the forward seek drains data and advances
that cursor.  If the read was actually intended for the other
cursor, the next read to that cursor will find it at the correct
offset (it was not modified).

**Residual risk**: The "wrong" cursor is advanced, wasting its
prefetched data.  The "right" cursor is unaffected.  This is a
performance issue (one cursor reset) not a correctness issue.

## Appendix B: Validation and Test Plan

### Unit tests (MockClient, in-process)

**B1. Sequential read unchanged**: Read an object sequentially
with `max_cursors = 8`.  Verify byte-for-byte correctness and
that only one `RequestTask` is ever created (no cursor switches,
no evictions).

**B2. Two-cursor round-robin**: Create a 10 MiB object.  Read
alternating 128 KiB chunks from offset 0 and offset 5 MiB.
Verify byte correctness at both positions.  Verify exactly 2
cursors are created (1 `out_of_order` event, 1 `cursor_switch`
per alternation after the first).

**B3. Cursor eviction**: Set `max_cursors = 2`.  Read from 3
distinct offsets (each beyond seekable range of the others).
Verify the LRU cursor is evicted, the eviction metric increments,
and subsequent reads to the evicted offset create a new cursor
with correct data.

**B4. Forward seek within cursor**: Two cursors at offsets 0 and
5 MiB.  Issue a read at offset 0 + 100 KiB (small forward seek
within cursor 0's range).  Verify cursor 0 serves it via forward
drain, not by creating a new cursor.

**B5. Backward seek within cursor**: Read 1 MiB from offset 0,
then read at offset 512 KiB (backward within seek window).
Verify data correctness from the seek window, no new cursor
created.

**B6. Readahead out-of-order**: Two cursors at 0 and 5 MiB.
Issue a read at offset 0 + 256 KiB (slightly ahead of cursor 0's
`next_sequential_read_offset` at 128 KiB).  Verify cursor 0 is
selected (nearest), forward-seeks to serve it, and cursor 1 is
unaffected.

**B7. Cursor not captured by distant read**: Cursor at offset 0
with `next_sequential_read_offset` = 1 MiB.  Issue read at
offset 50 MiB (beyond forward seek range).  Verify a new cursor
is created, the original cursor is not modified.

**B8. Random read memory bound**: Issue 100 random reads to
distinct offsets with `max_cursors = 4`.  Verify at most 4
`RequestTask` instances exist at any point (check via eviction
count: should be 96 evictions).

**B9. End-of-object cursor retained**: Read to end of object on
cursor A.  Then read backward within A's seek window.  Verify
data is served correctly from the seek window without creating a
new cursor.

**B10. Memory pressure scale-down**: Set a low memory limit.
Create multiple cursors.  Verify that `try_reserve` failures
cause individual cursor windows to shrink (via backpressure
controller) without crashing or affecting other cursors.

**B11. Drop releases all memory**: Create multiple cursors, then
drop the `PrefetchGetObject`.  Verify the `MemoryLimiter`'s
reserved count returns to zero.

### Proptest (randomised)

**B12. Random read correctness**: Randomise object size, read
offsets, read sizes, and `max_cursors`.  Verify every read
returns the correct bytes (same as existing `proptest_random_read`
but with multi-cursor enabled).

**B13. Multi-cursor sequential correctness**: Randomise number of
cursors (2-8), cursor spacing, bytes per visit, and read size.
Round-robin reads across cursors.  Verify total bytes read equals
object size and all data is correct.

### Integration tests (real S3, `e2e-tests` feature)

**B14. Multi-cursor throughput**: Run the `s3io_benchmark` with
`multi_cursor_sequential` pattern against a real 1 GiB object.
Verify throughput is within 50% of sequential baseline (current
behaviour gives ~10% of sequential for small visit sizes).

**B15. FUSE round-trip**: Mount a bucket, open a large file, and
issue interleaved `pread()` calls from multiple offsets in a
single file descriptor.  Verify all reads return correct data.

**B16. Memory stability**: Run multi-cursor workload for 60
seconds with memory monitoring.  Verify RSS does not grow
unboundedly (stays within `max_read_window_size` + seek window
reservations + baseline).
