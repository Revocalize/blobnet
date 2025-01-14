//! Configurable, async storage providers for blob access.

use std::collections::HashMap;
use std::fs::File;
use std::future::Future;
use std::io::{self, Cursor, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use auto_impl::auto_impl;
use aws_sdk_s3::{
    error::{GetObjectErrorKind, HeadObjectErrorKind},
    types::SdkError,
};
use cadence_macros::*;
use hashlink::LinkedHashMap;
use hyper::{body::Bytes, client::connect::Connect};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tempfile::tempfile;
use tokio::sync::broadcast::Sender;
use tokio::{fs, io::AsyncReadExt, sync::broadcast, task, time};
use tokio_stream::StreamExt;
use tokio_util::io::StreamReader;

use crate::client::FileClient;
use crate::fast_aio::file_reader;
use crate::utils::{atomic_copy, hash_path, stream_body};
use crate::{read_to_bytes_with_fit, BlobRange, BlobRead, Error, ReadStream};

/// Specifies a storage backend for the blobnet service.
///
/// Each method returns an error only when some operational problem occurs, such
/// as in I/O or communication. Retries should be handled internally by the
/// function since each provider has different failure modes.
///
/// This trait was designed to support flexible combinators that can be used to
/// add caching or fallbacks to providers.
#[async_trait]
#[auto_impl(&, Box, Arc)]
pub trait Provider: Send + Sync {
    /// Check if a file exists and returns its size in bytes.
    ///
    /// Equivalent to:
    ///
    /// ```ignore
    /// async fn head(&self, hash: &str) -> Result<u64, Error>;
    /// ```
    async fn head(&self, hash: &str) -> Result<u64, Error>;

    /// Returns the data from the file at the given path.
    ///
    /// Equivalent to:
    ///
    /// ```ignore
    /// async fn get(&self, hash: &str, range: Option<(u64, u64)>) -> Result<Read<'static>, Error>;
    /// ```
    async fn get(&self, hash: &str, range: BlobRange) -> Result<BlobRead<'static>, Error>;

    /// Adds a binary blob to storage, returning its hash.
    ///
    /// This function is not as latency-sensitive as the others, caring more
    /// about throughput. It may take two passes over the data.
    ///
    /// Equivalent to:
    ///
    /// ```ignore
    /// async fn put(&self, data: ReadStream<'_>) -> Result<String, Error>;
    /// ```
    async fn put(&self, data: ReadStream<'_>) -> Result<String, Error>;
}

/// A provider that stores blobs in memory, only used for testing.
#[derive(Default)]
pub struct Memory {
    data: parking_lot::RwLock<HashMap<String, Bytes>>,
}

impl Memory {
    /// Create a new, empty in-memory storage.
    pub fn new() -> Self {
        Default::default()
    }
}

#[async_trait]
impl Provider for Memory {
    async fn head(&self, hash: &str) -> Result<u64, Error> {
        let data = self.data.read();
        let bytes = data.get(hash).ok_or(Error::NotFound)?;
        Ok(bytes.len() as u64)
    }

    async fn get(&self, hash: &str, range: BlobRange) -> Result<BlobRead<'static>, Error> {
        check_range(range)?;
        let data = self.data.read();
        let mut bytes = match data.get(hash) {
            Some(bytes) => bytes.clone(),
            None => return Err(Error::NotFound),
        };
        if let Some((start, end)) = range {
            if start > bytes.len() as u64 {
                return Ok(empty_read());
            }
            bytes = bytes.slice(start as usize..bytes.len().min(end as usize));
        }
        Ok(BlobRead::from_bytes(bytes))
    }

    async fn put(&self, mut data: ReadStream<'_>) -> Result<String, Error> {
        let mut buf = Vec::new();
        data.read_to_end(&mut buf).await?;
        let hash = format!("{:x}", Sha256::new().chain_update(&buf).finalize());
        self.data.write().insert(hash.clone(), Bytes::from(buf));
        Ok(hash)
    }
}

/// A provider that stores blobs in an S3 bucket.
pub struct S3 {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl S3 {
    /// Creates a new S3 provider.
    pub async fn new(client: aws_sdk_s3::Client, bucket: &str) -> anyhow::Result<Self> {
        client
            .head_bucket()
            .bucket(bucket)
            .send()
            .await
            .with_context(|| format!("unable to create provider for S3 bucket {bucket}"))?;
        Ok(Self {
            client,
            bucket: bucket.into(),
        })
    }
}

#[async_trait]
impl Provider for S3 {
    async fn head(&self, hash: &str) -> Result<u64, Error> {
        let key = hash_path(hash)?;
        let result = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;

        match result {
            Ok(resp) => Ok(resp.content_length() as u64),
            Err(SdkError::ServiceError { err, .. })
                if matches!(err.kind, HeadObjectErrorKind::NotFound(_)) =>
            {
                Err(Error::NotFound)
            }
            Err(err) => Err(Error::Internal(err.into())),
        }
    }

    async fn get(&self, hash: &str, range: BlobRange) -> Result<BlobRead<'static>, Error> {
        check_range(range)?;

        if matches!(range, Some((s, e)) if s == e) {
            // Special case: The range has length 0, and S3 doesn't support
            // zero-length ranges directly, so we need a different request.
            self.head(hash).await?;
            return Ok(empty_read());
        }

        let key = hash_path(hash)?;
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .set_range(range.map(|(start, end)| format!("bytes={}-{}", start, end - 1)))
            .send()
            .await;

        match result {
            Ok(resp) => Ok(BlobRead::from_stream(resp.body.into_async_read())),
            Err(SdkError::ServiceError { err, .. })
                if matches!(err.kind, GetObjectErrorKind::NoSuchKey(_)) =>
            {
                Err(Error::NotFound)
            }
            // InvalidRange isn't supported on the `GetObjectErrorKind` enum.
            Err(SdkError::ServiceError { err, .. }) if err.code() == Some("InvalidRange") => {
                // Edge case: S3 throws errors if the start of the range is at or after the
                // end of the file, but we want to support this for consistency.
                Ok(empty_read())
            }
            Err(err) => Err(Error::Internal(err.into())),
        }
    }

    async fn put(&self, data: ReadStream<'_>) -> Result<String, Error> {
        let mut digest = Sha256::new();
        let file = make_data_tempfile(data, Some(&mut digest)).await?;
        let hash = format!("{:x}", digest.finalize());
        let body = stream_body(file_reader(file, None));
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(hash_path(&hash)?)
            .checksum_sha256(base64::encode(hex::decode(&hash).unwrap()))
            .body(body.into())
            .send()
            .await
            .map_err(anyhow::Error::from)?;
        Ok(hash)
    }
}

/// A provider that stores blobs in a local directory.
///
/// This is especially useful when targeting network file systems mounts.
pub struct LocalDir {
    dir: PathBuf,
}

impl LocalDir {
    /// Creates a new local directory provider.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            dir: path.as_ref().to_owned(),
        }
    }
}

