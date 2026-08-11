//! End-to-end (FUSE) tests for multi-cursor sequential prefetching.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, OpenOptionsExt};

use mountpoint_s3_fs::metrics::defs::{
    PREFETCH_CURSOR_CREATED, PREFETCH_CURSOR_EVICTED, PREFETCH_CURSOR_SWITCHES, PREFETCH_RESET_STATE,
};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use rusty_fork::rusty_fork_test;
use test_case::test_case;

use crate::common::fuse::{self, TestSessionConfig, TestSessionCreator};
use crate::common::test_recorder::TestRecorder;

/// A pseudo-random object body. Random content means any misattributed read (data served from the
/// wrong cursor/offset) is caught by the byte comparison, unlike a constant fill.
fn random_body(size: usize) -> Vec<u8> {
    let mut rng = SmallRng::seed_from_u64(0x6d_75_6c_74_69); // "multi"
    let mut body = vec![0u8; size];
    rng.fill_bytes(&mut body);
    body
}

/// Read exactly `len` bytes at `offset` via `pread` on `fd` and assert they match `expected`.
fn assert_pread(fd: &File, expected: &[u8], offset: u64, len: usize) {
    let mut buf = vec![0u8; len];
    fd.read_exact_at(&mut buf, offset)
        .unwrap_or_else(|e| panic!("pread at offset {offset} len {len} failed: {e}"));
    let want = &expected[offset as usize..offset as usize + len];
    assert_eq!(buf, want, "wrong bytes at offset {offset} (len {len})");
}

const MB: usize = 1024 * 1024;

/// Put a random-bodied object of `object_size` on a session with `max_cursors`, then open it for
/// reading with `O_DIRECT`. Direct IO keeps the kernel page cache and readahead out of the path, so
/// the offsets the prefetcher sees are exactly the ones the test issues. Returns the open fd
/// alongside the object body so reads can be checked against it.
fn setup(
    creator_fn: impl TestSessionCreator,
    name: &str,
    object_size: usize,
    max_cursors: usize,
) -> (File, Vec<u8>) {
    let test_session = creator_fn(name, TestSessionConfig::default().with_max_cursors(max_cursors));
    let key = "object.bin";
    let body = random_body(object_size);
    test_session.client().put_object(key, &body).unwrap();
    let fd = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(test_session.mount_path().join(key))
        .unwrap();
    (fd, body)
}

/// Interleaved multi-cursor sequential reads through one file handle return correct data:
/// `num_cursors` regions are read round-robin, each advancing sequentially within its region.
fn multi_cursor_roundtrip(creator_fn: impl TestSessionCreator, object_size: usize, num_cursors: usize) {
    let (fd, body) = setup(creator_fn, "multi_cursor_roundtrip", object_size, num_cursors);

    // Divide the object into `num_cursors` equally-spaced regions, then round-robin through them,
    // reading `visit` bytes from each per round until every region is exhausted.
    let region = object_size / num_cursors;
    let visit = 128 * 1024;
    let read_size = 64 * 1024;
    let mut cursor_offsets: Vec<usize> = (0..num_cursors).map(|i| i * region).collect();
    let region_ends: Vec<usize> = (0..num_cursors)
        .map(|i| {
            if i == num_cursors - 1 {
                object_size
            } else {
                (i + 1) * region
            }
        })
        .collect();

    let mut remaining = num_cursors;
    while remaining > 0 {
        for i in 0..num_cursors {
            let end = region_ends[i];
            if cursor_offsets[i] >= end {
                continue;
            }
            let visit_end = (cursor_offsets[i] + visit).min(end);
            while cursor_offsets[i] < visit_end {
                let len = read_size.min(visit_end - cursor_offsets[i]);
                assert_pread(&fd, &body, cursor_offsets[i] as u64, len);
                cursor_offsets[i] += len;
                if cursor_offsets[i] >= end {
                    remaining -= 1;
                    break;
                }
            }
        }
    }
}

/// Non-regression: a purely sequential whole-file read with multi-cursor enabled returns correct
/// data through FUSE.
fn sequential_unchanged(creator_fn: impl TestSessionCreator, object_size: usize) {
    let (fd, body) = setup(creator_fn, "sequential_unchanged", object_size, 8);
    let read_size = 256 * 1024;
    let mut offset = 0usize;
    while offset < object_size {
        let len = read_size.min(object_size - offset);
        assert_pread(&fd, &body, offset as u64, len);
        offset += len;
    }
}

