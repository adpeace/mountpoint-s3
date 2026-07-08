//! This module implements a prefetcher for GetObject requests.
//!
//! It works by relying on the CRT's flow-control window feature. The prefetcher creates a single
//! GetObject request with entire length of the object (starting from the first read offset) and
//! makes increasingly larger read window. We want the chunks to be large enough that they can make
//! effective use of the CRT's fan-out parallelism across the S3 frontend, but small enough that we
//! don't accumulate a lot of unread object data in memory or wastefully download data we'll never
//! read. As the reader continues to make sequential reads, we increase the size of the read window
//! up to some maximum. If the reader ever makes a non-sequential read, we abandon the prefetching
//! and start again with a new GetObject request with minimum read window size.
//!
//! In more technical details, the prefetcher creates a Cursor when receiving the first read
//! request from the file system or after it has just been reset. The Cursor manages the read
//! position, backward seek window, and owns a RequestTask. The RequestTask consists of two main
//! components.
//! 1.  An ObjectPartStream that has a role to continuously fetch data from the sources which can be
//!     either S3 or the cache on disk. The ObjectPartStream is spawned and run in a separate thread
//!     from the prefetcher.
//! 2.  A PartQueue, where we store data received from the ObjectPartStream, waiting to be read from
//!     the prefetcher via a RequestTask function.
//!
//! A backpressure mechanism is needed to control how much data we want to store in the part queue at
//! a time as we don't want to download the entire object into memory. For the client part stream, we
//! may be able to rely on the CRT flow-control flow window to block when we don't increase the read
//! window size, but for the caching part stream we don't have the machinery to do that yet. That's why
//! we introduce the BackpressureController and BackpressureLimiter to help solving this.
//!
//! Essentially, the BackpressureController and BackpressureLimiter is a pair of sender/receiver of a
//! channel, created at RequestTask initialization. The sender is handed to the RequestTask. Its role
//! is to communicate with its receiver to tell "when" it is ready to receive more data. The receiver
//! is handed to the ObjectPartStream where the stream should call a provided function "before" fetching
//! more data from the sources and put them into the part queue. The BackpressureLimiter should be used
//! as a mean to block ObjectPartStream thread to fetch more data.

use std::fmt::Debug;

use mountpoint_s3_client::error::{GetObjectError, ObjectClientError};
use mountpoint_s3_client::{ObjectClient, error_metadata::ProvideErrorMetadata};
use thiserror::Error;
use tracing::trace;

use crate::checksums::{ChecksummedBytes, IntegrityError};
use crate::data_cache::DataCache;
use crate::fs::error_metadata::{ErrorMetadata, MOUNTPOINT_ERROR_CLIENT};
use crate::metrics::defs::{
    FUSE_CACHE_HIT, PREFETCH_CURSOR_CREATED, PREFETCH_CURSOR_EVICTED, PREFETCH_CURSOR_SWITCHES, PREFETCH_RESET_STATE,
};
use crate::object::ObjectId;

mod backpressure_controller;
mod builder;
mod caching_stream;
mod cursor;
mod part;
mod part_queue;
mod part_stream;
mod seek_window;
mod task;

pub use builder::PrefetcherBuilder;
use cursor::CursorRead;
pub use cursor::{Cursor, CursorId};
use part::PartOperationError;
use part_stream::{PartStream, RequestRange, RequestTaskConfig};

// This is a weird looking number! We really want our first request size to be 1MiB,
// which is a common IO size. But Linux's readahead will try to read an extra 128k on on
// top of a 1MiB read, which we'd have to wait for a second request to service. Because
// FUSE doesn't know the difference between regular reads and readahead reads, it will
// send us a READ request for that 128k, so we'll have to block waiting for it even if
// the application doesn't want it. This is all in the noise for sequential IO, but
// waiting for the readahead hurts random IO. So we add 128k to the first request size
// to avoid the latency hit of the second request.
pub const INITIAL_REQUEST_SIZE: usize = 1024 * 1024 + 128 * 1024;

/// Default maximum number of cursors.  Set to 1 to mimic pre-multi-cursor-support behaviour.
pub const DEFAULT_MAX_CURSORS: usize = 1;

#[derive(Debug, Error)]
pub enum PrefetchReadError<E> {
    #[error("get object request failed")]
    GetRequestFailed {
        source: ObjectClientError<GetObjectError, E>,
        metadata: Box<ErrorMetadata>,
    },

    #[error("get object request returned wrong offset")]
    GetRequestReturnedWrongOffset { offset: u64, expected_offset: u64 },

    #[error("get request terminated unexpectedly")]
    GetRequestTerminatedUnexpectedly,