#[async_trait]
impl Provider for LocalDir {
    async fn head(&self, hash: &str) -> Result<u64, Error> {
        let key = hash_path(hash)?;
        let path = self.dir.join(key);
        match fs::metadata(&path).await {
            Ok(metadata) => Ok(metadata.len()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Err(Error::NotFound),
            Err(err) => Err(err.into()),
        }
    }

    async fn get(&self, hash: &str, range: BlobRange) -> Result<BlobRead<'static>, Error> {
        check_range(range)?;
        let key = hash_path(hash)?;
        let path = self.dir.join(key);
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(Error::NotFound),
            Err(err) => return Err(err.into()),
        };
        Ok(BlobRead::Stream(file_reader(file, range)))
    }

    async fn put(&self, data: ReadStream<'_>) -> Result<String, Error> {
        let mut digest = Sha256::new();
        let file = make_data_tempfile(data, Some(&mut digest)).await?;
        let hash = format!("{:x}", digest.finalize());
        let key = hash_path(&hash)?;
        let path = self.dir.join(key);
        task::spawn_blocking(move || atomic_copy(file, path))
            .await
            .map_err(anyhow::Error::from)??;
        Ok(hash)
    }
}

/// A provider that routes requests to a remote blobnet server.
pub struct Remote<C> {
    client: FileClient<C>,
}