/// A distant read creates a second cursor; returning to the original region still reads correctly,
/// so the original cursor's stream is undisturbed.
fn distant_read(creator_fn: impl TestSessionCreator) {
    let (fd, body) = setup(creator_fn, "distant_read", 64 * MB, 8);

    // Cursor A near the start.
    assert_pread(&fd, &body, 0, 128 * 1024);
    assert_pread(&fd, &body, 128 * 1024, 128 * 1024);
    // Distant read -> cursor B.
    assert_pread(&fd, &body, 50 * MB as u64, 128 * 1024);
    assert_pread(&fd, &body, 50 * MB as u64 + 128 * 1024, 128 * 1024);
    // Back to A's next sequential position.
    assert_pread(&fd, &body, 256 * 1024, 128 * 1024);
    // And continue B.
    assert_pread(&fd, &body, 50 * MB as u64 + 256 * 1024, 128 * 1024);
}

// ---- MockClient cases (no credentials required) ----

#[test_case(16 * MB, 2; "2 cursors")]
#[test_case(64 * MB, 8; "8 cursors")]
fn multi_cursor_roundtrip_mock(object_size: usize, num_cursors: usize) {
    multi_cursor_roundtrip(fuse::mock_session::new, object_size, num_cursors);
}

#[test_case(32 * MB; "32MiB")]
fn sequential_unchanged_mock(object_size: usize) {
    sequential_unchanged(fuse::mock_session::new, object_size);
}

#[test]
fn distant_read_mock() {
    distant_read(fuse::mock_session::new);
}

// ---- Real-S3 cases (require the `s3_tests` feature + credentials) ----

#[cfg(feature = "s3_tests")]
#[test_case(16 * MB, 2; "2 cursors")]
#[test_case(64 * MB, 8; "8 cursors")]
fn multi_cursor_roundtrip_s3(object_size: usize, num_cursors: usize) {
    multi_cursor_roundtrip(fuse::s3_session::new, object_size, num_cursors);
}

#[cfg(feature = "s3_tests")]
#[test]
fn distant_read_s3() {
    distant_read(fuse::s3_session::new);
}

// ---- Cursor-lifecycle metric observability ----
//
// Wrapped in `rusty_fork_test!` because `metrics::set_global_recorder` succeeds only once per
// process; forking gives each test its own recorder.

fn setup_recorder() -> TestRecorder {
    let recorder = TestRecorder::default();
    metrics::set_global_recorder(recorder.clone()).expect("failed to set global recorder");
    recorder
}

fn counter(recorder: &TestRecorder, name: &str) -> u64 {
    recorder.get(name, &[]).map_or(0, |m| m.counter())
}