    #[error("integrity check failed")]
    Integrity(#[from] IntegrityError),

    #[error("part read failed")]
    PartReadFailed(#[from] PartOperationError),

    #[error("backpressure must be enabled with non-zero initial read window")]
    BackpressurePreconditionFailed,

    #[error("read window increment failed")]
    ReadWindowIncrement,
}

impl<E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static> PrefetchReadError<E> {
    fn get_request_failed(err: ObjectClientError<GetObjectError, E>, bucket: &str, key: &str) -> Self {
        let metadata = ErrorMetadata {
            client_error_meta: err.meta(),
            error_code: Some(MOUNTPOINT_ERROR_CLIENT.to_string()),
            s3_bucket_name: Some(bucket.to_string()),
            s3_object_key: Some(key.to_string()),
        };
        let metadata = Box::new(metadata);
        Self::GetRequestFailed { source: err, metadata }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PrefetcherConfig {
    /// Maximum size of the read window
    pub max_read_window_size: usize,
    /// Factor to increase the request size by whenever the reader continues making sequential reads
    pub sequential_prefetch_multiplier: usize,
    /// The maximum amount of unavailable data the prefetcher will tolerate during a seek operation
    /// before resetting and starting a new S3 request.
    pub max_forward_seek_wait_distance: u64,
    /// The maximum distance the prefetcher will seek backwards before resetting and starting a new
    /// S3 request. We keep this much data in memory in addition to any inflight requests.
    pub max_backward_seek_distance: u64,
    /// Size of the initial request. This request, of size possibly smaller than part_size,
    /// is made to lower the latency in small-random-reads usage pattern. If set to 0, initial request
    /// is skipped.
    pub initial_request_size: usize,
    /// Maximum number of independent sequential read cursors tracked per file handle.
    pub max_cursors: usize,
}

impl Default for PrefetcherConfig {
    #[allow(clippy::identity_op)]
    fn default() -> Self {
        Self {
            max_read_window_size: determine_max_read_size(),
            sequential_prefetch_multiplier: 2,
            // We want these large enough to tolerate a single out-of-order Linux readahead, which
            // is at most 256KiB backwards and then 512KiB forwards. For forwards seeks, we're also
            // making a guess about where the optimal cut-off point is before it would be faster to
            // just start a new request instead.
            max_forward_seek_wait_distance: 16 * 1024 * 1024,
            max_backward_seek_distance: 1 * 1024 * 1024,
            initial_request_size: INITIAL_REQUEST_SIZE,
            max_cursors: DEFAULT_MAX_CURSORS,
        }
    }
}

/// Provide the maximum read size for the prefetcher, for which there is one prefetcher per file handle.
///
/// This allows a way to override the prefetch window rather than using the hardcoded default within Mountpoint.
/// We do not recommend using the override, and it may be removed at any time.
///
/// This parameter may not be accurately adopted when using small values.
/// When prefetching starts, it will fetch 1MiB + 128KiB at time of writing.
/// This parameter will only be used when scaling up the prefetch window.
///
/// This unstable override is expected to be removed once adaptive prefetching based on available memory is available:
/// https://github.com/awslabs/mountpoint-s3/issues/987
fn determine_max_read_size() -> usize {
    const ENV_VAR_KEY: &str = "UNSTABLE_MOUNTPOINT_MAX_PREFETCH_WINDOW_SIZE";
    const DEFAULT_READ_WINDOW_SIZE: usize = 2 * 1024 * 1024 * 1024;

    match std::env::var_os(ENV_VAR_KEY) {
        Some(val) => match val.to_string_lossy().parse() {
            Ok(val) => {
                tracing::warn!(
                    "successfully overridden prefetch read window size \
                        with new value {val} bytes from unstable environment config",
                );
                val
            }
            Err(_) => {
                tracing::warn!(
                    "{ENV_VAR_KEY} did not contain a valid positive integer \
                        for prefetch bytes, using {DEFAULT_READ_WINDOW_SIZE} bytes instead",
                );
                DEFAULT_READ_WINDOW_SIZE
            }
        },
        None => DEFAULT_READ_WINDOW_SIZE,
    }
}

/// A [Prefetcher] creates and manages prefetching GetObject requests to objects.
#[derive(Debug)]
pub struct Prefetcher<Client> {
    part_stream: PartStream<Client>,
    config: PrefetcherConfig,
}

impl<Client> Prefetcher<Client>
where
    Client: ObjectClient + Clone + Send + Sync + 'static,
{
    /// Creates an instance of the default [Prefetcher] builder.
    pub fn default_builder(client: Client) -> PrefetcherBuilder<Client> {
        PrefetcherBuilder::default_builder(client)
    }

    /// Creates an instance of a caching [Prefetcher] builder.
    pub fn caching_builder<Cache>(cache: Cache, client: Client) -> PrefetcherBuilder<Client>
    where
        Cache: DataCache + Send + Sync + 'static,
    {
        PrefetcherBuilder::caching_builder(cache, client)
    }

    /// Create a new [Prefetcher] from the given [ObjectPartStream] instance.
    pub fn new(part_stream: PartStream<Client>, config: PrefetcherConfig) -> Self {
        Self { part_stream, config }
    }

    /// Start a new prefetch request to the specified object.
    pub fn prefetch(&self, bucket: String, object_id: ObjectId, size: u64) -> PrefetchGetObject<Client>
    where
        Client: ObjectClient + Clone + Send + Sync + 'static,
    {
        PrefetchGetObject::new(self.part_stream.clone(), self.config, bucket, object_id, size)
    }
}

/// Result of a prefetch request. Allows callers to read object data.
#[derive(Debug)]
pub struct PrefetchGetObject<Client>
where
    Client: ObjectClient + Clone + Send + Sync + 'static,
{
    part_stream: PartStream<Client>,
    config: PrefetcherConfig,
    bucket: String,
    object_id: ObjectId,
    size: u64,
    /// Tracked sequential read cursors for this handle, ordered least- to
    /// most-recently-used: the last element is the active cursor and index 0 is
    /// the LRU (first to be dropped when the limit is exceeded). Empty until
    /// the first read.
    cursors: Vec<Cursor<Client>>,
    // preferred part size in the prefetcher's part queue, not the object part
    preferred_part_size: usize,
}

impl<Client> PrefetchGetObject<Client>
where
    Client: ObjectClient + Clone + Send + Sync + 'static,
{
    /// Create and spawn a new prefetching request for an object
    fn new(
        part_stream: PartStream<Client>,
        config: PrefetcherConfig,
        bucket: String,
        object_id: ObjectId,
        size: u64,
    ) -> Self {
        PrefetchGetObject {
            part_stream,
            config,
            bucket,
            object_id,
            size,
            cursors: Vec::new(),
            preferred_part_size: 128 * 1024,
        }
    }

    /// Read some bytes from the object. This function will always return exactly `size` bytes,
    /// except at the end of the object where it will return however many bytes are left (including
    /// possibly 0 bytes).
    pub async fn read(
        &mut self,
        offset: u64,
        length: usize,
    ) -> Result<ChecksummedBytes, PrefetchReadError<Client::ClientError>> {
        trace!(offset, length, "read");

        let remaining = self.size.saturating_sub(offset);
        if remaining == 0 {
            return Ok(ChecksummedBytes::default());
        }

        match self.try_read(offset, length).await {
            Ok((data, cache_hit)) => {
                // Record cache hit metric for FUSE layer. We only record a cache hit when ALL parts
                // for this read request were served from cache storage (disk/express), not from S3.
                // Partial cache hits (some parts from cache, some from S3) are counted as cache misses
                // to provide a clear binary metric. Parts served from in-memory prefetch buffers
                // (from previous S3 requests) don't count as cache hits.

                if !data.is_empty() && cache_hit {
                    // We only increment the counter on cache_hit=true because OTLP counters don't preserve
                    // data point counts needed for meaningful averages.
                    // FIXME: Consider using histogram to capture partial hit ratios.
                    metrics::counter!(FUSE_CACHE_HIT).increment(1);
                }
                Ok(data)
            }
            Err(err) => {
                self.reset_prefetch();
                Err(err)
            }
        }
    }

    async fn try_read(
        &mut self,
        offset: u64,
        length: usize,
    ) -> Result<(ChecksummedBytes, bool), PrefetchReadError<Client::ClientError>> {
        // Currently, we set preferred part size to the current read size.
        // Our assumption is that the read size will be the same for most sequential
        // read and it can be aligned to the size of prefetched chunks.
        //
        // We initialize this value to 128k as it is the Linux's readahead size
        // and it can also be used as a lower bound in case the read size is too small.
        // The upper bound is 1MiB since it should be a common IO size.
        let max_preferred_part_size = 1024 * 1024;
        self.preferred_part_size = self.preferred_part_size.max(length).min(max_preferred_part_size);

        // Try the MRU cursor first to avoid a sequential read on it being captured by another
        // cursor with an overlapping read window.
        if let Some(active) = self.cursors.last()
            && let Some(read) = active.try_read(offset, length).await?
        {
            // If the cursor reached its bound, recompute cursor bounds so that it may continue
            // forwards if the next cursor has moved on.
            if read.bound_reached {
                self.recompute_cursor_bounds().await;
            }
            return Ok((read.data, read.from_cache));
        }

        // The active cursor couldn't serve the read: check if any other cursor can, and select the
        // one closest to the read position. Note that the cursors might mutate between the check and
        // the try_read attempt, so even if a cursor is selected it may become ineligible and we fall
        // through to cursor creation anyway.
        if let Some(index) = self.select_cursor(offset).await
            && let Some(read) = self.cursors[index].try_read(offset, length).await?
        {
            // We switched to a non-active cursor (`select_cursor` never returns the active cursor):
            // make it the active (most-recently-used) cursor by moving it to the end.
            metrics::counter!(PREFETCH_CURSOR_SWITCHES).increment(1);
            let cursor = self.cursors.remove(index);
            trace!(
                cursor_id = cursor.cursor_id().as_raw(),
                key = self.object_id.key(),
                offset,
                "switched active cursor"
            );
            self.cursors.push(cursor);
            // See above: a read past its bound means the bound is stale, so recompute.
            if read.bound_reached {
                self.recompute_cursor_bounds().await;
            }
            return Ok((read.data, read.from_cache));
        }

        // No tracked cursor can serve the read: create a new one at `offset`, starting a new S3
        // request.
        let (cursor, read) = self.read_from_new_cursor(offset, length).await?;
        metrics::counter!(PREFETCH_CURSOR_CREATED).increment(1);
        trace!(
            cursor_id = cursor.cursor_id().as_raw(),
            key = self.object_id.key(),
            offset,
            tracked_cursors = self.cursors.len() + 1,
            "created new cursor"
        );
        self.push_cursor(cursor);
        // The tracked set changed (a cursor was added, possibly one evicted), so recompute each
        // cursor's forward window bound to keep it from fetching into the next cursor's region.
        self.recompute_cursor_bounds().await;
        Ok((read.data, read.from_cache))
    }

    /// Bound each tracked cursor's read-window growth at the current read position of the nearest
    /// cursor ahead of it, so no cursor fetches into another cursor's region (which causes
    /// over-fetch and read "capture"). A cursor with no other cursor ahead of it (the frontmost, and
    /// the single-cursor case) is left unbounded, so sequential reads are unaffected.
    ///
    /// Called on tracked-set changes only. Between changes cursors only advance forward, which
    /// merely relaxes the correct bound, so a slightly stale bound is always conservative (it never
    /// permits over-fetch, only limits growth a little).
    async fn recompute_cursor_bounds(&self) {
        // Snapshot each cursor's current read position (skips any reset by the memory limiter).
        let mut offsets = Vec::with_capacity(self.cursors.len());
        for cursor in &self.cursors {
            offsets.push(cursor.current_offset().await);
        }
        for (i, cursor) in self.cursors.iter().enumerate() {
            cursor.set_window_upper_bound(nearest_ahead_bound(i, &offsets)).await;
        }
    }

    /// Find the index of the tracked cursor best able to serve a read at `offset`, if any. Among
    /// eligible cursors, the one whose read position is nearest to `offset` wins; ties break towards
    /// the more-recently-used cursor (higher index). The active cursor (last) is skipped because the
    /// caller already tried it on the fast path.
    async fn select_cursor(&self, offset: u64) -> Option<usize> {
        let active = self.cursors.len().checked_sub(1)?;
        let mut best: Option<(usize, u64)> = None;
        for index in 0..active {
            if let Some(current_offset) = self.cursors[index].can_serve(offset).await {
                let distance = current_offset.abs_diff(offset);
                // `<=` so that on a tie we prefer the higher (more-recently-used) index.
                if best.is_none_or(|(_, best_distance)| distance <= best_distance) {
                    best = Some((index, distance));
                }
            }
        }
        best.map(|(index, _)| index)
    }

    /// Track a newly created cursor as the active (most-recently-used) cursor, dropping the least-
    /// recently-used cursor first if we reached `max_cursors`.
    fn push_cursor(&mut self, cursor: Cursor<Client>) {
        debug_assert!(self.config.max_cursors >= 1, "max_cursors must be >= 1");
        if self.cursors.len() >= self.config.max_cursors.max(1) {
            metrics::counter!(PREFETCH_CURSOR_EVICTED).increment(1);
            let evicted = self.cursors.remove(0);
            trace!(
                cursor_id = evicted.cursor_id().as_raw(),
                key = self.object_id.key(),
                max_cursors = self.config.max_cursors,
                "evicted least-recently-used cursor at cap"
            );
        }
        self.cursors.push(cursor);
    }

    /// Create a new [`Cursor`] and start reading from it.
    /// The new cursor starts a backpressure GetObject request with a range from current offset
    /// to the end of the file.
    async fn read_from_new_cursor(
        &self,
        offset: u64,
        initial_read_length: usize,
    ) -> Result<(Cursor<Client>, CursorRead), PrefetchReadError<Client::ClientError>> {
        let object_size = self.size as usize;
        let client = self.part_stream.client();

        // Validate backpressure preconditions: client must have backpressure enabled
        // with an initial read window size greater than 0.
        match client.initial_read_window_size() {
            Some(0) | None => return Err(PrefetchReadError::BackpressurePreconditionFailed),
            Some(_) => {}
        }

        let pool = self.part_stream.pool();
        let cursor_handle = pool.create_cursor();
        let task_config = RequestTaskConfig {
            cursor_state: cursor_handle.state(),
            bucket: self.bucket.clone(),
            object_id: self.object_id.clone(),
            range: RequestRange::new(object_size, offset, object_size),
            read_part_size: client.read_part_size(),
            preferred_part_size: self.preferred_part_size,
            initial_request_size: self.config.initial_request_size,
            max_read_window_size: self.config.max_read_window_size,
            read_window_size_multiplier: self.config.sequential_prefetch_multiplier,
        };
        let request_task = self.part_stream.spawn_request_task(task_config);
        Cursor::new_and_read(
            request_task,
            cursor_handle,
            &self.config,
            self.object_id.clone(),
            offset,
            initial_read_length,
        )
        .await
    }

    /// Discard all prefetch state for this handle, dropping every tracked cursor (cancelling their
    /// in-flight requests and releasing their prefetched data) so the next read starts from a clean
    /// slate. Called after a read error. This is the hard reset the `prefetch.reset_state` metric
    /// counts, distinct from creating an additional cursor, which is normal multi-cursor growth
    /// (`prefetch.cursor_created`).
    fn reset_prefetch(&mut self) {
        metrics::counter!(PREFETCH_RESET_STATE).increment(1);
        self.cursors.clear();
    }

    /// Number of cursors currently tracked by this handle. Test-only introspection used to assert
    /// that multi-cursor paths are actually exercised and that the `max_cursors` cap holds.
    #[cfg(test)]
    fn tracked_cursors(&self) -> usize {
        self.cursors.len()
    }

    /// The active (most-recently-used) cursor's id, or `None` if no cursor exists yet. Test-only
    /// introspection: sampling this right after a read on a given stream reveals whether that
    /// stream's cursor is the same one across reads (stable id) or has been evicted and recreated
    /// (id changes to a strictly larger, never-reused value).
    #[cfg(test)]
    fn active_cursor_id(&self) -> Option<CursorId> {
        self.cursors.last().map(|cursor| cursor.cursor_id())
    }

    /// Per-cursor `(current_offset, window_upper_bound)` for every live cursor, in tracking order.
    /// Test-only introspection used to assert that per-cursor window bounds are recomputed and
    /// relaxed as cursors advance and the tracked set changes.
    #[cfg(test)]
    async fn cursor_bounds_snapshot(&self) -> Vec<(u64, Option<u64>)> {
        let mut snapshot = Vec::with_capacity(self.cursors.len());
        for cursor in &self.cursors {
            if let Some(entry) = cursor.current_offset_and_bound().await {
                snapshot.push(entry);
            }
        }
        snapshot
    }
}

/// The window upper bound for the cursor at `index`: the position of the nearest cursor ahead of
/// it, or `None` if none is ahead (the frontmost cursor, and the single-cursor case, are left
/// unbounded). `offsets` holds each tracked cursor's current read position in tracking order, with
/// `None` for a cursor reset by the memory limiter.
///
/// A cursor counts as "ahead" of the one at `index` if its position is strictly greater, or equal
/// but tracked later (higher index). The tie-break matters: among cursors sitting at the same
/// offset, exactly one (the most-recently-used, highest index) is left unbounded and the rest are
/// bounded at that shared offset. Without it, two cursors at the same offset would each see the
/// other as "not ahead", both grow a read window over the identical region, and fetch it twice.
fn nearest_ahead_bound(index: usize, offsets: &[Option<u64>]) -> Option<u64> {
    let own = offsets[index]?;
    offsets
        .iter()
        .enumerate()
        .filter_map(|(other, offset)| offset.map(|offset| (other, offset)))
        .filter(|&(other, offset)| offset > own || (offset == own && other > index))
        .map(|(_, offset)| offset)
        .min()
}

#[cfg(test)]
mod tests {
    // It's convenient to write test constants like "1 * 1024 * 1024" for symmetry
    #![allow(clippy::identity_op)]

    use std::collections::HashMap;

    use futures::executor::{ThreadPool, block_on};
    use mountpoint_s3_client::failure_client::{
        CountdownFailureConfig, GetObjectFailureMode, countdown_failure_client,
    };
    use mountpoint_s3_client::mock_client::{MockClient, MockClientConfig, MockClientError, MockObject, ramp_bytes};
    use mountpoint_s3_client::types::ETag;
    use proptest::proptest;
    use proptest::strategy::{Just, Strategy};
    use proptest_derive::Arbitrary;
    use test_case::test_case;

    use crate::Runtime;
    use crate::data_cache::InMemoryDataCache;
    use crate::memory::{CandidateSize, PagedPool};
    use crate::sync::Arc;

    use super::*;

    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;

    #[derive(Debug, Arbitrary)]
    struct TestConfig {
        #[proptest(strategy = "16usize..1*1024*1024")]
        initial_request_size: usize,
        #[proptest(strategy = "16usize..1*1024*1024")]
        max_read_window_size: usize,
        #[proptest(strategy = "1usize..8usize")]
        sequential_prefetch_multiplier: usize,
        #[proptest(strategy = "16usize..2*1024*1024")]
        client_part_size: usize,
        #[proptest(strategy = "1u64..4*1024*1024")]
        max_forward_seek_wait_distance: u64,
        #[proptest(strategy = "1u64..4*1024*1024")]
        max_backward_seek_distance: u64,
        #[proptest(strategy = "16usize..1*1024*1024")]
        cache_block_size: usize,
        #[proptest(strategy = "1usize..8usize")]
        max_cursors: usize,
    }

    enum PrefetcherType {
        Default,
        InMemoryCache(usize),
    }

    fn build_prefetcher<Client>(
        client: Client,
        prefetcher_type: PrefetcherType,
        prefetcher_config: PrefetcherConfig,
    ) -> Prefetcher<Client>
    where
        Client: ObjectClient + Clone + Send + Sync + 'static,
    {
        let pool = PagedPool::config()
            .with_candidate_sizes([
                CandidateSize::new(client.read_part_size()),
                CandidateSize::new(client.write_part_size()),
            ])
            .with_minimum_memory_limit()
            .build();
        let runtime = Runtime::new(ThreadPool::builder().pool_size(1).create().unwrap());
        let builder = match prefetcher_type {
            PrefetcherType::Default => Prefetcher::default_builder(client),
            PrefetcherType::InMemoryCache(block_size) => {
                let cache = InMemoryDataCache::new(block_size as u64);
                Prefetcher::caching_builder(cache, client)
            }
        };
        builder.build(runtime, pool, prefetcher_config)
    }

    fn run_sequential_read_test(prefetcher_type: PrefetcherType, size: u64, read_size: usize, test_config: TestConfig) {
        let client = Arc::new(
            MockClient::config()
                .bucket("test-bucket")
                .part_size(test_config.client_part_size)
                .enable_backpressure(true)
                .initial_read_window_size(test_config.client_part_size)
                .build(),
        );
        let object = MockObject::ramp(0xaa, size as usize, ETag::for_tests());
        let etag = object.etag();

        client.add_object("hello", object);

        let prefetcher_config = PrefetcherConfig {
            max_read_window_size: test_config.max_read_window_size,
            sequential_prefetch_multiplier: test_config.sequential_prefetch_multiplier,
            max_forward_seek_wait_distance: test_config.max_forward_seek_wait_distance,
            max_backward_seek_distance: test_config.max_backward_seek_distance,
            initial_request_size: test_config.initial_request_size,
            max_cursors: test_config.max_cursors,
        };

        let prefetcher = build_prefetcher(client.clone(), prefetcher_type, prefetcher_config);
        let object_id = ObjectId::new("hello".to_owned(), etag);
        let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, size);

        let mut next_offset = 0;
        loop {
            let buf = block_on(request.read(next_offset, read_size)).unwrap();
            if buf.is_empty() {
                break;
            }
            let buf = buf.into_bytes().unwrap();
            let expected = ramp_bytes((0xaa + next_offset) as usize, buf.len());
            assert_eq!(&buf[..], &expected[..buf.len()]);
            next_offset += buf.len() as u64;
        }
        assert_eq!(next_offset, size);
    }

    #[test_case(PrefetcherType::Default)]
    #[test_case(PrefetcherType::InMemoryCache(1 * MB))]
    fn sequential_read_small(prefetcher_type: PrefetcherType) {
        let config = TestConfig {
            initial_request_size: 256 * 1024,
            max_read_window_size: 1024 * 1024 * 1024,
            sequential_prefetch_multiplier: 8,
            client_part_size: 8 * 1024 * 1024,
            max_forward_seek_wait_distance: 16 * 1024 * 1024,
            max_backward_seek_distance: 2 * 1024 * 1024,
            cache_block_size: 1 * MB,
            max_cursors: DEFAULT_MAX_CURSORS,
        };
        run_sequential_read_test(prefetcher_type, 1024 * 1024 + 111, 1024 * 1024, config);
    }

    #[test_case(PrefetcherType::Default)]
    #[test_case(PrefetcherType::InMemoryCache(1 * MB))]
    fn sequential_read_medium(prefetcher_type: PrefetcherType) {
        let config = TestConfig {
            initial_request_size: 256 * 1024,
            max_read_window_size: 64 * 1024 * 1024,
            sequential_prefetch_multiplier: 8,
            client_part_size: 8 * 1024 * 1024,
            max_forward_seek_wait_distance: 16 * 1024 * 1024,
            max_backward_seek_distance: 2 * 1024 * 1024,
            cache_block_size: 1 * MB,
            max_cursors: DEFAULT_MAX_CURSORS,
        };
        run_sequential_read_test(prefetcher_type, 16 * 1024 * 1024 + 111, 1024 * 1024, config);
    }

    #[test_case(PrefetcherType::Default)]
    #[test_case(PrefetcherType::InMemoryCache(1 * MB))]
    fn sequential_read_large(prefetcher_type: PrefetcherType) {
        let config = TestConfig {
            initial_request_size: 256 * 1024,
            max_read_window_size: 64 * 1024 * 1024,
            sequential_prefetch_multiplier: 8,
            client_part_size: 8 * 1024 * 1024,
            max_forward_seek_wait_distance: 16 * 1024 * 1024,
            max_backward_seek_distance: 2 * 1024 * 1024,
            cache_block_size: 1 * MB,
            max_cursors: DEFAULT_MAX_CURSORS,
        };

        run_sequential_read_test(prefetcher_type, 256 * 1024 * 1024 + 111, 1024 * 1024, config);
    }

    /// Each read is larger than a part, so `do_read` must stitch multiple parts together (the
    /// multi-part path that copies the first part off its pool buffer). Guards read correctness
    /// across that path.
    #[test_case(PrefetcherType::Default)]
    #[test_case(PrefetcherType::InMemoryCache(1 * MB))]
    fn sequential_read_size_larger_than_part(prefetcher_type: PrefetcherType) {
        let config = TestConfig {
            initial_request_size: 256 * 1024,
            max_read_window_size: 64 * 1024 * 1024,
            sequential_prefetch_multiplier: 8,
            client_part_size: 1 * 1024 * 1024,
            max_forward_seek_wait_distance: 16 * 1024 * 1024,
            max_backward_seek_distance: 2 * 1024 * 1024,
            cache_block_size: 1 * MB,
            max_cursors: DEFAULT_MAX_CURSORS,
        };

        // read_size (4 MiB + 111) > client_part_size (1 MiB), and the odd tail crosses part
        // boundaries so reads straddle rather than align.
        run_sequential_read_test(prefetcher_type, 16 * 1024 * 1024 + 111, 4 * 1024 * 1024 + 111, config);
    }

    fn fail_with_backpressure_precondition_test(
        prefetcher_type: PrefetcherType,
        test_config: TestConfig,
        client_config: MockClientConfig,
    ) {
        let client = Arc::new(MockClient::new(client_config));
        let read_size = 1 * MB;
        let object_size = 8 * MB;
        let object = MockObject::ramp(0xaa, object_size, ETag::for_tests());
        let etag = object.etag();

        let prefetcher_config = PrefetcherConfig {
            max_read_window_size: test_config.max_read_window_size,
            sequential_prefetch_multiplier: test_config.sequential_prefetch_multiplier,
            ..Default::default()
        };

        let prefetcher = build_prefetcher(client, prefetcher_type, prefetcher_config);
        let object_id = ObjectId::new("hello".to_owned(), etag);
        let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, object_size as u64);
        let result = block_on(request.read(0, read_size));
        assert!(matches!(result, Err(PrefetchReadError::BackpressurePreconditionFailed)));
    }

    #[test_case(PrefetcherType::Default)]
    #[test_case(PrefetcherType::InMemoryCache(1 * MB))]
    fn fail_with_backpressure_not_enabled(prefetcher_type: PrefetcherType) {
        let test_config = TestConfig {
            initial_request_size: 256 * 1024,
            max_read_window_size: 1024 * 1024 * 1024,
            sequential_prefetch_multiplier: 8,
            client_part_size: 8 * 1024 * 1024,
            max_forward_seek_wait_distance: 16 * 1024 * 1024,
            max_backward_seek_distance: 2 * 1024 * 1024,
            cache_block_size: 1 * MB,
            max_cursors: DEFAULT_MAX_CURSORS,
        };

        // backpressure is not enabled for the client
        let config = MockClient::config()
            .bucket("test-bucket")
            .part_size(test_config.client_part_size)
            .enable_backpressure(false);

        fail_with_backpressure_precondition_test(prefetcher_type, test_config, config);
    }

    #[test_case(PrefetcherType::Default)]
    #[test_case(PrefetcherType::InMemoryCache(1 * MB))]
    fn fail_with_backpressure_zero_read_window(prefetcher_type: PrefetcherType) {
        let test_config = TestConfig {
            initial_request_size: 256 * 1024,
            max_read_window_size: 1024 * 1024 * 1024,
            sequential_prefetch_multiplier: 8,
            client_part_size: 8 * 1024 * 1024,
            max_forward_seek_wait_distance: 16 * 1024 * 1024,
            max_backward_seek_distance: 2 * 1024 * 1024,
            cache_block_size: 1 * MB,
            max_cursors: DEFAULT_MAX_CURSORS,
        };

        // backpressure is enabled but initial read window size is zero
        let config = MockClient::config()
            .bucket("test-bucket")
            .part_size(test_config.client_part_size)
            .enable_backpressure(true)
            .initial_read_window_size(0);

        fail_with_backpressure_precondition_test(prefetcher_type, test_config, config);
    }

    fn fail_sequential_read_test(
        prefetcher_type: PrefetcherType,
        size: u64,
        read_size: usize,
        test_config: TestConfig,
        get_failures: HashMap<usize, GetObjectFailureMode<MockClientError>>,
    ) {
        let client = MockClient::config()
            .bucket("test-bucket")
            .part_size(test_config.client_part_size)
            .enable_backpressure(true)
            .initial_read_window_size(test_config.client_part_size)
            .build();
        let object = MockObject::ramp(0xaa, size as usize, ETag::for_tests());
        let etag = object.etag();

        client.add_object("hello", object);

        let client = Arc::new(countdown_failure_client(
            client,
            CountdownFailureConfig {
                get_failures,
                ..Default::default()
            },
        ));

        let prefetcher_config = PrefetcherConfig {
            max_read_window_size: test_config.max_read_window_size,
            sequential_prefetch_multiplier: test_config.sequential_prefetch_multiplier,
            initial_request_size: test_config.initial_request_size,
            ..Default::default()
        };

        let prefetcher = build_prefetcher(client, prefetcher_type, prefetcher_config);
        let object_id = ObjectId::new("hello".to_owned(), etag);
        let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, size);

        let mut next_offset = 0;
        loop {
            let buf = match block_on(request.read(next_offset, read_size)) {
                Ok(buf) => buf,
                Err(_) => break,
            };
            let buf = buf.into_bytes().unwrap();

            if buf.is_empty() {
                break;
            }
            let expected = ramp_bytes((0xaa + next_offset) as usize, buf.len());
            assert_eq!(&buf[..], &expected[..buf.len()]);
            next_offset += buf.len() as u64;
        }
        assert!(next_offset < size); // Since we're injecting failures, shouldn't make it to the end
    }

    #[test_case("invalid range; length=42", PrefetcherType::Default)]
    #[test_case("invalid range; length=42", PrefetcherType::InMemoryCache(1 * MB))]
    // test case for the request failure due to etag not matching
    #[test_case(
        "At least one of the pre-conditions you specified did not hold",
        PrefetcherType::Default
    )]
    #[test_case("At least one of the pre-conditions you specified did not hold", PrefetcherType::InMemoryCache(1 * MB))]
    fn fail_request_sequential_small(err_value: &str, prefetcher_type: PrefetcherType) {
        let config = TestConfig {
            initial_request_size: 256 * 1024,
            max_read_window_size: 1024 * 1024 * 1024,
            sequential_prefetch_multiplier: 8,
            client_part_size: 8 * 1024 * 1024,
            max_forward_seek_wait_distance: 16 * 1024 * 1024,
            max_backward_seek_distance: 2 * 1024 * 1024,
            cache_block_size: 1 * MB,
            max_cursors: DEFAULT_MAX_CURSORS,
        };

        let mut get_failures = HashMap::new();
        get_failures.insert(
            2,
            GetObjectFailureMode::OperationError(ObjectClientError::ClientError(MockClientError(
                err_value.to_owned().into(),
            ))),
        );

        fail_sequential_read_test(prefetcher_type, 1024 * 1024 + 111, 1024 * 1024, config, get_failures);
    }

    proptest! {
        #[test]
        fn proptest_sequential_read(
            size in 1u64..1 * 1024 * 1024,
            read_size in 1usize..1 * 1024 * 1024,
            config: TestConfig,
        ) {
            run_sequential_read_test(PrefetcherType::Default, size, read_size, config);
        }

        #[test]
        fn proptest_sequential_read_small_read_size(size in 1u64..1 * 1024 * 1024, read_factor in 1usize..10, config: TestConfig) {
            // Pick read size smaller than the object size
            let read_size = (size as usize / read_factor).max(1);
            run_sequential_read_test(PrefetcherType::Default, size, read_size, config);
        }

        #[test]
        fn proptest_sequential_read_with_cache(
            size in 1u64..1 * 1024 * 1024,
            read_size in 1usize..1 * 1024 * 1024,
            config: TestConfig,
        ) {
            run_sequential_read_test(PrefetcherType::InMemoryCache(config.cache_block_size), size, read_size, config);
        }

        #[test]
        fn proptest_sequential_read_small_read_size_with_cache(size in 1u64..1 * 1024 * 1024, read_factor in 1usize..10,
            config: TestConfig) {
            // Pick read size smaller than the object size
            let read_size = (size as usize / read_factor).max(1);
            run_sequential_read_test(PrefetcherType::InMemoryCache(config.cache_block_size), size, read_size, config);
        }
    }

    #[test]
    fn test_sequential_read_regression() {
        let object_size = 854966;
        let read_size = 161647;
        let config = TestConfig {
            initial_request_size: 484941,
            max_read_window_size: 81509,
            sequential_prefetch_multiplier: 1,
            client_part_size: 181682,
            max_forward_seek_wait_distance: 1,
            max_backward_seek_distance: 18668,
            cache_block_size: 1 * MB,
            max_cursors: DEFAULT_MAX_CURSORS,
        };
        run_sequential_read_test(PrefetcherType::Default, object_size, read_size, config);
    }

    fn run_random_read_test(
        prefetcher_type: PrefetcherType,
        object_size: u64,
        reads: Vec<(u64, usize)>,
        test_config: TestConfig,
    ) {
        let client = Arc::new(
            MockClient::config()
                .bucket("test-bucket")
                .part_size(test_config.client_part_size)
                .enable_backpressure(true)
                .initial_read_window_size(test_config.client_part_size)
                .build(),
        );
        let object = MockObject::ramp(0xaa, object_size as usize, ETag::for_tests());
        let etag = object.etag();

        client.add_object("hello", object);

        let prefetcher_config = PrefetcherConfig {
            max_read_window_size: test_config.max_read_window_size,
            sequential_prefetch_multiplier: test_config.sequential_prefetch_multiplier,
            max_forward_seek_wait_distance: test_config.max_forward_seek_wait_distance,
            max_backward_seek_distance: test_config.max_backward_seek_distance,
            initial_request_size: test_config.initial_request_size,
            max_cursors: test_config.max_cursors,
        };

        let prefetcher = build_prefetcher(client, prefetcher_type, prefetcher_config);
        let object_id = ObjectId::new("hello".to_owned(), etag);
        let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, object_size);

        for (offset, length) in reads {
            assert!(offset < object_size);
            assert!(offset + length as u64 <= object_size);
            let expected = ramp_bytes((0xaa + offset) as usize, length);
            let buf = block_on(request.read(offset, length)).unwrap();
            let buf = buf.into_bytes().unwrap();
            assert_eq!(buf.len(), expected.len());
            // Don't spew the giant buffer if this test fails
            if buf[..] != expected[..] {
                for i in 0..buf.len() {
                    if buf[i] != expected[i] {
                        panic!(
                            "buffer mismatch at offset {}, saw {} expected {}",
                            i, buf[i], expected[i]
                        );
                    }
                }
            }
        }
    }

    fn random_read_strategy(max_object_size: u64) -> impl Strategy<Value = (u64, Vec<(u64, usize)>)> {
        (1..=max_object_size).prop_flat_map(|object_size| {
            (
                Just(object_size),
                proptest::collection::vec(
                    (0..object_size).prop_flat_map(move |offset| {
                        (1..=object_size - offset).prop_map(move |length| (offset, length as usize))
                    }),
                    0..10,
                ),
            )
        })
    }

    proptest! {
        #[test]
        fn proptest_random_read(
            reads in random_read_strategy(1 * 1024 * 1024),
            config: TestConfig,
        ) {
            let (object_size, reads) = reads;
            run_random_read_test(PrefetcherType::Default, object_size, reads, config);
        }

        #[test]
        fn proptest_random_read_with_cache(
            reads in random_read_strategy(1 * 1024 * 1024),
            config: TestConfig,
        ) {
            let (object_size, reads) = reads;
            run_random_read_test(PrefetcherType::InMemoryCache(config.cache_block_size), object_size, reads, config);
        }
    }

    #[test]
    fn test_random_read_regression() {
        let object_size = 724314;
        let reads = vec![(0, 516883)];
        let config = TestConfig {
            initial_request_size: 3684779,
            max_read_window_size: 2147621,
            sequential_prefetch_multiplier: 4,
            client_part_size: 516882,
            max_forward_seek_wait_distance: 16 * 1024 * 1024,
            max_backward_seek_distance: 2 * 1024 * 1024,
            cache_block_size: 1 * MB,
            max_cursors: DEFAULT_MAX_CURSORS,
        };
        run_random_read_test(PrefetcherType::Default, object_size, reads, config);
    }

    #[test]
    fn test_random_read_regression2() {
        let object_size = 755678;
        let reads = vec![(0, 278499), (311250, 1)];
        let config = TestConfig {
            initial_request_size: 556997,
            max_read_window_size: 105938,
            sequential_prefetch_multiplier: 7,
            client_part_size: 1219731,
            max_forward_seek_wait_distance: 16 * 1024 * 1024,
            max_backward_seek_distance: 2 * 1024 * 1024,
            cache_block_size: 1 * MB,
            max_cursors: DEFAULT_MAX_CURSORS,
        };
        run_random_read_test(PrefetcherType::Default, object_size, reads, config);
    }

    #[test]
    fn test_random_read_regression3() {
        let object_size = 755678;
        let reads = vec![(0, 236766), (291204, 1), (280930, 36002)];
        let config = TestConfig {
            initial_request_size: 556997,
            max_read_window_size: 105938,
            sequential_prefetch_multiplier: 7,
            client_part_size: 1219731,
            max_forward_seek_wait_distance: 2260662,
            max_backward_seek_distance: 2369799,
            cache_block_size: 1 * MB,
            max_cursors: DEFAULT_MAX_CURSORS,
        };
        run_random_read_test(PrefetcherType::Default, object_size, reads, config);
    }

    #[test]
    fn test_random_read_regression4() {
        let object_size = 14201;
        let reads = vec![(3584, 1), (9424, 1460), (3582, 3340), (248, 9218)];
        let config = TestConfig {
            initial_request_size: 457999,
            max_read_window_size: 863511,
            sequential_prefetch_multiplier: 5,
            client_part_size: 1972409,
            max_forward_seek_wait_distance: 2810651,
            max_backward_seek_distance: 3531090,
            cache_block_size: 1 * MB,
            max_cursors: DEFAULT_MAX_CURSORS,
        };
        run_random_read_test(PrefetcherType::Default, object_size, reads, config);
    }

    #[test]
    fn test_forward_seek_failure() {
        const PART_SIZE: usize = 8192;
        const OBJECT_SIZE: usize = 2 * PART_SIZE;

        let client = MockClient::config()
            .bucket("test-bucket")
            .part_size(PART_SIZE)
            .enable_backpressure(true)
            .initial_read_window_size(OBJECT_SIZE)
            .build();
        let object = MockObject::ramp(0xaa, OBJECT_SIZE, ETag::for_tests());
        let etag = object.etag();
        client.add_object("hello", object);

        let mut get_failures = HashMap::new();
        get_failures.insert(
            1,
            GetObjectFailureMode::StreamPositionError(
                2,
                ObjectClientError::ClientError(MockClientError(
                    "error in the second chunk of the first request".into(),
                )),
            ),
        );
        get_failures.insert(
            2,
            GetObjectFailureMode::OperationError(ObjectClientError::ClientError(MockClientError(
                "error in second request".into(),
            ))),
        );

        let client = Arc::new(countdown_failure_client(
            client,
            CountdownFailureConfig {
                get_failures,
                ..Default::default()
            },
        ));
        let prefetcher = build_prefetcher(client, PrefetcherType::Default, Default::default());
        block_on(async {
            let object_id = ObjectId::new("hello".to_owned(), etag.clone());
            let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);

            // The first read should trigger the prefetcher to try and get the whole object (in 2 parts).
            _ = request.read(0, 1).await.expect("first read should succeed");

            // Seek to the second part (where we injected a failure).
            let offset = PART_SIZE + 1;
            _ = request.read(offset as u64, 1).await.expect_err("seek should fail");

            // A retry should trigger a new request (also failing).
            _ = request
                .read(offset as u64, 1)
                .await
                .expect_err("first retry after failure should fail");

            // New retry should succeed (no more failures injected).
            let byte = request
                .read(offset as u64, 1)
                .await
                .expect("second retry should succeed");
            let expected = ramp_bytes(0xaa + offset, 1);
            assert_eq!(byte.into_bytes().unwrap()[..], expected[..]);
        });
    }

    #[test_case(PrefetcherType::Default)]
    #[test_case(PrefetcherType::InMemoryCache(8192))]
    fn test_short_read_failure(prefetcher_type: PrefetcherType) {
        const PART_SIZE: usize = 8192;
        const OBJECT_SIZE: usize = 2 * PART_SIZE;

        let client = MockClient::config()
            .bucket("test-bucket")
            .part_size(PART_SIZE)
            .enable_backpressure(true)
            .initial_read_window_size(PART_SIZE)
            .build();
        let object = MockObject::ramp(0xaa, OBJECT_SIZE, ETag::for_tests());
        let etag = object.etag();
        client.add_object("hello", object);

        let mut get_failures = HashMap::new();
        // On first request, terminate the stream without producing any data
        get_failures.insert(1, GetObjectFailureMode::StreamShortCircuit(1));
        // On third request (second request of second prefetcher),
        // terminate the stream early without producing all the requested data
        get_failures.insert(3, GetObjectFailureMode::StreamShortCircuit(1));

        let client = Arc::new(countdown_failure_client(
            client,
            CountdownFailureConfig {
                get_failures,
                ..Default::default()
            },
        ));
        let prefetcher_config = PrefetcherConfig {
            initial_request_size: PART_SIZE,
            ..Default::default()
        };
        let prefetcher = build_prefetcher(client, prefetcher_type, prefetcher_config);

        block_on(async {
            let object_id = ObjectId::new("hello".to_owned(), etag.clone());
            let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);

            // First read will terminate early
            assert!(matches!(
                request.read(0, 10).await.expect_err("read should fail"),
                PrefetchReadError::GetRequestTerminatedUnexpectedly,
            ));

            // Second read will return first part, but then terminate early before returning the remaining parts
            let bytes = request.read(0, PART_SIZE).await.unwrap();
            let expected = ramp_bytes(0xaa, PART_SIZE);
            assert_eq!(bytes.into_bytes().unwrap()[..], expected[..]);
            _ = request
                .read(PART_SIZE as u64, PART_SIZE)
                .await
                .expect_err("read should fail");

            // There are no more failures injected, since the prefetcher will reset on failure, now we should be able to read the whole data.
            let bytes = request.read(0, OBJECT_SIZE).await.unwrap();
            let expected = ramp_bytes(0xaa, OBJECT_SIZE);
            assert_eq!(bytes.into_bytes().unwrap()[..], expected[..]);

            // Shouldn't fail if the short read is due to object size not due to the stream terminating early
            let bytes = request.read(PART_SIZE as u64, OBJECT_SIZE).await.unwrap();
            let expected = ramp_bytes(0xaa + PART_SIZE, PART_SIZE);
            assert_eq!(bytes.into_bytes().unwrap()[..], expected[..]);
        });
    }

    #[test_case(0, 25; "no first read")]
    #[test_case(60, 25; "read beyond first part")]
    #[test_case(20, 25; "read in first part")]
    #[test_case(125, 110; "read in second request")]
    fn test_forward_seek(first_read_size: usize, part_size: usize) {
        const OBJECT_SIZE: usize = 200;
        const FIRST_REQUEST_SIZE: usize = 100;

        let client = Arc::new(
            MockClient::config()
                .bucket("test-bucket")
                .part_size(part_size)
                .enable_backpressure(true)
                .initial_read_window_size(FIRST_REQUEST_SIZE)
                .build(),
        );
        let object = MockObject::ramp(0xaa, OBJECT_SIZE, ETag::for_tests());
        let etag = object.etag();

        client.add_object("hello", object);

        let prefetcher = build_prefetcher(client, PrefetcherType::Default, Default::default());

        // Try every possible seek from first_read_size
        for offset in first_read_size + 1..OBJECT_SIZE {
            let object_id = ObjectId::new("hello".to_owned(), etag.clone());
            let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);
            if first_read_size > 0 {
                let _first_read = block_on(request.read(0, first_read_size)).unwrap();
            }

            let byte = block_on(request.read(offset as u64, 1)).unwrap();
            let expected = ramp_bytes(0xaa + offset, 1);
            assert_eq!(byte.into_bytes().unwrap()[..], expected[..]);
        }
    }

    #[test_case(60, 25; "read beyond first part")]
    #[test_case(20, 25; "read in first part")]
    #[test_case(125, 110; "read in second request")]
    fn test_backward_seek(first_read_size: usize, part_size: usize) {
        const OBJECT_SIZE: usize = 200;

        let client = Arc::new(
            MockClient::config()
                .bucket("test-bucket")
                .part_size(part_size)
                .enable_backpressure(true)
                .initial_read_window_size(part_size)
                .build(),
        );
        let object = MockObject::ramp(0xaa, OBJECT_SIZE, ETag::for_tests());
        let etag = object.etag();

        client.add_object("hello", object);

        let prefetcher = build_prefetcher(client, PrefetcherType::Default, Default::default());

        // Try every possible seek from first_read_size
        for offset in 0..first_read_size {
            let object_id = ObjectId::new("hello".to_owned(), etag.clone());
            let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);
            if first_read_size > 0 {
                let _first_read = block_on(request.read(0, first_read_size)).unwrap();
            }

            let byte = block_on(request.read(offset as u64, 1)).unwrap();
            let expected = ramp_bytes(0xaa + offset, 1);
            assert_eq!(byte.into_bytes().unwrap()[..], expected[..]);
        }
    }

    #[test_case(8 * MB, 8 * MB, 1 * MB + 128 * KB; "default")]
    #[test_case(8 * MB, 8 * MB, 0; "no initial request")]
    #[test_case(1 * KB, 1 * MB, 10 * MB; "initial request larger than part size")]
    #[test_case(16 * MB, 8 * MB, 1 * MB + 128 * KB; "larger initial read window")]
    #[test_case(16 * MB, 8 * MB, 0; "larger initial read window w/o initial request")]
    #[test_case(1 * KB, 8 * MB, 1 * MB + 128 * KB; "smaller initial read window")]
    #[test_case(1 * KB, 8 * MB, 0; "smaller initial read window w/o initial request")]
    fn test_initial_reqeust_size(initial_read_window_size: usize, part_size: usize, initial_request_size: usize) {
        let object_size = (16 * MB) as u64;

        let client = Arc::new(
            MockClient::config()
                .bucket("test-bucket")
                .part_size(part_size)
                .enable_backpressure(true)
                .initial_read_window_size(initial_read_window_size)
                .build(),
        );

        let object = MockObject::ramp(0xaa, object_size as usize, ETag::for_tests());
        let etag = object.etag();
        client.add_object("test-object", object);

        let prefetcher_config = PrefetcherConfig {
            initial_request_size,
            ..Default::default()
        };

        let prefetcher = build_prefetcher(client.clone(), PrefetcherType::Default, prefetcher_config);
        let object_id = ObjectId::new("test-object".to_owned(), etag);
        let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, object_size);

        // Perform sequential reads to test the download functionality
        let mut next_offset = 0;
        while next_offset < object_size {
            let buf = block_on(request.read(next_offset, 256 * KB)).unwrap();
            if buf.is_empty() {
                break;
            }
            let buf = buf.into_bytes().unwrap();
            let expected = ramp_bytes((0xaa + next_offset) as usize, buf.len());
            assert_eq!(&buf[..], &expected[..]);
            next_offset += buf.len() as u64;
        }
        assert_eq!(next_offset, object_size);
    }

    /// Resetting a cursor mid-read reclaims memory, and the next read transparently
    /// creates a new cursor and returns correct data.
    #[test]
    fn reset_cursor_mid_read_recovers() {
        const PART_SIZE: usize = 256 * KB;
        const OBJECT_SIZE: usize = 4 * MB;
        const READ_SIZE: usize = 256 * KB;

        let client = Arc::new(
            MockClient::config()
                .bucket("test-bucket")
                .part_size(PART_SIZE)
                .enable_backpressure(true)
                .initial_read_window_size(PART_SIZE)
                .build(),
        );
        let object = MockObject::ramp(0xaa, OBJECT_SIZE, ETag::for_tests());
        let expected = object.read(0, READ_SIZE * 2);
        let etag = object.etag();
        client.add_object("hello", object);

        let prefetcher = build_prefetcher(client, PrefetcherType::Default, Default::default());
        let object_id = ObjectId::new("hello".to_owned(), etag);
        let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);

        block_on(async {
            // Read the first chunk
            let buf = request.read(0, READ_SIZE).await.unwrap();
            assert_eq!(buf.into_bytes().unwrap()[..], expected[..READ_SIZE]);

            // Reset the cursor via the pool
            let cursor_id = request
                .cursors
                .last()
                .expect("cursor should still be populated")
                .cursor_id();
            let pool = request.part_stream.pool();
            let available_before = pool.memory_available_for_reservation();
            assert!(pool.reset_cursor(cursor_id));

            // Memory should be reclaimed (reservation released)
            let available_after = pool.memory_available_for_reservation();
            assert!(
                available_after > available_before,
                "expected memory to be reclaimed: before={available_before}, after={available_after}"
            );

            // Second reset should return false
            assert!(!pool.reset_cursor(cursor_id));

            // Next read should succeed transparently (creates a new cursor)
            let buf = request.read(READ_SIZE as u64, READ_SIZE).await.unwrap();
            assert_eq!(buf.into_bytes().unwrap()[..], expected[READ_SIZE..]);
        });
    }

    mod multi_cursor {
        use super::*;
        use mountpoint_s3_client::mock_client::Operation;
        use rand::{RngExt, SeedableRng};
        use rand_pcg::Pcg64;

        const PART_SIZE: usize = 1 * MB;

        /// Build a mock client backed by a ramp object of the given size, returning the client and
        /// the object's ETag. Backpressure is enabled with a one-part initial read window (matching
        /// the real S3 client's behaviour).
        fn mock_client_with_object(object_size: usize) -> (Arc<MockClient>, ETag) {
            let client = Arc::new(
                MockClient::config()
                    .bucket("test-bucket")
                    .part_size(PART_SIZE)
                    .enable_backpressure(true)
                    .initial_read_window_size(PART_SIZE)
                    .build(),
            );
            let object = MockObject::ramp(0xaa, object_size, ETag::for_tests());
            let etag = object.etag();
            client.add_object("hello", object);
            (client, etag)
        }

        fn prefetcher_with_max_cursors(client: Arc<MockClient>, max_cursors: usize) -> Prefetcher<Arc<MockClient>> {
            let config = PrefetcherConfig {
                max_cursors,
                ..Default::default()
            };
            build_prefetcher(client, PrefetcherType::Default, config)
        }

        /// Read `length` bytes at `offset` and assert they match the ramp object.
        async fn read_and_check(request: &mut PrefetchGetObject<Arc<MockClient>>, offset: u64, length: usize) {
            let buf = request.read(offset, length).await.unwrap();
            let buf = buf.into_bytes().unwrap();
            assert_eq!(buf.len(), length, "short read at offset {offset}");
            let expected = ramp_bytes((0xaa + offset) as usize, length);
            assert_eq!(&buf[..], &expected[..], "wrong bytes at offset {offset}");
        }

        /// Two-cursor round-robin should use two cursors.
        /// Alternating reads from two well-separated positions must return correct data and, once both cursors exist,
        /// must not issue a new request on every visit (the cursors are reused via switches, not recreated).
        #[test]
        fn two_cursor_round_robin_reuses_cursors() {
            const OBJECT_SIZE: usize = 16 * MB;
            const VISIT: usize = 128 * KB;

            let (client, etag) = mock_client_with_object(OBJECT_SIZE);
            let get_counter = client.new_counter(Operation::GetObject);
            let prefetcher = prefetcher_with_max_cursors(client.clone(), 8);
            let object_id = ObjectId::new("hello".to_owned(), etag);
            let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);

            block_on(async {
                // Prime both cursors: position 0 and position 8 MiB.
                read_and_check(&mut request, 0, VISIT).await;
                read_and_check(&mut request, 8 * MB as u64, VISIT).await;
                let after_priming = get_counter.count();

                // Now alternate between the two sequential streams for several rounds. Each cursor
                // serves its own reads sequentially; no new GetObject request should be needed
                // beyond growing the existing streams' windows.
                let mut off0 = VISIT as u64;
                let mut off1 = 8 * MB as u64 + VISIT as u64;
                for _ in 0..8 {
                    read_and_check(&mut request, off0, VISIT).await;
                    read_and_check(&mut request, off1, VISIT).await;
                    off0 += VISIT as u64;
                    off1 += VISIT as u64;
                }
                let after_rounds = get_counter.count();

                // The alternating rounds are served by the two existing cursors, not by resetting on
                // every switch (which would cost one request per visit, i.e. 16). Each cursor issues
                // exactly one more request here: priming opened each cursor's first request covering
                // `initial_request_size` (1152 KiB) from its start offset, and the remainder of each
                // cursor's stream is fetched by a second request that is deferred until the reader
                // has consumed half of the first window. At 128 KiB per visit that threshold falls in
                // the fifth visit to each stream, so both cursors open their second (and final)
                // request during these rounds and neither needs another.
                let extra_requests = after_rounds - after_priming;
                assert_eq!(
                    extra_requests, 2,
                    "expected cursors to be reused across alternation, but issued {extra_requests} \
                     extra requests over 16 visits (looks like reset-per-switch)"
                );
            });
            // Exactly the two sequential streams should be tracked; the workload genuinely
            // exercises the multi-cursor path (not a trivial single-cursor pass).
            assert_eq!(request.tracked_cursors(), 2, "expected exactly two tracked cursors");
        }

        /// A distant read (beyond any cursor's forward reach) creates a new cursor and does
        /// not corrupt the existing one; returning to the original position still reads correctly.
        #[test]
        fn distant_read_creates_new_cursor_without_disturbing_original() {
            const OBJECT_SIZE: usize = 64 * MB;
            const VISIT: usize = 128 * KB;

            let (client, etag) = mock_client_with_object(OBJECT_SIZE);
            let prefetcher = prefetcher_with_max_cursors(client.clone(), 8);
            let object_id = ObjectId::new("hello".to_owned(), etag);
            let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);

            block_on(async {
                // Establish cursor A near the start.
                read_and_check(&mut request, 0, VISIT).await;
                read_and_check(&mut request, VISIT as u64, VISIT).await;
                // Jump far away (beyond forward reach), creating cursor B.
                read_and_check(&mut request, 50 * MB as u64, VISIT).await;
                // Continue B sequentially.
                read_and_check(&mut request, 50 * MB as u64 + VISIT as u64, VISIT).await;
                // Return to A's next sequential position; A must be intact and serve it.
                read_and_check(&mut request, 2 * VISIT as u64, VISIT).await;
            });
            // Both cursor A (near start) and cursor B (at 50 MiB) should be tracked.
            assert_eq!(
                request.tracked_cursors(),
                2,
                "distant read should have created a second cursor"
            );
        }

        /// A cursor's window bound must relax when the cursor ahead of it advancesand the tracked
        /// set changes (between those events a downstream cursor can move forward, leaving a
        /// trailing cursor's bound stale). The next recompute must widen the trailing bound to the
        /// downstream cursor's new position to prevent the cursor stalling or slowing down
        /// significantly.
        #[test]
        fn cursor_window_bound_relaxes_as_downstream_cursor_advances() {
            const OBJECT_SIZE: usize = 128 * MB;
            const VISIT: usize = 128 * KB;
            const A_START: u64 = 0;
            const B_START: u64 = 40 * MB as u64;
            const C_START: u64 = 80 * MB as u64;

            let (client, etag) = mock_client_with_object(OBJECT_SIZE);
            let prefetcher = prefetcher_with_max_cursors(client.clone(), 8);
            let object_id = ObjectId::new("hello".to_owned(), etag);
            let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);

            block_on(async {
                // Cursor A near the start.
                read_and_check(&mut request, A_START, VISIT).await;
                // Distant read creates cursor B ahead of A; creating it recomputes bounds, so A is
                // now bounded at B's current position and B (frontmost) is unbounded.
                read_and_check(&mut request, B_START, VISIT).await;

                let snapshot = request.cursor_bounds_snapshot().await;
                let mut sorted = snapshot.clone();
                sorted.sort_by_key(|(off, _)| *off);
                let (_, a_bound_before) = sorted[0];
                let (b_current_before, b_bound) = sorted[1];
                assert_eq!(
                    a_bound_before,
                    Some(b_current_before),
                    "A should be bounded at B's initial position"
                );
                assert_eq!(b_bound, None, "frontmost cursor B should be unbounded");

                // Advance B well forward via the fast path (active cursor). This does NOT recompute
                // bounds, so A's bound stays at B's old position (stale but conservative).
                let b_target = B_START + 8 * MB as u64;
                let mut b_off = B_START + VISIT as u64;
                while b_off < b_target {
                    read_and_check(&mut request, b_off, VISIT).await;
                    b_off += VISIT as u64;
                }
                let stale = request.cursor_bounds_snapshot().await;
                let mut stale_sorted = stale.clone();
                stale_sorted.sort_by_key(|(off, _)| *off);
                assert_eq!(
                    stale_sorted[0].1, a_bound_before,
                    "A's bound must not change without a tracked-set change (recompute is create-only)"
                );

                // A third distant read creates cursor C, recomputing bounds. A's nearest cursor ahead
                // is still B, but B has advanced, so A's bound must relax to B's new position.
                read_and_check(&mut request, C_START, VISIT).await;

                let after = request.cursor_bounds_snapshot().await;
                let mut after_sorted = after.clone();
                after_sorted.sort_by_key(|(off, _)| *off);
                assert_eq!(after_sorted.len(), 3, "A, B, C should all be tracked");
                let (_, a_bound_after) = after_sorted[0];
                let (b_current_after, b_bound_after) = after_sorted[1];
                let (c_current, c_bound) = after_sorted[2];

                // The relaxation: A's bound widened to B's new (larger) position.
                assert_eq!(
                    a_bound_after,
                    Some(b_current_after),
                    "A should be re-bounded at B's new position"
                );
                assert!(
                    a_bound_after.unwrap() > a_bound_before.unwrap(),
                    "A's bound should have relaxed as B advanced: {:?} -> {:?}",
                    a_bound_before,
                    a_bound_after
                );
                // The middle cursor B is now bounded at C; C (frontmost) is unbounded.
                assert_eq!(b_bound_after, Some(c_current), "B should be bounded at C's position");
                assert_eq!(c_bound, None, "frontmost cursor C should be unbounded");
            });
        }

        /// Creating a cursor recomputes every tracked cursor's window bound against the current
        /// tracked set: each cursor is bounded at the read position of the nearest cursor ahead of
        /// it, and the frontmost cursor is left unbounded. Because the bound tracks the downstream
        /// cursor's current position, a trailing cursor's bound widens (relaxes) when the cursor
        /// ahead of it has advanced since the previous recompute.
        ///
        /// This asserts only the observable post-creation bounds, not when the recompute runs, so it
        /// does not constrain whether bounds are also refreshed on other paths (e.g. a read crossing
        /// a bound, covered by `cursor_window_bound_relaxes_when_read_crosses_it`).
        #[test]
        fn creating_cursor_recomputes_bounds_against_current_positions() {
            const OBJECT_SIZE: usize = 128 * MB;
            const VISIT: usize = 128 * KB;
            const A_START: u64 = 0;
            const B_START: u64 = 40 * MB as u64;
            const C_START: u64 = 80 * MB as u64;
            // How far B advances (via its own sequential reads) after it is created but before C is
            // created. This is what makes A's re-bound at C-creation a relaxation rather than a
            // no-op: A must be re-bounded at B's advanced position, not its original one.
            const B_ADVANCE: u64 = 8 * MB as u64;

            let (client, etag) = mock_client_with_object(OBJECT_SIZE);
            let prefetcher = prefetcher_with_max_cursors(client.clone(), 8);
            let object_id = ObjectId::new("hello".to_owned(), etag);
            let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);

            block_on(async {
                // Cursor A near the start; while it is alone it is frontmost and unbounded.
                read_and_check(&mut request, A_START, VISIT).await;
                // A distant read creates cursor B ahead of A. The recompute bounds A at B's start
                // and leaves B (now frontmost) unbounded.
                read_and_check(&mut request, B_START, VISIT).await;
                let b_start_pos = {
                    let mut s = request.cursor_bounds_snapshot().await;
                    s.sort_by_key(|(off, _)| *off);
                    assert_eq!(s[0].1, Some(s[1].0), "A should be bounded at B's start position");
                    assert_eq!(s[1].1, None, "frontmost cursor B should be unbounded");
                    s[1].0
                };

                // Advance B forward via its own sequential reads. B is frontmost/unbounded, so this
                // crosses no bound and simply moves B's read position ahead of where A is bounded.
                let mut b_off = B_START + VISIT as u64;
                while b_off < B_START + B_ADVANCE {
                    read_and_check(&mut request, b_off, VISIT).await;
                    b_off += VISIT as u64;
                }

                // A third distant read creates cursor C. The recompute now sees B at its advanced
                // position, so A is re-bounded there (a relaxation), B is bounded at C, and C is
                // frontmost/unbounded.
                read_and_check(&mut request, C_START, VISIT).await;

                let mut after = request.cursor_bounds_snapshot().await;
                after.sort_by_key(|(off, _)| *off);
                assert_eq!(after.len(), 3, "A, B, C should all be tracked");
                let (_, a_bound) = after[0];
                let (b_pos, b_bound) = after[1];
                let (_, c_bound) = after[2];

                assert_eq!(a_bound, Some(b_pos), "A should be bounded at B's current position");
                assert!(
                    a_bound.unwrap() > b_start_pos,
                    "A's bound should have relaxed as B advanced: bounded at {:?}, B started at {b_start_pos}",
                    a_bound,
                );
                assert_eq!(b_bound, Some(after[2].0), "B should be bounded at C's position");
                assert_eq!(c_bound, None, "frontmost cursor C should be unbounded");
            });
        }

        /// A trailing cursor that reads forward past its own bound must have that bound relaxed.
        /// This is the read-past-bound trigger. The bound is recomputed against the downstream
        /// cursor's current position, so the window can grow again.
        #[test]
        fn cursor_window_bound_relaxes_when_read_crosses_it() {
            const OBJECT_SIZE: usize = 128 * MB;
            const READ: usize = 1 * MB;
            // B must be far enough ahead of A that it is created as a separate cursor (beyond A's
            // forward reach), matching the distant-read convention used elsewhere.
            const B_START: u64 = 50 * MB as u64;
            const B_ADVANCED: u64 = 58 * MB as u64;

            let (client, etag) = mock_client_with_object(OBJECT_SIZE);
            let prefetcher = prefetcher_with_max_cursors(client.clone(), 8);
            let object_id = ObjectId::new("hello".to_owned(), etag);
            let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);

            block_on(async {
                // Cursor A at offset 0.
                read_and_check(&mut request, 0, READ).await;
                // Distant read creates cursor B, bounding A at B's start (50 MiB).
                read_and_check(&mut request, B_START, READ).await;
                // Advance B forward to ~58 MiB (sequential reads on B). No new cursor is created, so
                // A's bound stays stale at 50 MiB.
                let mut b_off = B_START + READ as u64;
                while b_off < B_ADVANCED {
                    read_and_check(&mut request, b_off, READ).await;
                    b_off += READ as u64;
                }

                // Precondition: A's bound is stale (reflects A's starting position)
                let before = {
                    let mut s = request.cursor_bounds_snapshot().await;
                    s.sort_by_key(|(off, _)| *off);
                    s
                };
                let (_, a_bound_before) = before[0];
                let (b_current_before, _) = before[1];
                let a_bound_before = a_bound_before.expect("A should be bounded while B is ahead");
                assert!(
                    a_bound_before < b_current_before,
                    "A's bound {a_bound_before} should be stale (below B's advanced position {b_current_before})"
                );

                // Drive A forward from 1 MiB across its stale bound (~51 MiB) toward B. We stop a few
                // parts short of B (55 MiB, with B at ~58 MiB) so B never back-seeks to serve A's
                // reads (its backward seek window is only `max_backward_seek_distance` = 1 MiB). A
                // serves them itself, and the read that reaches its stale bound triggers a recompute
                // against B's current position, relaxing the bound so A can keep advancing.
                let a_target = B_ADVANCED - 3 * READ as u64;
                let mut a_off = READ as u64;
                while a_off <= a_target {
                    read_and_check(&mut request, a_off, READ).await;
                    a_off += READ as u64;
                }

                let after = {
                    let mut s = request.cursor_bounds_snapshot().await;
                    s.sort_by_key(|(off, _)| *off);
                    s
                };
                // Still exactly A and B tracked (no new cursor created by the traversal).
                assert_eq!(after.len(), 2, "no new cursor should have been created");
                let (a_current_after, a_bound_after) = after[0];
                let (b_current_after, _) = after[1];
                // A read past its old bound, so its bound relaxed to B's current position, strictly
                // larger than the stale value, letting A's window grow through the region again.
                assert!(
                    a_current_after > a_bound_before,
                    "A should have read past its old bound"
                );
                assert_eq!(
                    a_bound_after,
                    Some(b_current_after),
                    "A should be re-bounded at B's current position after crossing its stale bound"
                );
            });
        }

        /// Evicting a cursor at the `max_cursors` cap recomputes the surviving cursors' bounds
        /// against the remaining tracked set. A survivor that was bounded at the evicted cursor's
        /// position must be re-bounded at whatever cursor is now ahead of it, so its window is never
        /// left throttled against a cursor that no longer exists.
        #[test]
        fn evicting_cursor_recomputes_bounds_on_survivors() {
            const OBJECT_SIZE: usize = 128 * MB;
            const VISIT: usize = 128 * KB;
            // Cap of two: creating a third cursor evicts the least-recently-used one.
            const MAX_CURSORS: usize = 2;
            const MID: u64 = 40 * MB as u64;
            const NEAR: u64 = 0;
            const FAR: u64 = 80 * MB as u64;

            let (client, etag) = mock_client_with_object(OBJECT_SIZE);
            let prefetcher = prefetcher_with_max_cursors(client.clone(), MAX_CURSORS);
            let object_id = ObjectId::new("hello".to_owned(), etag);
            let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);

            block_on(async {
                // Create the MID cursor first; it is alone and frontmost, so unbounded.
                read_and_check(&mut request, MID, VISIT).await;
                // A distant read behind MID creates the NEAR cursor, reaching the cap of two. The
                // recompute bounds NEAR at MID's position; MID (frontmost) stays unbounded. NEAR was
                // read most recently, so MID is now the least-recently-used cursor.
                read_and_check(&mut request, NEAR, VISIT).await;
                let before = {
                    let mut s = request.cursor_bounds_snapshot().await;
                    s.sort_by_key(|(off, _)| *off);
                    s
                };
                assert_eq!(before.len(), 2, "NEAR and MID should be tracked");
                assert_eq!(before[0].1, Some(before[1].0), "NEAR should be bounded at MID's position");
                assert_eq!(before[1].1, None, "frontmost cursor MID should be unbounded");

                // A distant read ahead of both creates the FAR cursor. At the cap this evicts the
                // least-recently-used cursor (MID, the very cursor NEAR was bounded at), and the
                // ensuing recompute must re-bound NEAR against FAR rather than leave it pointing at
                // MID's vacated position.
                read_and_check(&mut request, FAR, VISIT).await;

                let mut after = request.cursor_bounds_snapshot().await;
                after.sort_by_key(|(off, _)| *off);
                assert_eq!(after.len(), 2, "MID evicted at the cap, leaving NEAR and FAR");
                let (near_pos, near_bound) = after[0];
                let (far_pos, far_bound) = after[1];
                // `near_pos` is the cursor's next read position (past the VISIT it just served),
                // so it sits just above NEAR's start but well below the evicted MID.
                assert!(near_pos < MID, "the surviving trailing cursor is NEAR (below evicted MID)");
                assert_eq!(
                    near_bound,
                    Some(far_pos),
                    "NEAR must be re-bounded at FAR after MID (its old bound target) was evicted"
                );
                assert!(
                    near_bound.unwrap() > before[0].1.unwrap(),
                    "NEAR's bound should have moved forward from MID ({:?}) to FAR ({:?}) on eviction",
                    before[0].1,
                    near_bound,
                );
                assert_eq!(far_bound, None, "frontmost cursor FAR should be unbounded");
            });
        }

        /// Core "never worse" gate: a random workload at `max_cursors = 8` must not issue more
        /// GetObject requests than the same workload at `max_cursors = 1` (the prior single-cursor
        /// behaviour), plus a small allowance for the up-to-`max_cursors` initial speculative
        /// requests in flight. This guards against multi-cursor amplifying request counts on a
        /// random-access workload, where extra cursors bring no benefit.
        #[test]
        fn random_read_not_worse_than_single_cursor() {
            const OBJECT_SIZE: usize = 128 * MB;
            const READ: usize = 4 * KB;
            const MULTI_CURSOR_COUNT: usize = 8;
            // Deterministic pseudo-random offsets, each far from the others.
            let mut rng = Pcg64::seed_from_u64(0x5eed_1234);
            let max_offset = (OBJECT_SIZE - READ) as u64;
            // `& !0xfff` rounds each offset down to a 4 KiB boundary.
            let offsets: Vec<u64> = (0..200).map(|_| (rng.random_range(0..=max_offset)) & !0xfff).collect();

            // Run the identical offset sequence at a given cursor count; returns
            // (GetObject request count, number of cursors still tracked at the end).
            let run = |max_cursors: usize| -> (u64, usize) {
                let (client, etag) = mock_client_with_object(OBJECT_SIZE);
                let get_counter = client.new_counter(Operation::GetObject);
                let prefetcher = prefetcher_with_max_cursors(client.clone(), max_cursors);
                let object_id = ObjectId::new("hello".to_owned(), etag);
                let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);
                block_on(async {
                    for &offset in &offsets {
                        read_and_check(&mut request, offset, READ).await;
                    }
                });
                (get_counter.count(), request.tracked_cursors())
            };

            let (single, _) = run(1);
            let (multi, multi_cursors) = run(MULTI_CURSOR_COUNT);
            // Assertion 1: the soft cap holds. This workload touches ~200 distinct far-apart
            // positions, so the tracked set can only stay <= 8 if the LRU is actually evicting
            // cursors (a broken eviction would let it grow unbounded).
            assert!(
                multi_cursors <= MULTI_CURSOR_COUNT,
                "tracked cursors {multi_cursors} exceeded the max_cursors soft cap of 8"
            );
            // Assertion 2: no request amplification vs. single-cursor. Multi-cursor may issue up to
            // `max_cursors` extra requests (each new cursor fires one speculative initial GetObject
            // that a random workload may never reuse); a larger gap means it is amplifying reads on
            // random access, which this test guards against.
            assert!(
                multi <= single + MULTI_CURSOR_COUNT as u64,
                "random workload amplified requests: single-cursor={single}, 8-cursor={multi}"
            );
        }

        /// Multi-cursor doesn't regress a purely sequential read:
        /// `max_cursors = 8` issues the same number of GetObject requests as with
        /// `max_cursors = 1`, i.e. multi-cursor tracking adds no requests to
        /// the common case.
        #[test]
        fn sequential_read_no_extra_requests_with_multicursor_enabled() {
            const OBJECT_SIZE: usize = 32 * MB;
            const READ: usize = 256 * KB;

            let run = |max_cursors: usize| -> u64 {
                let (client, etag) = mock_client_with_object(OBJECT_SIZE);
                let get_counter = client.new_counter(Operation::GetObject);
                let prefetcher = prefetcher_with_max_cursors(client.clone(), max_cursors);
                let object_id = ObjectId::new("hello".to_owned(), etag);
                let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);
                block_on(async {
                    let mut offset = 0u64;
                    while offset < OBJECT_SIZE as u64 {
                        read_and_check(&mut request, offset, READ).await;
                        offset += READ as u64;
                    }
                });
                get_counter.count()
            };

            assert_eq!(
                run(1),
                run(8),
                "sequential read issued a different number of requests with 8 cursors than with 1"
            );
        }

        /// `nearest_ahead_bound` bounds a cursor at the position of the nearest cursor ahead of it,
        /// and leaves only the frontmost cursor unbounded.  Similar coverage to
        /// creating_cursor_recomputes_bounds_against_current_positions but directly tests
        /// nearest_ahead_bound and validates the case where the bounds are stored out of order.
        #[test]
        fn nearest_ahead_bound_orders_distinct_offsets() {
            // Cursors at 0, 80, 40 (tracking order arbitrary): each is bounded at the next-ahead
            // position, and the frontmost (80) is unbounded.
            let offsets = [Some(0), Some(80), Some(40)];
            assert_eq!(nearest_ahead_bound(0, &offsets), Some(40), "cursor at 0 -> next is 40");
            assert_eq!(
                nearest_ahead_bound(1, &offsets),
                None,
                "frontmost cursor (80) is unbounded"
            );
            assert_eq!(nearest_ahead_bound(2, &offsets), Some(80), "cursor at 40 -> next is 80");
        }

        /// Only one of several cursors at the same offset may prefetch, avoiding read
        /// amplification.
        #[test]
        fn nearest_ahead_bound_breaks_equal_offset_ties() {
            // Two cursors at 40, one leader at 80. The lower-indexed 40 is bounded at its equal-offset
            // peer; the higher-indexed 40 is bounded at the leader; the leader is unbounded.
            let offsets = [Some(40), Some(40), Some(80)];
            assert_eq!(
                nearest_ahead_bound(0, &offsets),
                Some(40),
                "lower-indexed equal-offset cursor is bounded at its peer (not left unbounded)"
            );
            assert_eq!(
                nearest_ahead_bound(1, &offsets),
                Some(80),
                "higher-indexed equal-offset cursor is bounded at the leader ahead"
            );
            assert_eq!(nearest_ahead_bound(2, &offsets), None, "leader (80) is unbounded");

            // Three cursors all at the same offset: only the last stays unbounded, so exactly one
            // window grows over the region rather than three.
            let all_equal = [Some(40), Some(40), Some(40)];
            assert_eq!(nearest_ahead_bound(0, &all_equal), Some(40));
            assert_eq!(nearest_ahead_bound(1, &all_equal), Some(40));
            assert_eq!(
                nearest_ahead_bound(2, &all_equal),
                None,
                "only the last equal cursor is unbounded"
            );
        }

        /// A cursor reset by the memory limiter (position `None`) is skipped as a bound candidate,
        /// and asking for the bound of a reset cursor yields `None`.
        #[test]
        fn nearest_ahead_bound_ignores_reset_cursors() {
            // The cursor ahead (index 1) has been reset, so index 0 falls through to the next live
            // cursor ahead (index 2 at 80) rather than being bounded at a vacated slot.
            let offsets = [Some(0), None, Some(80)];
            assert_eq!(
                nearest_ahead_bound(0, &offsets),
                Some(80),
                "skip reset cursor, bound at next live one"
            );
            assert_eq!(nearest_ahead_bound(1, &offsets), None, "reset cursor has no bound");
            assert_eq!(
                nearest_ahead_bound(2, &offsets),
                None,
                "frontmost live cursor is unbounded"
            );
        }

        /// A dominant sequential stream sharing a file handle with up to `max_cursors - 1` concurrent
        /// distant readers is undisturbed: its serving cursor keeps one stable id for the whole run.
        /// Each dominant read refreshes its cursor to most-recently-used, so a burst of R distant
        /// reads between two dominant reads (R competing cursors) evicts the dominant only once
        /// `1 + R > max_cursors`; below that (`R <= max_cursors - 1`) it is always protected.
        #[test]
        fn dominant_stream_protected_below_cursor_cap() {
            const MAX_CURSORS: usize = 8;
            const OBJECT_SIZE: usize = 512 * MB;
            const VISIT: usize = 128 * KB;
            // The dominant stream is confined to the first half of the object; look-aside reads land
            // at random VISIT-aligned offsets in the second half, spread widely enough that each
            // lands where no existing cursor can serve it and creates a fresh competing cursor.
            const DOMINANT_END: u64 = 256 * MB as u64;
            const BURSTS: usize = 12;
            // One fewer distant reader per burst than the cap, so the dominant is never evicted.
            const DISTANT_PER_BURST: usize = MAX_CURSORS - 1;

            block_on(async {
                let (client, etag) = mock_client_with_object(OBJECT_SIZE);
                let prefetcher = prefetcher_with_max_cursors(client.clone(), MAX_CURSORS);
                let object_id = ObjectId::new("hello".to_owned(), etag);
                let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, OBJECT_SIZE as u64);

                // Deterministic RNG for the random look-aside offsets in [DOMINANT_END, OBJECT_SIZE).
                let distant_slots = (OBJECT_SIZE as u64 - DOMINANT_END) / VISIT as u64;
                let mut rng = Pcg64::seed_from_u64(0x5eed);

                let mut dominant_offset = 0u64;
                // Prime the dominant cursor and record its id (the "created once" baseline).
                read_and_check(&mut request, dominant_offset, VISIT).await;
                dominant_offset += VISIT as u64;
                let dominant_id = request.active_cursor_id().expect("dominant cursor must exist");

                for _ in 0..BURSTS {
                    // A burst of distant look-aside reads, with no dominant read between them, so they
                    // create competing cursors after the dominant's last refresh.
                    for _ in 0..DISTANT_PER_BURST {
                        let offset = DOMINANT_END + rng.random_range(0..distant_slots) * VISIT as u64;
                        read_and_check(&mut request, offset, VISIT).await;
                    }
                    // Return to the dominant stream: its serving cursor must be the same one throughout.
                    read_and_check(&mut request, dominant_offset, VISIT).await;
                    dominant_offset += VISIT as u64;
                    assert_eq!(
                        request.active_cursor_id().expect("dominant cursor must exist"),
                        dominant_id,
                        "dominant stream's cursor should stay stable with {DISTANT_PER_BURST} distant \
                         readers (<= cap-1), but its id changed"
                    );
                    assert_eq!(
                        request.tracked_cursors(),
                        MAX_CURSORS,
                        "cursor set should be saturated so eviction is actually exercised"
                    );
                }
            });
        }
    }

    #[cfg(feature = "shuttle")]
    mod shuttle_tests {
        use super::*;

        use futures::task::{FutureObj, Spawn, SpawnError};
        use shuttle::future::block_on;
        use shuttle::rand::Rng;
        use shuttle::scheduler::{PctScheduler, RandomScheduler};
        use shuttle::{Config, Runner};

        struct ShuttleRuntime;
        impl Spawn for ShuttleRuntime {
            fn spawn_obj(&self, future: FutureObj<'static, ()>) -> Result<(), SpawnError> {
                shuttle::future::spawn(future);
                Ok(())
            }
        }

        fn sequential_read_stress_helper() {
            let mut rng = shuttle::rand::thread_rng();
            let object_size = rng.gen_range(1u64..1 * 1024 * 1024);
            let max_read_window_size = rng.gen_range(16usize..1 * 1024 * 1024);
            let sequential_prefetch_multiplier = rng.gen_range(2usize..16);
            let part_size = rng.gen_range(16usize..INITIAL_REQUEST_SIZE);
            let initial_request_size = rng.gen_range(0..INITIAL_REQUEST_SIZE);
            let max_forward_seek_wait_distance = rng.gen_range(16u64..1 * 1024 * 1024 + 256 * 1024);
            let max_backward_seek_distance = rng.gen_range(16u64..1 * 1024 * 1024 + 256 * 1024);

            let client = Arc::new(
                MockClient::config()
                    .bucket("test-bucket")
                    .part_size(part_size)
                    .enable_backpressure(true)
                    .initial_read_window_size(part_size)
                    .build(),
            );
            let pool = PagedPool::config()
                .with_candidate_sizes([CandidateSize::new(part_size)])
                .with_minimum_memory_limit()
                .build();
            let object = MockObject::ramp(0xaa, object_size as usize, ETag::for_tests());
            let file_etag = object.etag();

            client.add_object("hello", object);

            let prefetcher_config = PrefetcherConfig {
                max_read_window_size,
                sequential_prefetch_multiplier,
                max_forward_seek_wait_distance,
                max_backward_seek_distance,
                initial_request_size,
            };

            let prefetcher =
                Prefetcher::default_builder(client).build(Runtime::new(ShuttleRuntime), pool, prefetcher_config);
            let object_id = ObjectId::new("hello".to_owned(), file_etag);
            let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, object_size);

            let mut next_offset = 0;
            loop {
                let read_size = rng.gen_range(1usize..1 * 1024 * 1024);
                let buf = block_on(request.read(next_offset, read_size)).unwrap();
                if buf.is_empty() {
                    break;
                }
                let buf = buf.into_bytes().unwrap();
                let expected = ramp_bytes((0xaa + next_offset) as usize, buf.len());
                assert_eq!(&buf[..], &expected[..buf.len()]);
                next_offset += buf.len() as u64;
            }
            assert_eq!(next_offset, object_size);
        }

        // The default coroutine stack size in shuttle 0.9 is too small for these tests.
        fn shuttle_config() -> Config {
            let mut config = Config::default();
            config.stack_size = 256 * 1024;
            config
        }

        #[test]
        fn sequential_read_stress() {
            Runner::new(RandomScheduler::new(1000), shuttle_config()).run(sequential_read_stress_helper);
            Runner::new(PctScheduler::new(3, 1000), shuttle_config()).run(sequential_read_stress_helper);
        }

        fn random_read_stress_helper() {
            let mut rng = shuttle::rand::thread_rng();
            let max_read_window_size = rng.gen_range(16usize..32 * 1024);
            let sequential_prefetch_multiplier = rng.gen_range(2usize..16);
            let part_size = rng.gen_range(16usize..128 * 1024);
            let initial_request_size = rng.gen_range(16usize..128 * 1024);
            let max_forward_seek_wait_distance = rng.gen_range(16u64..192 * 1024);
            let max_backward_seek_distance = rng.gen_range(16u64..192 * 1024);
            // Try to prevent testing very small reads of very large objects, which are easy to OOM
            // under Shuttle (lots of concurrent tasks)
            let max_object_size = initial_request_size.min(max_read_window_size) * 20;
            let object_size = rng.gen_range(1u64..(64 * 1024).min(max_object_size) as u64);

            let client = Arc::new(
                MockClient::config()
                    .bucket("test-bucket")
                    .part_size(part_size)
                    .enable_backpressure(true)
                    .initial_read_window_size(part_size)
                    .build(),
            );
            let pool = PagedPool::config()
                .with_candidate_sizes([CandidateSize::new(part_size)])
                .with_minimum_memory_limit()
                .build();
            let object = MockObject::ramp(0xaa, object_size as usize, ETag::for_tests());
            let file_etag = object.etag();

            client.add_object("hello", object);

            let prefetcher_config = PrefetcherConfig {
                max_read_window_size,
                sequential_prefetch_multiplier,
                max_forward_seek_wait_distance,
                max_backward_seek_distance,
                initial_request_size,
            };

            let prefetcher =
                Prefetcher::default_builder(client).build(Runtime::new(ShuttleRuntime), pool, prefetcher_config);
            let object_id = ObjectId::new("hello".to_owned(), file_etag);
            let mut request = prefetcher.prefetch("test-bucket".to_owned(), object_id, object_size);

            let num_reads = rng.gen_range(10usize..50);
            for _ in 0..num_reads {
                let offset = rng.gen_range(0u64..object_size);
                let length = rng.gen_range(1usize..(object_size - offset + 1) as usize);
                let expected = ramp_bytes((0xaa + offset) as usize, length);
                let buf = block_on(request.read(offset, length)).unwrap();
                let buf = buf.into_bytes().unwrap();
                assert_eq!(buf.len(), expected.len());
                // Don't spew the giant buffer if this test fails
                if buf[..] != expected[..] {
                    for i in 0..buf.len() {
                        if buf[i] != expected[i] {
                            panic!(
                                "buffer mismatch at offset {}, saw {} expected {}",
                                i, buf[i], expected[i]
                            );
                        }
                    }
                }
            }
        }

        #[test]
        fn random_read_stress() {
            Runner::new(RandomScheduler::new(1000), shuttle_config()).run(random_read_stress_helper);
            Runner::new(PctScheduler::new(3, 1000), shuttle_config()).run(random_read_stress_helper);
        }
    }
}