impl<C> Remote<C> {
    /// Construct a new remote provider using the given client.
    pub fn new(client: FileClient<C>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl<C: Connect + Clone + Send + Sync + 'static> Provider for Remote<C> {
    async fn head(&self, hash: &str) -> Result<u64, Error> {
        self.client.head(hash).await
    }

    async fn get(&self, hash: &str, range: BlobRange) -> Result<BlobRead<'static>, Error> {
        self.client.get(hash, range).await
    }

    async fn put(&self, data: ReadStream<'_>) -> Result<String, Error> {
        let file = make_data_tempfile(data, None).await?;
        let file = Arc::new(file);
        self.client
            .put(|| async { Ok(stream_body(file_reader(Arc::clone(&file), None))) })
            .await
    }
}

/// Stream data from a source into a temporary file and optionally compute the
/// data stream's hash.
async fn make_data_tempfile(
    mut data: ReadStream<'_>,
    mut digest: Option<&mut Sha256>,
) -> anyhow::Result<File> {
    let mut file = task::spawn_blocking(tempfile).await??;
    loop {
        let mut buf = Vec::with_capacity(1 << 21);
        let size = data.read_buf(&mut buf).await?;
        if size == 0 {
            break;
        }
        if let Some(ref mut d) = digest {
            d.update(&buf);
        }
        // Hack needed for ownership issues with spawn_blocking being 'static.
        file = task::spawn_blocking(move || file.write_all(&buf).map(|_| file)).await??;
    }
    file = task::spawn_blocking(move || file.rewind().map(|_| file)).await??;
    Ok(file)
}

fn check_range(range: BlobRange) -> Result<(), Error> {
    match range {
        Some((start, end)) if start > end => Err(Error::BadRange),
        _ => Ok(()),
    }
}

fn empty_read() -> BlobRead<'static> {
    BlobRead::from_bytes(Bytes::from_static(b"" as &[u8]))
}

// A pair of providers is also a provider, acting as a fallback.
//
// This is useful for gradually migrating between two providers without
// downtime. Note that PUT requests are only sent to the primary provider.
#[async_trait]
impl<P1: Provider, P2: Provider> Provider for (P1, P2) {
    async fn head(&self, hash: &str) -> Result<u64, Error> {
        match self.0.head(hash).await {
            Ok(res) => Ok(res),
            Err(_) => self.1.head(hash).await,
        }
    }

    async fn get(&self, hash: &str, range: BlobRange) -> Result<BlobRead<'static>, Error> {
        match self.0.get(hash, range).await {
            Ok(res) => Ok(res),
            Err(_) => self.1.get(hash, range).await,
        }
    }

    async fn put(&self, data: ReadStream<'_>) -> Result<String, Error> {
        self.0.put(data).await
    }
}

/// A provider wrapper that caches data locally.
pub struct Cached<P> {
    state: Arc<CachedState<P>>,
    prefetch_depth: u32,
}

/// Constant cost associated with every entry in the page cache.
const PAGE_CACHE_ENTRY_COST: u64 = 80;
/// Maximum time to wait for a pending cache population request.
const MAX_PENDING_REQUEST_WAIT_MS: u64 = 60_000;

/// An in-memory, two-stage LRU based page cache.
struct PageCache {
    /// A mapping from `(hash, offset)` pairs to shared references to data.
    mapping: LinkedHashMap<(String, u64), Bytes>,
    /// The total cached pages' size, plus a fixed [`PAGE_CACHE_ENTRY_COST`].
    total_cost: u64,
    /// The maximum cost size of the page cache in bytes.
    total_capacity: u64,
}