rusty_fork_test! {
    /// With multi-cursor enabled, Interleaved reads across two well-separated sequential positions
    /// through one file handle must, once both cursors exist, be served by switching between
    /// tracked cursors (`prefetch.cursor_switches`) rather than by creating a fresh cursor
    /// (`prefetch.cursor_created`) on every position change.
    #[test]
    fn cursor_switches_metric_mock() {
        let recorder = setup_recorder();
        let (fd, body) = setup(fuse::mock_session::new, "cursor_switches_metric", 32 * MB, 8);

        // Prime two cursors at offset 0 and offset 16 MiB.
        let pos_a = 0u64;
        let pos_b = 16 * MB as u64;
        let read = 128 * 1024;
        assert_pread(&fd, &body, pos_a, read);
        assert_pread(&fd, &body, pos_b, read);

        let created_after_priming = counter(&recorder, PREFETCH_CURSOR_CREATED);
        let switches_after_priming = counter(&recorder, PREFETCH_CURSOR_SWITCHES);

        // Now alternate between the two sequential streams for several rounds.
        let rounds = 8u64;
        let mut off_a = pos_a + read as u64;
        let mut off_b = pos_b + read as u64;
        for _ in 0..rounds {
            assert_pread(&fd, &body, off_a, read);
            assert_pread(&fd, &body, off_b, read);
            off_a += read as u64;
            off_b += read as u64;
        }

        let created_total = counter(&recorder, PREFETCH_CURSOR_CREATED);
        let switches_total = counter(&recorder, PREFETCH_CURSOR_SWITCHES);

        // The alternating rounds must be served by switching between the two existing cursors...
        let switches = switches_total - switches_after_priming;
        assert!(
            switches >= rounds,
            "expected at least {rounds} cursor switches over the alternating rounds, got {switches}"
        );
        // ...and must NOT create a new cursor on each switch
        let created = created_total - created_after_priming;
        assert!(
            created < rounds,
            "expected few/no new cursors during alternation, got {created} (looks like \
             create-per-switch)"
        );
    }

    /// A workload with more distinct distant streams than `max_cursors` must, once the cap is
    /// reached, evict a least-recently-used cursor to make room for each new one
    /// (`prefetch.cursor_evicted`). This is the "cap too small for the workload" signal that
    /// `prefetch.cursor_created` alone cannot distinguish from healthy growth: below the cap the
    /// same reads create cursors without any eviction.
    #[test]
    fn cursor_evicted_metric_mock() {
        let recorder = setup_recorder();

        // Object with room for many well-separated cursors. Each stream reads at a distinct 16 MiB
        // stride so it lands far enough from the others to require its own cursor.
        let max_cursors = 3;
        let (fd, body) = setup(fuse::mock_session::new, "cursor_evicted_metric", 96 * MB, max_cursors);

        let read = 128 * 1024;
        let stride = 16 * MB as u64;
        let starts: Vec<u64> = (0..6).map(|i| i * stride).collect();

        // Fill the cursor set up to the cap: the first `max_cursors` distant reads each create a
        // cursor, and none should evict (the set has room).
        for &start in &starts[..max_cursors] {
            assert_pread(&fd, &body, start, read);
        }
        let created_at_cap = counter(&recorder, PREFETCH_CURSOR_CREATED);
        assert_eq!(
            created_at_cap,
            max_cursors as u64,
            "each distant stream up to the cap should create its own cursor"
        );
        assert_eq!(
            counter(&recorder, PREFETCH_CURSOR_EVICTED),
            0,
            "no eviction should occur while the cursor set is below the cap"
        );

        // Each further distant stream needs a new cursor but the set is full, so each creation must
        // evict the least-recently-used cursor.
        let extra = &starts[max_cursors..];
        for &start in extra {
            assert_pread(&fd, &body, start, read);
        }

        let created = counter(&recorder, PREFETCH_CURSOR_CREATED) - created_at_cap;
        let evicted = counter(&recorder, PREFETCH_CURSOR_EVICTED);
        assert_eq!(
            created,
            extra.len() as u64,
            "each distant stream past the cap should create a cursor"
        );
        assert_eq!(
            evicted,
            extra.len() as u64,
            "each cursor created past the cap should evict the least-recently-used cursor"
        );
    }

    /// Small forward and backward seeks around a sequential position stay within a single cursor:
    /// the seeks fall inside the cursor's forward/backward seek windows, so no new cursor is created
    /// and no switch or reset occurs. Reads use `O_DIRECT` (via `setup`) so the offsets reaching the
    /// prefetcher are exactly those issued here, rather than reshaped by kernel readahead.
    #[test]
    fn small_seeks_metric_mock() {
        let recorder = setup_recorder();
        let (fd, body) = setup(fuse::mock_session::new, "small_seeks_metric", 16 * MB, 8);

        // Establish a sequential position (creates the single cursor).
        assert_pread(&fd, &body, 0, 128 * 1024);
        assert_pread(&fd, &body, 128 * 1024, 128 * 1024);
        // Small forward seek within the forward seek window.
        assert_pread(&fd, &body, 512 * 1024, 128 * 1024);
        // Small backward seek within the backward seek window.
        assert_pread(&fd, &body, 256 * 1024, 128 * 1024);
        // Resume forward.
        assert_pread(&fd, &body, 640 * 1024, 128 * 1024);

        assert_eq!(
            counter(&recorder, PREFETCH_CURSOR_CREATED),
            1,
            "the initial read creates one cursor and the seeks must stay on it"
        );
        assert_eq!(
            counter(&recorder, PREFETCH_CURSOR_SWITCHES),
            0,
            "seeks within one cursor must not switch to another cursor"
        );
        assert_eq!(
            counter(&recorder, PREFETCH_RESET_STATE),
            0,
            "seeks within the seek windows must not reset the prefetcher"
        );
    }
}