impl PageCache {
    /// Insert an entry into the page cache, with LRU eviction.
    fn insert(&mut self, hash: String, n: u64, bytes: Bytes) {
        use hashlink::linked_hash_map::Entry;

        match self.mapping.entry((hash, n)) {
            Entry::Occupied(mut o) => o.to_back(),
            Entry::Vacant(v) => {
                v.insert(bytes.clone());
                self.total_cost += bytes.len() as u64 + PAGE_CACHE_ENTRY_COST;
                while self.total_cost > self.total_capacity {
                    // This should never panic because of a data structure invariant. If we reached
                    // this line, the total cost in the page cache must be nonzero, so there must be
                    // still pages in the mapping.
                    let (_, bytes) = self.mapping.pop_front().expect("cache with cost items");
                    self.total_cost -= bytes.len() as u64 + PAGE_CACHE_ENTRY_COST;
                }
            }
        }
    }

    /// Get an entry from the page cache, with LRU eviction.
    fn get(&mut self, hash: String, n: u64) -> Option<Bytes> {
        use hashlink::linked_hash_map::Entry;

        match self.mapping.entry((hash, n)) {
            Entry::Occupied(mut o) => {
                o.to_back();
                Some(o.get().clone())
            }
            Entry::Vacant(_) => None,
        }
    }

    /// Get an entry from the page cache, without LRU eviction.
    fn peek(&mut self, hash: String, n: u64) -> Option<Bytes> {
        self.mapping.get(&(hash, n)).map(Bytes::clone)
    }
}

impl Default for PageCache {
    fn default() -> Self {
        Self {
            mapping: LinkedHashMap::new(),
            total_cost: 0,
            total_capacity: 1 << 26, // 64 MiB in-memory page cache
        }
    }
}

type RequestKey = (String, u64);
type PendingRequest = broadcast::Receiver<Result<Bytes, Error>>;

struct CachedState<P> {
    inner: P,
    page_cache: Mutex<PageCache>,
    pending_requests: Mutex<HashMap<RequestKey, PendingRequest>>,
    dir: PathBuf,
    pagesize: u64,
    diskcache_semaphore: tokio::sync::Semaphore,
    diskcache_pending_write_pages: AtomicU64,
    diskcache_pending_write_bytes: AtomicU64,
}

/// Stats of `CachedState`
#[non_exhaustive]
#[derive(Debug)]
pub struct CacheStats {
    /// Number of pages pending writing to disk cache
    pub pending_disk_write_pages: u64,

    /// Bytes pending writing to disk cache
    pub pending_disk_write_bytes: u64,

    /// Number of pending requests
    pub pending_requests: u64,
}

impl<P: Provider + 'static> Cached<P> {
    /// Create a new cache wrapper for the given provider.
    ///
    /// Set the page size in bytes for cached chunks, as well as the directory
    /// where fragments should be stored.
    pub fn new(inner: P, dir: impl AsRef<Path>, pagesize: u64) -> Self {
        assert!(pagesize >= 4096, "pagesize must be at least 4096");
        Self {
            state: Arc::new(CachedState {
                inner,
                page_cache: Default::default(),
                pending_requests: Default::default(),
                dir: dir.as_ref().to_owned(), // File system cache
                pagesize,
                diskcache_semaphore: tokio::sync::Semaphore::new(1),
                diskcache_pending_write_pages: Default::default(),
                diskcache_pending_write_bytes: Default::default(),
            }),
            prefetch_depth: 0,
        }
    }

    /// Prefetch the N-ahead chunk. Setting to 0 implies no prefetching.
    pub fn set_prefetch_depth(&mut self, n: u32) {
        self.prefetch_depth = n;
    }

    /// A background process that periodically cleans the cache directory.
    ///
    /// Since the cache directory is limited in size but local to the machine,
    /// it is acceptable to delete files from this folder at any time.
    /// Therefore, we can simply remove 1/(256^2) of all files at an
    /// interval of 60 seconds.
    ///
    /// Doing the math, it would take (256^2) / 60 / 24 = ~46 days on average to
    /// expire any given file from the disk cache directory.
    pub fn cleaner(&self) -> impl Future<Output = ()> {
        let state = Arc::clone(&self.state);
        async move { state.cleaner().await }
    }

    /// A background process that logs stats
    pub fn stats_logger(&self) -> impl Future<Output = ()> {
        let state = Arc::clone(&self.state);
        async move { state.stats_logger().await }
    }

    /// A background process that emits stats to statsd
    pub fn stats_emitter(&self) -> impl Future<Output = ()> {
        let state = Arc::clone(&self.state);
        async move { state.stats_emitter().await }
    }

    /// Get a snapshot of current stats like volume of pending disk writes.
    pub fn stats(&self) -> CacheStats {
        self.state.stats()
    }
}

impl<P: Provider + 'static> CachedState<P> {
    /// Run a function with both file system and memory caches.
    ///
    /// The first cache page of each hash stores HEAD metadata. After that,
    /// index `n` stores the byte range from `(n - 1) * pagesize` to `n *
    /// pagesize`.
    async fn with_cache<F, Out>(
        self: &Arc<Self>,
        hash: String,
        n: u64,
        func: F,
    ) -> Result<Bytes, Error>
    where
        F: (Fn(Arc<Self>, String, u64) -> Out) + Send + Clone + 'static,
        Out: Future<Output = Result<Bytes, Error>> + Send,
    {
        if let Some(bytes) = self.page_cache.lock().get(hash.clone(), n) {
            return Ok(bytes);
        }

        // Check for pending requests to the same hash and offset and deduplicate them
        // if found, listening for the immediate following response.
        let key = (hash.clone(), n);
        let mut receiver = {
            use std::collections::hash_map::Entry::*;
            match self.pending_requests.lock().entry(key.clone()) {
                Occupied(o) => o.get().resubscribe(),
                Vacant(v) => {
                    // Spawn a task to fetch the chunk to insulate from request cancellation
                    let (tx, rx) = broadcast::channel(1);
                    let state = Arc::clone(self);
                    let receiver = rx.resubscribe();
                    let hash = hash.clone();
                    let func = func.clone();
                    task::spawn(async move { state.fetch(hash, n, func, tx).await });
                    v.insert(rx);
                    receiver
                }
            }
        };

        match time::timeout(
            Duration::from_millis(MAX_PENDING_REQUEST_WAIT_MS),
            receiver.recv(),
        )
        .await
        {
            Ok(result) => match result {
                Ok(r) => r,
                Err(_) => {
                    _ = self.pending_requests.lock().remove(&key); // Remove the dropped key
                    Err(anyhow!(
                        "pending request channel dropped hash={} n={}",
                        hash.clone(),
                        n
                    )
                    .into())
                }
            },
            Err(_) => {
                eprintln!("timeout waiting on pending request {key:?}");
                func(self.clone(), hash.clone(), n).await
            }
        }
    }

    /// Run a cached pending request function and broadcast result to waiters
    async fn fetch<F, Out>(
        self: &Arc<Self>,
        hash: String,
        n: u64,
        func: F,
        tx: Sender<Result<Bytes, Error>>,
    ) where
        F: (Fn(Arc<Self>, String, u64) -> Out) + Send + 'static,
        Out: Future<Output = Result<Bytes, Error>> + Send,
    {
        let result = self.try_fetch(&hash, n, func).await;

        // Finish all blocked, pending requests.
        let key = (hash.clone(), n);
        if let Some(_rx) = self.pending_requests.lock().remove(&key) {
            tx.send(result).ok();
        }
    }

    /// Try to run a cached request function
    async fn try_fetch<F, Out>(
        self: &Arc<Self>,
        hash: &str,
        n: u64,
        func: F,
    ) -> Result<Bytes, Error>
    where
        F: (Fn(Arc<Self>, String, u64) -> Out) + Send + 'static,
        Out: Future<Output = Result<Bytes, Error>> + Send,
    {
        let path = self.page_disk_path(hash, n)?;
        let hash = hash.to_owned();

        // Attempt fetch from diskcache first, falling back to function.
        let (result, hit_diskcache) = if let Ok(data) = fs::read(&path).await {
            (Ok(Bytes::from(data)), true)
        } else {
            let f = func(self.clone(), hash.clone(), n);
            (f.await, false)
        };

        // Populate in-memory and disk caches.
        if let Ok(bytes) = &result {
            self.page_cache
                .lock()
                .insert(hash.clone(), n, bytes.clone());
            if !hit_diskcache {
                self.populate_disk_cache(hash, n, func, bytes.len() as u64);
            }
        };

        result
    }

    /// Asynchronously populate the disk cache with the specified hash chunk
    fn populate_disk_cache<F, Out>(self: &Arc<Self>, hash: String, n: u64, func: F, bytes_len: u64)
    where
        F: (Fn(Arc<Self>, String, u64) -> Out) + Send + 'static,
        Out: Future<Output = Result<Bytes, Error>> + Send,
    {
        self.diskcache_pending_write_pages.fetch_add(1, Relaxed);
        self.diskcache_pending_write_bytes
            .fetch_add(bytes_len, Relaxed);

        let state = scopeguard::guard(self.clone(), move |state| {
            state.diskcache_pending_write_pages.fetch_sub(1, Relaxed);
            state
                .diskcache_pending_write_bytes
                .fetch_sub(bytes_len, Relaxed);
        });

        tokio::spawn(async move {
            // Throttle disk writes to not impact user read request latency
            if let Ok(_permit) = state.diskcache_semaphore.acquire().await {
                state._populate_disk_cache(hash, n, func).await;
            }
        });
    }

    async fn _populate_disk_cache<F, Out>(self: &Arc<Self>, hash: String, n: u64, func: F)
    where
        F: (Fn(Arc<Self>, String, u64) -> Out) + Send + 'static,
        Out: Future<Output = Result<Bytes, Error>> + Send,
    {
        let path = match self.page_disk_path(&hash, n) {
            // Bail if the page is already on disk
            Ok(path) if fs::metadata(&path).await.is_ok() => return,
            Ok(path) => path,
            Err(err) => {
                eprintln!("error computing page disk path: {err:?}");
                return;
            }
        };

        // Get the page from page cache if present
        let bytes = self.page_cache.lock().peek(hash.clone(), n);
        let bytes = if let Some(bytes) = bytes {
            Some(bytes)
        } else {
            // Fall back to re-fetching the page
            let f = func(self.clone(), hash, n);
            f.await
                .map_err(|err| {
                    eprintln!("error getting {path:?} cache contents: {err:?}");
                    err
                })
                .ok()
        };

        // Write the page to disk
        if let Some(bytes) = bytes {
            task::spawn_blocking(move || {
                let read_buf = Cursor::new(bytes);
                if let Err(err) = atomic_copy(read_buf, &path) {
                    eprintln!("error writing {path:?} cache file: {err:?}");
                }
            })
            .await
            .ok();
        }
    }

    fn page_disk_path(self: &Arc<Self>, hash: &str, n: u64) -> Result<PathBuf, Error> {
        let diskcache_key = hash_path(hash)?;
        let path = self.dir.join(format!("{diskcache_key}/{n}"));
        Ok(path)
    }

    async fn cleaner(&self) {
        const CLEAN_INTERVAL: Duration = Duration::from_secs(30);
        loop {
            time::sleep(CLEAN_INTERVAL).await;
            let prefix = fastrand::u16(..);
            let (d1, d2) = (prefix / 256, prefix % 256);
            let subfolder = self.dir.join(format!("{d1:x}/{d2:x}"));
            if fs::metadata(&subfolder).await.is_ok() {
                println!("cleaning cache directory: {}", subfolder.display());
                let subfolder_tmp = self.dir.join(format!("{d1:x}/.tmp-{d2:x}"));
                fs::remove_dir_all(&subfolder_tmp).await.ok();
                if fs::rename(&subfolder, &subfolder_tmp).await.is_ok() {
                    fs::remove_dir_all(&subfolder_tmp).await.ok();
                }
            }
        }
    }

    async fn stats_logger(&self) {
        let interval = Duration::from_millis(
            std::env::var("BLOBNET_STATS_LOG_INTERVAL_MS")
                .map_or(30_000, |s| s.parse::<u64>().unwrap()),
        );
        loop {
            time::sleep(interval).await;
            let stats = self.stats();
            println!("{stats:?}");
        }
    }

    async fn stats_emitter(&self) {
        let interval = Duration::from_millis(
            std::env::var("BLOBNET_STATS_EMIT_INTERVAL_MS")
                .map_or(5_000, |s| s.parse::<u64>().unwrap()),
        );
        loop {
            time::sleep(interval).await;
            let stats = self.stats();
            statsd_gauge!("cache.pending_requests", stats.pending_requests);
            statsd_gauge!(
                "cache.pending_disk_write_bytes",
                stats.pending_disk_write_bytes
            );
            statsd_gauge!(
                "cache.pending_disk_write_pages",
                stats.pending_disk_write_pages
            );
        }
    }

    fn stats(&self) -> CacheStats {
        CacheStats {
            pending_disk_write_bytes: self.diskcache_pending_write_bytes.load(Relaxed),
            pending_disk_write_pages: self.diskcache_pending_write_pages.load(Relaxed),
            pending_requests: self.pending_requests.lock().len() as u64,
        }
    }
}

impl<P: Provider + 'static> CachedState<P> {
    /// Read the size of a file, with caching.
    async fn get_cached_size(self: &Arc<Self>, hash: &str) -> Result<u64, Error> {
        let size = self
            .with_cache(hash.into(), 0, |state, hash, _| async move {
                let size = state.inner.head(&hash).await?;
                Ok(Bytes::from_iter(size.to_le_bytes()))
            })
            .await?;
        Ok(u64::from_le_bytes(
            size.as_ref().try_into().map_err(anyhow::Error::from)?,
        ))
    }

    /// Read a chunk of data, with caching.
    async fn get_single_cached_chunk(
        self: &Arc<Self>,
        hash: &str,
        n: u64,
        len: u64,
    ) -> Result<Bytes, Error> {
        assert!(n > 0, "chunks of file data start at 1");

        let lo = (n - 1) * self.pagesize;
        let hi = n * self.pagesize;

        self.with_cache(hash.into(), n, move |state, hash, _| async move {
            let read = state.inner.get(&hash, Some((lo, hi))).await?;
            // Ensure the buffer is fit for saving in the page cache
            Ok(read_to_bytes_with_fit(read, len as usize).await?)
        })
        .await
    }

    /// Read a chunk of data, with caching and optionally prefetching.
    async fn get_cached_chunk(
        self: &Arc<Self>,
        hash: &str,
        n: u64,
        prefetch_depth: u32,
        len: u64,
    ) -> Result<Bytes, Error> {
        if prefetch_depth > 0 {
            let this = Arc::clone(self);
            let hash = hash.to_string();
            let len = self.pagesize;
            tokio::spawn(async move {
                this.get_single_cached_chunk(&hash, n + prefetch_depth as u64, len)
                    .await
                    .ok();
            });
        }
        self.get_single_cached_chunk(hash, n, len).await
    }
}

#[async_trait]
impl<P: Provider + 'static> Provider for Cached<P> {
    async fn head(&self, hash: &str) -> Result<u64, Error> {
        self.state.get_cached_size(hash).await
    }

    async fn get(&self, hash: &str, range: BlobRange) -> Result<BlobRead<'static>, Error> {
        let (start, end) = range.unwrap_or((0, u64::MAX));
        check_range(range)?;

        if start == end {
            // Special case: The range has length 0, so we can't divide it into chunks.
            self.head(hash).await?;
            return Ok(empty_read());
        }

        let chunk_begin: u64 = 1 + start / self.state.pagesize;
        let chunk_end: u64 = 1 + (end - 1) / self.state.pagesize;
        debug_assert!(chunk_begin >= 1);
        debug_assert!(chunk_begin <= chunk_end);

        let prefetch_depth = self.prefetch_depth;

        // Read the first chunk, and return empty data if out of bounds (or NotFound if
        // non-existent). Otherwise, the range should be valid, and we can continue
        // reading until we reach the end of the requested range or get an error.
        let first_chunk = self
            .state
            // TODO(dano): limit len to file size - let the user pass in known file size
            .get_cached_chunk(hash, chunk_begin, prefetch_depth, self.state.pagesize)
            .await?;
        let initial_offset = start - (chunk_begin - 1) * self.state.pagesize;
        if initial_offset > first_chunk.len() as u64 {
            return Ok(empty_read());
        }

        let reached_end = (first_chunk.len() as u64) < self.state.pagesize;
        let first_chunk = first_chunk.slice(initial_offset as usize..);
        // If it fits in a single chunk, just return the data immediately.
        if reached_end || first_chunk.len() as u64 >= end - start {
            let total_len = first_chunk.len().min((end - start) as usize);
            return Ok(BlobRead::from_bytes(first_chunk.slice(..total_len)));
        }
        let remaining_bytes = Arc::new(Mutex::new(end - start - first_chunk.len() as u64));

        let state = Arc::clone(&self.state);
        let hash = hash.to_string();
        let stream = tokio_stream::iter(chunk_begin..=chunk_end).then(move |chunk| {
            let state = Arc::clone(&state);
            let remaining_bytes = Arc::clone(&remaining_bytes);
            let first_chunk = first_chunk.clone();
            let hash = hash.clone();

            async move {
                if chunk == chunk_begin {
                    return Ok::<_, Error>(first_chunk);
                }
                // TODO(dano): limit len to file size
                let bytes = state
                    .get_cached_chunk(&hash, chunk, prefetch_depth, state.pagesize)
                    .await?;
                let mut remaining_bytes = remaining_bytes.lock();
                if bytes.len() as u64 > *remaining_bytes {
                    let result = bytes.slice(..*remaining_bytes as usize);
                    *remaining_bytes = 0;
                    Ok(result)
                } else {
                    *remaining_bytes -= bytes.len() as u64;
                    Ok(bytes)
                }
            }
        });
        let stream = stream.take_while(|result| match result {
            Ok(bytes) => !bytes.is_empty(), // gracefully end the stream
            Err(_) => true,
        });
        Ok(BlobRead::from_stream(StreamReader::new(stream)))
    }

    async fn put(&self, data: ReadStream<'_>) -> Result<String, Error> {
        self.state.inner.put(data).await
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use hyper::body::Bytes;

    use super::{Memory, PageCache, Provider};
    use crate::Error;

    #[test]
    fn page_cache_eviction() {
        let mut cache = PageCache::default();
        let bigpage = Bytes::from(vec![42; 1 << 21]);
        for i in 0..4096 {
            cache.insert(String::new(), i, bigpage.clone());
        }
        assert_eq!(cache.get(String::new(), 0), None);
        assert_eq!(cache.get(String::new(), 4095), Some(bigpage));
        assert!(cache.mapping.len() < 2048);
    }

    #[test]
    fn page_cache_duplicates() {
        let mut cache = PageCache::default();
        let page = Bytes::from(vec![42; 256]);
        for _ in 0..4096 {
            cache.insert(String::new(), 0, page.clone());
        }
        assert_eq!(cache.get(String::new(), 0), Some(page));
        assert!(cache.mapping.len() == 1);
    }

    #[tokio::test]
    async fn fallback_provider() {
        let p = (Memory::new(), Memory::new());
        let hash = p
            .put(Box::pin(Cursor::new(vec![42; 1 << 21])))
            .await
            .unwrap();

        assert!(matches!(p.get(&hash, None).await, Ok(_)));
        assert!(matches!(p.0.get(&hash, None).await, Ok(_)));
        assert!(matches!(p.1.get(&hash, None).await, Err(Error::NotFound)));
    }
}
