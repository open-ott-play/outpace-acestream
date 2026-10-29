//! Registry of live sessions keyed by `(network, id)`. One shared session per key; lazy
//! start via the provider; teardown after the last subscriber leaves + grace.

use crate::config::HlsConfig;
use crate::hls::HlsPackager;
use crate::provider::{ProviderError, ProviderRegistry, TsSource, VodByteSource, VodContent};
use crate::session::{StreamSession, Subscription};
use ace_swarm::types::{StreamInfo, StreamMetadata};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;

/// A resolved VOD plus its last-access time (for idle expiry), keyed by `(network, id)`.
type VodCacheEntry = (Arc<dyn VodContent>, Instant);
type StreamKey = (String, String);

/// A VOD response can outlive its cached descriptor. Its reader must still stop when
/// the manager closes admissions, so HTTP drain does not wait for a stalled range.
struct ShutdownVodSource {
    source: Option<Box<dyn VodByteSource>>,
    // A response may outlive the router that created it. Keep its cancellation owner
    // alive until EOF or body drop; explicit shutdown still interrupts the reader.
    manager: Option<Arc<StreamManager>>,
    length: u64,
    stopping: watch::Receiver<bool>,
}

#[async_trait::async_trait]
impl VodByteSource for ShutdownVodSource {
    fn content_length(&self) -> u64 {
        self.length
    }

    async fn next(&mut self) -> Option<bytes::Bytes> {
        if *self.stopping.borrow() {
            self.source.take();
            self.manager.take();
            return None;
        }
        let source = self.source.as_mut()?;
        let next = tokio::select! {
            biased;
            _ = self.stopping.changed() => None,
            next = source.next() => next,
        };
        if next.is_none() {
            self.source.take();
            self.manager.take();
        }
        next
    }
}

#[derive(Default)]
struct HlsLifecycleState {
    next_generation: u64,
    pending: HashMap<StreamKey, HashSet<u64>>,
    cancelled: HashSet<(StreamKey, u64)>,
}

impl HlsLifecycleState {
    fn finish(&mut self, key: &StreamKey, generation: u64) -> bool {
        let cancelled = self.cancelled.remove(&(key.clone(), generation));
        if let Some(generations) = self.pending.get_mut(key) {
            generations.remove(&generation);
            if generations.is_empty() {
                self.pending.remove(key);
            }
        }
        cancelled
    }
}

struct HlsTransition {
    state: Arc<StdMutex<HlsLifecycleState>>,
    key: StreamKey,
    generation: u64,
    active: bool,
}

impl HlsTransition {
    fn begin(state: Arc<StdMutex<HlsLifecycleState>>, key: StreamKey) -> Self {
        let generation = {
            let mut lifecycle = state.lock().unwrap();
            lifecycle.next_generation = lifecycle
                .next_generation
                .checked_add(1)
                .expect("HLS transition generation overflow");
            let generation = lifecycle.next_generation;
            lifecycle
                .pending
                .entry(key.clone())
                .or_default()
                .insert(generation);
            generation
        };
        Self {
            state,
            key,
            generation,
            active: true,
        }
    }

    fn is_cancelled(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .cancelled
            .contains(&(self.key.clone(), self.generation))
    }

    fn finish(&mut self) -> bool {
        let cancelled = self
            .state
            .lock()
            .unwrap()
            .finish(&self.key, self.generation);
        self.active = false;
        cancelled
    }
}

impl Drop for HlsTransition {
    fn drop(&mut self) {
        if self.active {
            self.state
                .lock()
                .unwrap()
                .finish(&self.key, self.generation);
        }
    }
}

/// Readable media never owns the cancellation or join lifetime.
struct HlsEntry {
    media: Arc<HlsPackager>,
    control: WorkerControl,
}

#[derive(Debug)]
enum CleanupJob {
    Join(JoinHandle<()>, watch::Sender<bool>),
    Close,
}

struct CleanupCoordinator {
    sender: tokio::sync::mpsc::UnboundedSender<CleanupJob>,
    // Shared polling keeps the coordinator join owned if a shutdown caller is cancelled.
    join: futures::future::Shared<futures::future::BoxFuture<'static, ()>>,
}

pub(crate) struct WorkerControl {
    cancel: watch::Sender<bool>,
    join: Option<JoinHandle<()>>,
    completed: watch::Receiver<bool>,
    done: Option<watch::Sender<bool>>,
    cleanup: tokio::sync::mpsc::UnboundedSender<CleanupJob>,
}

impl WorkerControl {
    fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    fn transfer(&mut self) {
        if let Some(join) = self.join.take() {
            // The coordinator is runtime-owned and exits only after every sender closes and
            // every transferred join completes. Neither HTTP cancellation nor Drop aborts it.
            self.cleanup
                .send(CleanupJob::Join(join, self.done.take().unwrap()))
                .expect("cleanup coordinator must live while controls exist");
        }
    }

    async fn join(mut self) {
        self.transfer();
        while !*self.completed.borrow_and_update() {
            if self.completed.changed().await.is_err() {
                break;
            }
        }
    }
}

impl Drop for WorkerControl {
    fn drop(&mut self) {
        self.cancel();
        self.transfer();
    }
}

/// Completion is registered synchronously before spawn/publication. A single coordinator per
/// manager owns transferred joins, rather than adding a monitor task to every worker. Draining
/// borrows registered completion receivers, so cancelling its caller never abandons cleanup.
#[derive(Default)]
pub(crate) struct CleanupTracker {
    pending: StdMutex<Vec<watch::Receiver<bool>>>,
    coordinator: StdMutex<Option<CleanupCoordinator>>,
}

impl CleanupTracker {
    fn coordinator(&self) -> tokio::sync::mpsc::UnboundedSender<CleanupJob> {
        use futures::future::BoxFuture;
        use futures::stream::{FuturesUnordered, StreamExt};
        use futures::FutureExt;
        let mut coordinator = self.coordinator.lock().unwrap();
        coordinator
            .get_or_insert_with(|| {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<CleanupJob>();
                let task = tokio::spawn(async move {
                    let mut joins: FuturesUnordered<BoxFuture<'static, ()>> =
                        FuturesUnordered::new();
                    let mut closed = false;
                    loop {
                        tokio::select! {
                            job = rx.recv(), if !closed => match job {
                                Some(CleanupJob::Join(join, done)) => joins.push(Box::pin(async move {
                                    let _ = join.await;
                                    done.send_replace(true);
                                })),
                                Some(CleanupJob::Close) => rx.close(),
                                None => closed = true,
                            },
                            _ = joins.next(), if !joins.is_empty() => {},
                            else => break,
                        }
                    }
                });
                CleanupCoordinator {
                    sender: tx,
                    join: async move { let _ = task.await; }.boxed().shared(),
                }
            })
            .sender.clone()
    }

    pub(crate) fn spawn<F, Fut>(&self, run: F) -> WorkerControl
    where
        F: FnOnce(watch::Receiver<bool>) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let cleanup = self.coordinator();
        let (cancel, rx) = watch::channel(false);
        let (done, completed) = watch::channel(false);
        {
            let mut pending = self.pending.lock().unwrap();
            pending.retain(|rx| !*rx.borrow());
            pending.push(completed.clone());
        }
        let join = Some(tokio::spawn(run(rx)));
        WorkerControl {
            cancel,
            join,
            done: Some(done),
            completed,
            cleanup,
        }
    }

    pub(crate) async fn drain(&self) {
        loop {
            let pending = self.pending.lock().unwrap().clone();
            for mut done in pending {
                while !*done.borrow_and_update() {
                    if done.changed().await.is_err() {
                        break;
                    }
                }
            }
            let mut pending = self.pending.lock().unwrap();
            pending.retain(|done| !*done.borrow() && done.has_changed().is_ok());
            if pending.is_empty() {
                break;
            }
        }
        let join = self
            .coordinator
            .lock()
            .unwrap()
            .as_ref()
            .map(|coordinator| {
                let _ = coordinator.sender.send(CleanupJob::Close);
                coordinator.join.clone()
            });
        if let Some(join) = join {
            join.await;
        }
    }
}

pub struct StreamManager {
    registry: ProviderRegistry,
    sessions: Mutex<HashMap<(String, String), Arc<StreamSession>>>,
    packagers: Mutex<HashMap<(String, String), HlsEntry>>,
    /// Synchronous pending/cancellation bookkeeping so a dropped future cleans up in `Drop`.
    hls_transitions: Arc<StdMutex<HlsLifecycleState>>,
    /// Serializes only short HLS publication/reap/stop map operations, never provider work.
    hls_finalization: Mutex<()>,
    /// Weak per-key session-start mutexes shared by direct TS and HLS paths. Different keys open
    /// concurrently; same-key callers perform exactly one provider open.
    session_start_locks: StdMutex<HashMap<StreamKey, Weak<Mutex<()>>>>,
    /// Resolved single-file VODs, shared across a playback's many range reads (the HLS manifest
    /// plus every segment). Each entry keeps its provider-resolved handle (and, for the ace
    /// provider, its bounded piece cache + discovered peers) alive so segments resolve the
    /// transport descriptor once and reuse downloaded pieces instead of re-fetching per request.
    /// Entries expire after `grace` of no access. Value: (handle, last-access time).
    vod_cache: Mutex<HashMap<(String, String), VodCacheEntry>>,
    /// Serializes VOD *resolution* so concurrent first reads for the same id (a manifest fetch
    /// racing an eager segment fetch) trigger exactly one `provider.resolve_vod()`.
    vod_resolve_locks: StdMutex<HashMap<StreamKey, Weak<Mutex<()>>>>,
    buffer: usize,
    hls: HlsConfig,
    history: crate::history::HlsHistoryConfig,
    history_budget: crate::history::HistoryBudget,
    normalizer: Option<crate::hls_audio::PreparedNormalizer>,
    grace: Duration,
    cleanup: CleanupTracker,
    reaper: StdMutex<Option<WorkerControl>>,
    shutting_down: std::sync::atomic::AtomicBool,
    shutdown_signal: watch::Sender<bool>,
}

impl StreamManager {
    pub fn new(registry: ProviderRegistry) -> Arc<StreamManager> {
        Self::with_buffer(registry, 256)
    }

    pub fn with_buffer(registry: ProviderRegistry, buffer: usize) -> Arc<StreamManager> {
        Self::with_config(registry, buffer, HlsConfig::default())
    }

    pub fn with_config(
        registry: ProviderRegistry,
        buffer: usize,
        hls: HlsConfig,
    ) -> Arc<StreamManager> {
        Self::with_normalizer(registry, buffer, hls, None)
    }

    pub fn with_normalizer(
        registry: ProviderRegistry,
        buffer: usize,
        hls: HlsConfig,
        normalizer: Option<crate::hls_audio::PreparedNormalizer>,
    ) -> Arc<StreamManager> {
        Self::with_normalizer_and_history(registry, buffer, hls, normalizer, Default::default())
    }

    pub fn with_history_config(
        registry: ProviderRegistry,
        buffer: usize,
        hls: HlsConfig,
        history: crate::history::HlsHistoryConfig,
    ) -> Arc<StreamManager> {
        Self::with_normalizer_and_history(registry, buffer, hls, None, history)
    }

    pub fn with_normalizer_and_history(
        registry: ProviderRegistry,
        buffer: usize,
        hls: HlsConfig,
        normalizer: Option<crate::hls_audio::PreparedNormalizer>,
        history: crate::history::HlsHistoryConfig,
    ) -> Arc<StreamManager> {
        Arc::new(StreamManager {
            registry,
            sessions: Mutex::new(HashMap::new()),
            packagers: Mutex::new(HashMap::new()),
            hls_transitions: Arc::new(StdMutex::new(HlsLifecycleState::default())),
            hls_finalization: Mutex::new(()),
            session_start_locks: StdMutex::new(HashMap::new()),
            vod_cache: Mutex::new(HashMap::new()),
            vod_resolve_locks: StdMutex::new(HashMap::new()),
            buffer,
            hls,
            history,
            history_budget: crate::history::HistoryBudget::new(history.global_max_bytes),
            normalizer,
            grace: Duration::from_secs(30),
            cleanup: CleanupTracker::default(),
            reaper: StdMutex::new(None),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            shutdown_signal: watch::channel(false).0,
        })
    }

    #[cfg(test)]
    pub(crate) fn buffer(&self) -> usize {
        self.buffer
    }

    pub(crate) fn hls_config(&self) -> HlsConfig {
        self.hls
    }

    /// Managed origins and native packagers share one configured retention budget.
    pub(crate) fn history_context(
        &self,
    ) -> (
        crate::history::HlsHistoryConfig,
        crate::history::HistoryBudget,
    ) {
        (self.history, self.history_budget.clone())
    }

    pub(crate) fn hls_startup_timeout(&self) -> Duration {
        Duration::from_millis(self.hls.startup_timeout_ms)
    }

    /// Dropping the operation releases its per-key locks and transition guards. The
    /// publication fence remains authoritative if shutdown races a completed open.
    async fn while_running<T>(
        &self,
        operation: impl Future<Output = Result<T, ProviderError>>,
    ) -> Result<T, ProviderError> {
        let mut stopping = self.shutdown_signal.subscribe();
        if *stopping.borrow() {
            return Err(ProviderError::NotFound);
        }
        let result = tokio::select! {
            biased;
            _ = stopping.changed() => Err(ProviderError::NotFound),
            result = operation => result,
        };
        if *stopping.borrow() {
            return Err(ProviderError::NotFound);
        }
        result
    }

    pub(crate) async fn open_vod_range(
        self: &Arc<Self>,
        content: &dyn VodContent,
        start: u64,
        end: u64,
    ) -> Result<Box<dyn VodByteSource>, ProviderError> {
        let source = self.while_running(content.open_range(start, end)).await?;
        let length = source.content_length();
        Ok(Box::new(ShutdownVodSource {
            source: Some(source),
            manager: Some(self.clone()),
            length,
            stopping: self.shutdown_signal.subscribe(),
        }))
    }

    /// Offline pre-check that `id` can be opened on `network`; see
    /// [`StreamProvider::check_openable`](crate::provider::StreamProvider::check_openable).
    /// `NotFound` if the network is unregistered.
    pub fn check_openable(&self, network: &str, id: &str) -> Result<(), ProviderError> {
        self.registry
            .get(network)
            .ok_or(ProviderError::NotFound)?
            .check_openable(id)
    }

    /// Hand a live descriptor resolved outside `open` to `network`'s provider; see
    /// [`StreamProvider::remember_live_descriptor`](crate::provider::StreamProvider::remember_live_descriptor).
    /// Callers must pass only descriptors resolved through the signed catalog. A no-op for an
    /// unregistered network.
    pub fn remember_live_descriptor(&self, network: &str, info: &StreamInfo) {
        if let Some(provider) = self.registry.get(network) {
            provider.remember_live_descriptor(info);
        }
    }

    /// Read already verified live metadata without starting a source or doing network I/O.
    pub fn verified_live_descriptor(
        &self,
        network: &str,
        infohash: &[u8; 20],
    ) -> Option<StreamInfo> {
        self.registry
            .get(network)?
            .verified_live_descriptor(infohash)
            .filter(|info| &info.infohash == infohash)
    }

    /// Cache a live descriptor and its content-id binding from the same signed catalog result.
    pub fn remember_catalog_descriptor(&self, network: &str, content_id: &str, info: &StreamInfo) {
        if let Some(provider) = self.registry.get(network) {
            provider.remember_catalog_descriptor(content_id, info);
        }
    }

    /// Resolve `id` to a single-file VOD, caching the resolved handle per `(network, id)` so a
    /// whole playback — the HLS manifest plus every segment read — shares one resolution (and, for
    /// the ace provider, one piece cache + one peer discovery) instead of re-resolving per request.
    /// Returns `NotFound` for an unregistered network.
    pub async fn resolve_vod(
        &self,
        network: &str,
        id: &str,
    ) -> Result<Arc<dyn VodContent>, ProviderError> {
        self.while_running(self.resolve_vod_inner(network, id))
            .await
    }

    async fn resolve_vod_inner(
        &self,
        network: &str,
        id: &str,
    ) -> Result<Arc<dyn VodContent>, ProviderError> {
        let key = (network.to_string(), id.to_string());
        if let Some(v) = self.cached_vod(&key).await {
            return Ok(v);
        }
        // Serialize resolution and re-check: a concurrent request may have resolved this id while
        // we waited, so we must not resolve (and discover) a second time.
        let resolve_lock = Self::key_lock(&self.vod_resolve_locks, &key);
        let _resolving = resolve_lock.lock().await;
        if let Some(v) = self.cached_vod(&key).await {
            return Ok(v);
        }
        let provider = self.registry.get(network).ok_or(ProviderError::NotFound)?;
        let vod: Arc<dyn VodContent> = Arc::from(provider.resolve_vod(id).await?);
        let _finalizing = self.hls_finalization.lock().await;
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(ProviderError::NotFound);
        }
        self.vod_cache
            .lock()
            .await
            .insert(key, (vod.clone(), Instant::now()));
        Ok(vod)
    }

    /// A live (non-expired) cached VOD for `key`, refreshing its access time. Sweeps entries idle
    /// longer than `grace` on the way, so the cache is bounded to VODs read within the last `grace`.
    async fn cached_vod(&self, key: &(String, String)) -> Option<Arc<dyn VodContent>> {
        let mut map = self.vod_cache.lock().await;
        let now = Instant::now();
        map.retain(|_, (_, at)| now.duration_since(*at) < self.grace);
        map.get_mut(key).map(|(v, at)| {
            *at = now;
            v.clone()
        })
    }

    fn session_start_lock(&self, key: &StreamKey) -> Arc<Mutex<()>> {
        Self::key_lock(&self.session_start_locks, key)
    }

    fn key_lock(
        locks: &StdMutex<HashMap<StreamKey, Weak<Mutex<()>>>>,
        key: &StreamKey,
    ) -> Arc<Mutex<()>> {
        let mut locks = locks.lock().unwrap();
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
            return lock;
        }

        let lock = Arc::new(Mutex::new(()));
        locks.insert(key.clone(), Arc::downgrade(&lock));
        lock
    }

    /// Reuse a live source, or the generation published by another start while this caller
    /// waited. A finite replacement may already have ended: queued callers still share that
    /// one acquisition instead of reopening once per waiter. Only a later fresh acquisition
    /// that observed this ended generation on entry may replace it again.
    fn reuse_session(current: &Arc<StreamSession>, observed: Option<&Arc<StreamSession>>) -> bool {
        !current.is_finished() || observed.is_none_or(|old| !Arc::ptr_eq(old, current))
    }

    /// Get the running session for `(network, id)` or start one via the provider. Returns
    /// `NotFound` if the network is unregistered.
    pub async fn get_or_start(
        self: &Arc<Self>,
        network: &str,
        id: &str,
    ) -> Result<Arc<StreamSession>, ProviderError> {
        self.while_running(self.get_or_start_inner(network, id))
            .await
    }

    async fn get_or_start_inner(
        self: &Arc<Self>,
        network: &str,
        id: &str,
    ) -> Result<Arc<StreamSession>, ProviderError> {
        let key = (network.to_string(), id.to_string());
        let observed = self.sessions.lock().await.get(&key).cloned();
        if let Some(session) = observed.as_ref().filter(|s| !s.is_finished()) {
            return Ok(session.clone());
        }
        // Serialize same-key starts and re-check: direct and HLS callers share this lock, while
        // unrelated provider opens remain concurrent.
        let mut transition = HlsTransition::begin(self.hls_transitions.clone(), key.clone());
        let start_lock = self.session_start_lock(&key);
        let _starting = start_lock.lock().await;
        if transition.is_cancelled() {
            return Err(ProviderError::NotFound);
        }
        {
            let map = self.sessions.lock().await;
            if let Some(s) = map
                .get(&key)
                .filter(|s| Self::reuse_session(s, observed.as_ref()))
            {
                return Ok(s.clone());
            }
        }
        let provider = self.registry.get(network).ok_or(ProviderError::NotFound)?;
        let source = provider.open(id).await?;
        let session = StreamSession::start(source, self.buffer);
        // Publication ends the pending transition before old-worker cleanup finishes.
        // Keep the replacement owned until this request can attach its real receiver.
        let _transition_pin = session.pin();
        // A completed generation may still have clients draining its queued data. Publish
        // the new source without reusing its old HLS worker or segment namespace.
        let obsolete = {
            let _finalizing = self.hls_finalization.lock().await;
            if transition.finish()
                || self
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(ProviderError::NotFound);
            }
            self.sessions
                .lock()
                .await
                .insert(key.clone(), session.clone());
            let obsolete = self.packagers.lock().await.remove(&key);
            if let Some(entry) = &obsolete {
                entry.media.stop();
                entry.control.cancel();
            }
            obsolete
        };
        if let Some(entry) = obsolete {
            entry.control.join().await;
        }
        Ok(session)
    }

    /// Peek at an already-running session without starting one.
    pub async fn get(&self, network: &str, id: &str) -> Option<Arc<StreamSession>> {
        self.sessions
            .lock()
            .await
            .get(&(network.to_string(), id.to_string()))
            .cloned()
    }

    /// Get (or lazily start) a native HLS packager, recording native demand before it is returned.
    pub async fn get_or_start_hls(
        self: &Arc<Self>,
        network: &str,
        id: &str,
    ) -> Result<Arc<HlsPackager>, ProviderError> {
        let (pkg, _pin) = self.get_or_start_hls_with_pin(network, id).await?;
        Ok(pkg)
    }

    /// Keep native first-playlist startup alive until its request finishes or is cancelled.
    pub(crate) async fn get_or_start_hls_with_pin(
        self: &Arc<Self>,
        network: &str,
        id: &str,
    ) -> Result<(Arc<HlsPackager>, Subscription), ProviderError> {
        let (pkg, _session, pin) = self.get_or_start_pinned_hls(network, id, true).await?;
        Ok((pkg, pin))
    }

    /// Get (or lazily start) a compatibility HLS packager without recording native activity.
    /// The returned subscription pins the session across creation and must be transferred
    /// directly to the compatibility lease owner.
    pub async fn get_or_start_compatibility_hls(
        self: &Arc<Self>,
        network: &str,
        id: &str,
    ) -> Result<(Arc<HlsPackager>, Arc<StreamSession>, Subscription), ProviderError> {
        self.get_or_start_pinned_hls(network, id, false).await
    }

    async fn get_or_start_pinned_hls(
        self: &Arc<Self>,
        network: &str,
        id: &str,
        native: bool,
    ) -> Result<(Arc<HlsPackager>, Arc<StreamSession>, Subscription), ProviderError> {
        self.while_running(self.get_or_start_pinned_hls_inner(network, id, native))
            .await
    }

    async fn get_or_start_pinned_hls_inner(
        self: &Arc<Self>,
        network: &str,
        id: &str,
        native: bool,
    ) -> Result<(Arc<HlsPackager>, Arc<StreamSession>, Subscription), ProviderError> {
        let key = (network.to_string(), id.to_string());
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(ProviderError::NotFound);
        }
        let observed = self.sessions.lock().await.get(&key).cloned();
        let mut transition = HlsTransition::begin(self.hls_transitions.clone(), key.clone());
        let start_lock = self.session_start_lock(&key);

        // Serialize only starts for this key. Unrelated provider opens, reap, and stop proceed.
        let _starting = start_lock.lock().await;
        if transition.is_cancelled() {
            return Err(ProviderError::NotFound);
        }

        let existing_session = self
            .sessions
            .lock()
            .await
            .get(&key)
            .filter(|session| Self::reuse_session(session, observed.as_ref()))
            .cloned();
        let mut source: Option<Box<dyn TsSource>> = if existing_session.is_none() {
            let provider = self.registry.get(network).ok_or(ProviderError::NotFound)?;
            Some(provider.open(id).await?)
        } else {
            None
        };

        // Finalization is atomic with reap/stop, but contains no provider or other long I/O.
        let _finalizing = self.hls_finalization.lock().await;
        if transition.finish()
            || self
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(ProviderError::NotFound);
        }

        // Whether to replace is decided before provider I/O. A source that ends while we
        // hold the per-key start lock can still be drained; a later acquisition replaces it.
        let replacing = existing_session.is_none();
        let session = {
            let mut sessions = self.sessions.lock().await;
            if let Some(session) = existing_session {
                session
            } else {
                let session = StreamSession::start(
                    source
                        .take()
                        .expect("an absent HLS session must have an opened source"),
                    self.buffer,
                );
                sessions.insert(key.clone(), session.clone());
                session
            }
        };
        let pkg = {
            let mut packagers = self.packagers.lock().await;
            if replacing {
                if let Some(obsolete) = packagers.remove(&key) {
                    obsolete.media.stop();
                    obsolete.control.cancel();
                    // WorkerControl transfers unfinished joins to the manager's cleanup
                    // tracker on drop; no provider work runs under this map lock.
                }
            }
            packagers
                .entry(key)
                .or_insert_with(|| {
                    let media = HlsPackager::create_with_history(
                        &session,
                        self.hls,
                        self.normalizer.is_some(),
                        self.history,
                        self.history_budget.clone(),
                    );
                    let control = crate::hls_audio::spawn_worker(
                        &session,
                        media.clone(),
                        self.hls,
                        self.normalizer.clone(),
                        &self.cleanup,
                    );
                    HlsEntry { media, control }
                })
                .media
                .clone()
        };
        // The worker attaches its receiver synchronously before this subscription can
        // start a new source, even if acquiring the packager map had to wait.
        let transition_pin = session.subscribe();
        if native {
            pkg.record_native_access();
        }
        Ok((pkg, session, transition_pin))
    }

    /// Peek at an already-running HLS packager without starting a session or packager.
    pub async fn get_hls(&self, network: &str, id: &str) -> Option<Arc<HlsPackager>> {
        self.packagers
            .lock()
            .await
            .get(&(network.to_string(), id.to_string()))
            .map(|entry| entry.media.clone())
    }

    /// Force-stop a session: cancel its source and remove it (and any HLS packager), even if
    /// callers retain session handles. Returns `true` if a session for `(network, id)` existed.
    /// Connected clients see the stream end after draining any queued events.
    pub async fn stop(&self, network: &str, id: &str) -> bool {
        let key = (network.to_string(), id.to_string());
        let (entry, existed) = {
            let _finalizing = self.hls_finalization.lock().await;
            {
                let mut lifecycle = self.hls_transitions.lock().unwrap();
                let pending = lifecycle
                    .pending
                    .get(&key)
                    .into_iter()
                    .flatten()
                    .copied()
                    .collect::<Vec<_>>();
                for generation in pending {
                    lifecycle.cancelled.insert((key.clone(), generation));
                }
            }
            let entry = self.packagers.lock().await.remove(&key);
            if let Some(entry) = &entry {
                entry.media.stop();
                entry.control.cancel();
            }
            let removed = self.sessions.lock().await.remove(&key);
            if let Some(session) = &removed {
                session.stop();
            }
            let existed = removed.is_some();
            (entry, existed)
        };
        if let Some(entry) = entry {
            entry.control.join().await;
        }
        existed
    }

    /// Active sessions as `(network, id, subscriber_count, metadata)`.
    pub async fn list(&self) -> Vec<(String, String, u64, StreamMetadata)> {
        self.sessions
            .lock()
            .await
            .iter()
            .map(|((n, i), s)| {
                (
                    n.clone(),
                    i.clone(),
                    s.subscriber_count(),
                    s.metadata().clone(),
                )
            })
            .collect()
    }

    async fn reap_idle_at(&self, now: Instant) {
        self.vod_cache
            .lock()
            .await
            .retain(|_, (_, accessed)| now.saturating_duration_since(*accessed) < self.grace);
        let controls = {
            let _finalizing = self.hls_finalization.lock().await;
            let pending_hls: HashSet<_> = self
                .hls_transitions
                .lock()
                .unwrap()
                .pending
                .keys()
                .cloned()
                .collect();
            let active_hls: HashSet<_> = {
                let packagers = self.packagers.lock().await;
                packagers
                    .iter()
                    .filter(|(_, pkg)| pkg.media.was_accessed_within(now, self.grace))
                    .map(|(key, _)| key.clone())
                    .collect()
            };

            let retained_sessions: HashSet<_> = {
                let mut sessions = self.sessions.lock().await;
                sessions.retain(|key, session| {
                    session.subscriber_count() > 0
                        || active_hls.contains(key)
                        || pending_hls.contains(key)
                });
                sessions.keys().cloned().collect()
            };

            let mut entries = self.packagers.lock().await;
            let removed: Vec<_> = entries
                .keys()
                .filter(|key| !retained_sessions.contains(*key))
                .cloned()
                .collect();
            let mut controls = Vec::new();
            for key in removed {
                if let Some(entry) = entries.remove(&key) {
                    entry.media.stop();
                    entry.control.cancel();
                    controls.push(entry.control);
                }
            }
            controls
        };
        // A child reap can wait on OS cleanup indefinitely. Transfer its join to
        // the existing coordinator so it cannot delay later idle sweeps for
        // unrelated sources. Explicit stop/shutdown still await owned cleanup.
        drop(controls);
    }

    /// The reaper holds only a Weak manager, including while sleeping.
    pub fn spawn_reaper(self: &Arc<Self>) {
        let mut reaper = self.reaper.lock().unwrap();
        if reaper.is_some()
            || self
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let manager = Arc::downgrade(self);
        let grace = self.grace;
        *reaper = Some(self.cleanup.spawn(move |mut cancel| async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.changed() => break,
                    _ = tokio::time::sleep(grace) => {}
                }
                let Some(manager) = manager.upgrade() else {
                    break;
                };
                manager.reap_idle_at(Instant::now()).await;
            }
        }));
    }

    /// Fence publication and transfer cancellation under locks; join outside every lifecycle lock.
    pub async fn shutdown(&self) {
        let mut controls = Vec::new();
        {
            let _finalizing = self.hls_finalization.lock().await;
            self.shutting_down
                .store(true, std::sync::atomic::Ordering::Release);
            self.shutdown_signal.send_replace(true);
            if let Some(reaper) = self.reaper.lock().unwrap().take() {
                reaper.cancel();
                controls.push(reaper);
            }
            let mut entries = self.packagers.lock().await;
            for entry in entries.values() {
                entry.media.stop();
                entry.control.cancel();
            }
            controls.extend(entries.drain().map(|(_, entry)| entry.control));
            let mut sessions = self.sessions.lock().await;
            for session in sessions.values() {
                session.stop();
            }
            sessions.clear();
            drop(sessions);
            self.vod_cache.lock().await.clear();
        }
        for control in controls {
            control.join().await;
        }
        self.cleanup.drain().await;
    }
}

impl Drop for StreamManager {
    fn drop(&mut self) {
        self.shutdown_signal.send_replace(true);
        for session in self.sessions.get_mut().values() {
            session.stop();
        }
        if let Some(reaper) = self.reaper.get_mut().unwrap().as_ref() {
            reaper.cancel();
        }
        for entry in self.packagers.get_mut().values() {
            entry.media.stop();
            entry.control.cancel();
        }
    }
}

#[cfg(test)]
mod tests_shutdown;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{SourceStats, StreamProvider, TsSource};
    use crate::testprovider::TestProvider;
    use async_trait::async_trait;
    use tokio::sync::Notify;

    struct IdleSource;

    #[async_trait]
    impl TsSource for IdleSource {
        async fn next(&mut self) -> Option<bytes::Bytes> {
            std::future::pending().await
        }

        fn stats(&self) -> SourceStats {
            SourceStats::default()
        }
    }

    struct BlockingProvider {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl StreamProvider for BlockingProvider {
        fn network(&self) -> &'static str {
            "blocking"
        }

        async fn open(&self, id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
            if id == "fast" {
                return Ok(Box::new(IdleSource));
            }
            self.started.notify_one();
            self.release.notified().await;
            Ok(Box::new(IdleSource))
        }
    }

    fn blocking_manager() -> (Arc<StreamManager>, Arc<Notify>, Arc<Notify>) {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(BlockingProvider {
            started: started.clone(),
            release: release.clone(),
        }));
        (StreamManager::new(registry), started, release)
    }

    fn registry() -> ProviderRegistry {
        let mut r = ProviderRegistry::new();
        r.register(Arc::new(TestProvider { chunks: 1000 }));
        r
    }

    struct FeedSource {
        rx: tokio::sync::mpsc::UnboundedReceiver<(bool, bytes::Bytes)>,
        discontinuity: bool,
    }

    #[async_trait]
    impl TsSource for FeedSource {
        async fn next(&mut self) -> Option<bytes::Bytes> {
            let (discontinuity, bytes) = self.rx.recv().await?;
            self.discontinuity = discontinuity;
            Some(bytes)
        }
        fn take_discontinuity(&mut self) -> bool {
            std::mem::take(&mut self.discontinuity)
        }
        fn stats(&self) -> SourceStats {
            SourceStats::default()
        }
    }

    struct FeedProvider(
        StdMutex<Option<tokio::sync::mpsc::UnboundedReceiver<(bool, bytes::Bytes)>>>,
    );

    #[async_trait]
    impl StreamProvider for FeedProvider {
        fn network(&self) -> &'static str {
            "normalization-test"
        }
        async fn open(&self, _id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
            Ok(Box::new(FeedSource {
                rx: self.0.lock().unwrap().take().expect("one source open"),
                discontinuity: false,
            }))
        }
    }

    fn task4_input(video_only: bool) -> Vec<u8> {
        let fixture = crate::hls_audio::process::test_fixtures::transport_prefix();
        let mut input = fixture[..2 * 188].to_vec();
        if video_only {
            let mut pmt = vec![
                0x02, 0xb0, 0x00, 0x00, 0x09, 0xc1, 0, 0, 0xe1, 0x00, 0xf0, 0, 0x1b, 0xe1, 0x00,
                0xf0, 0,
            ];
            let section_length = pmt.len() - 3 + 4;
            pmt[1] = 0xb0 | ((section_length >> 8) as u8 & 0x0f);
            pmt[2] = section_length as u8;
            let crc = ace_media::mpegts::mpeg_crc32(&pmt);
            pmt.extend_from_slice(&crc.to_be_bytes());
            input[188..376].copy_from_slice(&crate::hls_audio::process::test_fixtures::psi_packet(
                4096, 2, &pmt,
            ));
        }
        input
            .extend_from_slice(&crate::hls_audio::process::test_fixtures::video_access_packet(256));
        for pcr in [0, 108_000] {
            input.extend_from_slice(&crate::hls_audio::process::test_fixtures::pcr_packet(
                256, pcr,
            ));
        }
        input
    }

    fn task4_mpeg_audio_input() -> Vec<u8> {
        let mut input = crate::hls_audio::process::test_fixtures::transport_prefix();
        let section =
            crate::hls_audio::process::test_fixtures::pmt_section(9, 256, 256, 0x1b, 257, 0x03);
        input[188..376].copy_from_slice(&crate::hls_audio::process::test_fixtures::psi_packet(
            4096, 2, &section,
        ));
        input.truncate(2 * 188);
        for packet in crate::hls_audio::process::test_fixtures::mpeg_layer2_audio_packets(257) {
            input.extend_from_slice(&packet);
        }
        input
            .extend_from_slice(&crate::hls_audio::process::test_fixtures::video_access_packet(256));
        for pcr in [0, 108_000, 216_000] {
            input.extend_from_slice(&crate::hls_audio::process::test_fixtures::pcr_packet(
                256, pcr,
            ));
        }
        input
    }

    #[tokio::test]
    async fn hls_audio_native_aac_and_video_only_skip_child_even_with_invalid_prepared_path() {
        let _serial = crate::hls_audio::process::test_support::process_test_lock().await;
        for (id, video_only) in [("native-aac", false), ("video-only", true)] {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let mut providers = ProviderRegistry::new();
            providers.register(Arc::new(FeedProvider(StdMutex::new(Some(rx)))));
            let hls = HlsConfig {
                segment_packets: 128,
                window_segments: 4,
                segment_duration_ms: 1000,
                startup_segments: 1,
                startup_timeout_ms: 5000,
            };
            let manager = StreamManager::with_normalizer(
                providers,
                32,
                hls,
                Some(crate::hls_audio::PreparedNormalizer {
                    executable: std::path::PathBuf::from("__missing_task4_ffmpeg_binary__"),
                }),
            );
            let media = manager
                .get_or_start_hls("normalization-test", id)
                .await
                .unwrap();
            tx.send((false, bytes::Bytes::from(task4_input(video_only))))
                .unwrap();
            let status = tokio::time::timeout(
                Duration::from_secs(4),
                media.wait_available(Duration::from_secs(4)),
            )
            .await
            .expect("native source should become ready without invoking FFmpeg");
            assert_eq!(status, crate::hls::HlsStatus::Ready, "case {id}");
            let completed = media.segment(0).expect("completed native segment");
            drop(tx);
            tokio::time::timeout(Duration::from_secs(1), async {
                while !media
                    .playlist_with_segment_prefix("/segments")
                    .contains("#EXT-X-ENDLIST")
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("source EOF must finish retained native HLS media");
            assert_eq!(media.status(), crate::hls::HlsStatus::Ready);
            assert_eq!(media.segment(0).as_deref(), Some(completed.as_ref()));
            manager.shutdown().await;
            assert_eq!(media.status(), crate::hls::HlsStatus::Stopped);
        }
    }

    #[tokio::test]
    async fn hls_audio_manager_source_recovery_retains_old_segment_and_emits_one_new_boundary() {
        let _serial = crate::hls_audio::process::test_support::process_test_lock().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(FeedProvider(StdMutex::new(Some(rx)))));
        let manager = StreamManager::with_normalizer(
            providers,
            32,
            HlsConfig {
                segment_packets: 128,
                window_segments: 4,
                segment_duration_ms: 1000,
                startup_segments: 1,
                startup_timeout_ms: 5000,
            },
            Some(crate::hls_audio::PreparedNormalizer {
                executable: std::path::PathBuf::from("__missing_task4_ffmpeg_binary__"),
            }),
        );
        let media = manager
            .get_or_start_hls("normalization-test", "recovering")
            .await
            .unwrap();
        tx.send((false, bytes::Bytes::from(task4_input(false))))
            .unwrap();
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(4),
                media.wait_available(Duration::from_secs(4))
            )
            .await
            .unwrap(),
            crate::hls::HlsStatus::Ready,
            "first native segment is available",
        );
        let old = media.segment(0).expect("segment retained before recovery");

        let mut marker = [0xff; 188];
        marker[..6].copy_from_slice(&[0x47, 0x40, 0x11, 0x30, 1, 0x80]);
        tx.send((true, bytes::Bytes::copy_from_slice(&marker)))
            .unwrap(); // paired marker on filtered SDT PID before opening
        tokio::time::timeout(Duration::from_secs(2), async {
            while media.status() != crate::hls::HlsStatus::Recovering {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("source discontinuity reaches the worker before resumed packets");
        tx.send((false, bytes::Bytes::from(task4_input(false))))
            .unwrap();

        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(4),
                media.wait_available(Duration::from_secs(4))
            )
            .await
            .unwrap(),
            crate::hls::HlsStatus::Ready,
            "recovered generation becomes ready",
        );
        assert_eq!(
            media.segment(0).as_deref(),
            Some(old.as_ref()),
            "old retained sequence remains byte-identical"
        );
        let new = media.segment(1).expect("new generation segment");
        assert_eq!(
            ace_media::mpegts::ts_pid(&new[..188]),
            0,
            "new segment starts with fresh PAT"
        );
        assert_eq!(
            ace_media::mpegts::ts_pid(&new[188..376]),
            4096,
            "new segment follows with fresh PMT"
        );
        let playlist = media.playlist_with_segment_prefix("/segments");
        assert_eq!(playlist.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn hls_audio_enabled_worker_ignores_version_repeat_then_recovers_semantic_layout_change()
    {
        let _serial = crate::hls_audio::process::test_support::process_test_lock().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(FeedProvider(StdMutex::new(Some(rx)))));
        let manager = StreamManager::with_normalizer(
            providers,
            32,
            HlsConfig {
                segment_packets: 128,
                window_segments: 4,
                segment_duration_ms: 1000,
                startup_segments: 1,
                startup_timeout_ms: 5000,
            },
            Some(crate::hls_audio::PreparedNormalizer {
                executable: std::path::PathBuf::from("__missing_task4_ffmpeg_binary__"),
            }),
        );
        let media = manager
            .get_or_start_hls("normalization-test", "layout-worker")
            .await
            .unwrap();
        tx.send((false, bytes::Bytes::from(task4_input(false))))
            .unwrap();
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(3),
                media.wait_available(Duration::from_secs(3))
            )
            .await
            .unwrap(),
            crate::hls::HlsStatus::Ready,
        );

        let mut version_repeat = task4_input(false);
        version_repeat[10] = 0xc3; // PAT version 1; selected program unchanged.
        let pat_packet: &mut [u8; 188] = (&mut version_repeat[..188]).try_into().unwrap();
        crate::hls_audio::process::test_fixtures::set_psi_crc(pat_packet);
        version_repeat[188 + 10] = 0xc3; // PMT version 1; selected streams unchanged.
        let pmt_packet: &mut [u8; 188] = (&mut version_repeat[188..376]).try_into().unwrap();
        crate::hls_audio::process::test_fixtures::set_psi_crc(pmt_packet);
        let before_repeat = media.playlist_with_segment_prefix("/segments");
        let before_discontinuities = before_repeat.matches("#EXT-X-DISCONTINUITY\n").count();
        tx.send((false, bytes::Bytes::from(version_repeat)))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            media.status(),
            crate::hls::HlsStatus::Ready,
            "version-only PAT repeat must not invalidate the active worker generation"
        );
        let after_repeat = media.playlist_with_segment_prefix("/segments");
        assert_eq!(
            after_repeat.matches("#EXT-X-DISCONTINUITY\n").count(),
            before_discontinuities,
            "version-only PAT/PMT updates do not create a media boundary"
        );

        let mut semantic_change = crate::hls_audio::process::test_fixtures::transport_prefix();
        let changed_pmt =
            crate::hls_audio::process::test_fixtures::pmt_section(9, 256, 256, 0x1b, 258, 0x0f);
        semantic_change[188..376].copy_from_slice(
            &crate::hls_audio::process::test_fixtures::psi_packet(4096, 2, &changed_pmt),
        );
        tx.send((false, bytes::Bytes::from(semantic_change)))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while media.status() != crate::hls::HlsStatus::Recovering {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("semantic selected-audio PID change invalidates the active generation");
        assert_eq!(media.status(), crate::hls::HlsStatus::Recovering);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn hls_audio_post_opening_filtered_pid_marker_is_independent_after_paired_source_recovery(
    ) {
        let _serial = crate::hls_audio::process::test_support::process_test_lock().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(FeedProvider(StdMutex::new(Some(rx)))));
        let manager = StreamManager::with_normalizer(
            providers,
            32,
            HlsConfig {
                segment_packets: 128,
                window_segments: 4,
                segment_duration_ms: 1000,
                startup_segments: 1,
                startup_timeout_ms: 5000,
            },
            Some(crate::hls_audio::PreparedNormalizer {
                executable: std::path::PathBuf::from("__missing_task4_ffmpeg_binary__"),
            }),
        );
        let media = manager
            .get_or_start_hls("normalization-test", "post-opening-marker")
            .await
            .unwrap();
        tx.send((false, bytes::Bytes::from(task4_input(false))))
            .unwrap();
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(3),
                media.wait_available(Duration::from_secs(3))
            )
            .await
            .unwrap(),
            crate::hls::HlsStatus::Ready,
        );
        let old = media.segment(0).expect("first generation segment");

        let marker = filtered_sdt_discontinuity_packet();
        tx.send((true, bytes::Bytes::copy_from_slice(&marker)))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while media.status() != crate::hls::HlsStatus::Recovering {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("paired source event begins one replacement generation");
        tx.send((false, bytes::Bytes::from(task4_input(false))))
            .unwrap();
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(3),
                media.wait_available(Duration::from_secs(3))
            )
            .await
            .unwrap(),
            crate::hls::HlsStatus::Ready,
        );
        assert_eq!(media.segment(0).as_deref(), Some(old.as_ref()));

        // The paired marker token is consumed at the accepted opening point.
        // A later marker on filtered SDT PID 0x11 must independently invalidate.
        let marker = filtered_sdt_discontinuity_packet();
        tx.send((false, bytes::Bytes::copy_from_slice(&marker)))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while media.status() != crate::hls::HlsStatus::Recovering {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("post-opening filtered-PID marker is independent");
        tx.send((false, bytes::Bytes::from(task4_input(false))))
            .unwrap();
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(3),
                media.wait_available(Duration::from_secs(3))
            )
            .await
            .unwrap(),
            crate::hls::HlsStatus::Ready,
        );
        assert_eq!(media.segment(0).as_deref(), Some(old.as_ref()));
        assert!(
            media.segment(2).is_some(),
            "independent post-opening recovery publishes a third generation segment"
        );
        assert_eq!(
            media
                .playlist_with_segment_prefix("/segments")
                .matches("#EXT-X-DISCONTINUITY\n")
                .count(),
            2
        );
        manager.shutdown().await;
    }

    fn filtered_sdt_discontinuity_packet() -> [u8; 188] {
        let mut marker = [0xff; 188];
        marker[..6].copy_from_slice(&[0x47, 0x40, 0x11, 0x30, 1, 0x80]);
        marker
    }

    #[tokio::test]
    async fn hls_audio_transform_recovery_limit_fences_before_fourth_child_reap() {
        let _serial = crate::hls_audio::process::test_support::process_test_lock().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(FeedProvider(StdMutex::new(Some(rx)))));
        let (prepared, counters) = crate::hls_audio::process::test_support::fake_prepared_child(
            crate::hls_audio::process::test_support::ChildMode::ReadyThenStall,
        );
        let manager = StreamManager::with_normalizer(
            providers,
            32,
            HlsConfig {
                segment_packets: 128,
                window_segments: 4,
                segment_duration_ms: 1000,
                startup_segments: 1,
                startup_timeout_ms: 5000,
            },
            Some(prepared),
        );
        let media = manager
            .get_or_start_hls("normalization-test", "live-transform-recovery-limit")
            .await
            .unwrap();
        let input = bytes::Bytes::from(task4_mpeg_audio_input());
        tx.send((false, input.clone())).unwrap();
        let wait_ready = || async {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if media.status() == crate::hls::HlsStatus::Ready {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("fake transform generation becomes Ready");
        };
        wait_ready().await;
        assert_eq!(counters.snapshot().children_started, 1);

        for generation_child in 2..=4 {
            tx.send((true, bytes::Bytes::new())).unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                while counters.snapshot().children_reaped < generation_child - 1 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("previous transform child is reaped before replacement");
            assert_eq!(counters.snapshot().children_started, generation_child - 1);
            tx.send((false, input.clone())).unwrap();
            wait_ready().await;
            assert_eq!(counters.snapshot().children_started, generation_child);
        }

        // This is the fourth invalidation (the initial generation is not counted).
        // The fourth child is held in ReadyThenStall while Failed must publish before
        // its cancellation/reap completes.
        tx.send((true, bytes::Bytes::new())).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let snapshot = counters.snapshot();
                if media.status()
                    == crate::hls::HlsStatus::Failed(crate::hls::HlsFailure::RecoveryLimit)
                    && snapshot.children_started == 4
                    && snapshot.children_reaped == 3
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("fourth recovery fences Ready media before old child reap");
        manager.shutdown().await;
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.children_started, 4);
        assert_eq!(snapshot.children_reaped, 4);
        assert_eq!(snapshot.pumps_started, snapshot.pumps_joined);
        assert_eq!(snapshot.active_tasks, 0);
    }

    #[tokio::test]
    async fn hls_audio_manager_terminal_transform_failure_is_not_restarted_by_retry() {
        let _serial = crate::hls_audio::process::test_support::process_test_lock().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut providers = ProviderRegistry::new();
        providers.register(Arc::new(FeedProvider(StdMutex::new(Some(rx)))));
        let manager = StreamManager::with_normalizer(
            providers,
            32,
            HlsConfig {
                startup_timeout_ms: 5000,
                ..HlsConfig::default()
            },
            Some(crate::hls_audio::PreparedNormalizer {
                executable: std::path::PathBuf::from("__missing_task4_ffmpeg_binary__"),
            }),
        );
        let media = manager
            .get_or_start_hls("normalization-test", "transform")
            .await
            .unwrap();
        tx.send((false, bytes::Bytes::from(task4_mpeg_audio_input())))
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let status = media.status();
                if matches!(status, crate::hls::HlsStatus::Failed(_)) {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("invalid transform executable should fail terminally");
        assert!(matches!(status, crate::hls::HlsStatus::Failed(_)));
        let retry = manager
            .get_or_start_hls("normalization-test", "transform")
            .await
            .unwrap();
        assert!(
            Arc::ptr_eq(&media, &retry),
            "retry shares the terminal manager entry"
        );
        assert_eq!(
            retry.status(),
            status,
            "an HTTP-style retry cannot restart a failed worker"
        );
        manager.shutdown().await;
    }

    async fn manager_with_blocking_worker_for_test(
    ) -> (Arc<StreamManager>, Arc<Notify>, Arc<Notify>) {
        let manager = StreamManager::new(registry());
        manager
            .get_or_start_hls("test", "slow-cleanup")
            .await
            .unwrap();
        let cleanup_started = Arc::new(Notify::new());
        let release_cleanup = Arc::new(Notify::new());
        let started = cleanup_started.clone();
        let release = release_cleanup.clone();
        let control = manager.cleanup.spawn(move |mut cancel| async move {
            while cancel.changed().await.is_ok() && !*cancel.borrow() {}
            started.notify_one();
            release.notified().await;
        });
        let media = manager.get_hls("test", "slow-cleanup").await.unwrap();
        let old = manager.packagers.lock().await.insert(
            ("test".into(), "slow-cleanup".into()),
            HlsEntry { media, control },
        );
        drop(old);
        (manager, cleanup_started, release_cleanup)
    }

    type SourceResult = Result<Box<dyn TsSource>, ProviderError>;

    struct ReopenProvider {
        sources: Mutex<tokio::sync::mpsc::UnboundedReceiver<SourceResult>>,
        opened: tokio::sync::mpsc::UnboundedSender<()>,
    }

    #[async_trait]
    impl StreamProvider for ReopenProvider {
        fn network(&self) -> &'static str {
            "reopen"
        }

        async fn open(&self, _: &str) -> SourceResult {
            self.opened.send(()).unwrap();
            self.sources.lock().await.recv().await.unwrap()
        }
    }

    struct FinishedSource;

    #[async_trait]
    impl TsSource for FinishedSource {
        async fn next(&mut self) -> Option<bytes::Bytes> {
            None
        }

        fn stats(&self) -> SourceStats {
            SourceStats::default()
        }
    }

    struct ImmediateSource {
        data: Option<bytes::Bytes>,
        pulls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl TsSource for ImmediateSource {
        async fn next(&mut self) -> Option<bytes::Bytes> {
            self.pulls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.data.take()
        }

        fn stats(&self) -> SourceStats {
            SourceStats::default()
        }
    }

    fn reopen_manager() -> (
        Arc<StreamManager>,
        tokio::sync::mpsc::UnboundedSender<SourceResult>,
        tokio::sync::mpsc::UnboundedReceiver<()>,
    ) {
        let (sources, rx) = tokio::sync::mpsc::unbounded_channel();
        let (opened_tx, opened) = tokio::sync::mpsc::unbounded_channel();
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(ReopenProvider {
            sources: Mutex::new(rx),
            opened: opened_tx,
        }));
        (StreamManager::new(registry), sources, opened)
    }

    async fn wait_finished(session: &StreamSession) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while !session.is_finished() {
                // A timer also advances paused test clocks if the source never finishes.
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("synthetic source must reach EOF");
    }

    async fn reopen_acquire(
        manager: &Arc<StreamManager>,
        hls: bool,
    ) -> Result<Arc<StreamSession>, ProviderError> {
        if hls {
            manager
                .get_or_start_compatibility_hls("reopen", "same")
                .await
                .map(|(_, session, _)| session)
        } else {
            manager.get_or_start("reopen", "same").await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn direct_replacement_stays_owned_while_old_hls_cleanup_waits() {
        let (manager, sources, mut opened) = reopen_manager();
        sources.send(Ok(Box::new(FinishedSource))).unwrap();
        let (media, old_session, _old_pin) = manager
            .get_or_start_compatibility_hls("reopen", "same")
            .await
            .unwrap();
        opened.recv().await.unwrap();
        wait_finished(&old_session).await;

        let cleanup_started = Arc::new(Notify::new());
        let release_cleanup = Arc::new(Notify::new());
        let started = cleanup_started.clone();
        let release = release_cleanup.clone();
        let control = manager.cleanup.spawn(move |mut cancel| async move {
            while cancel.changed().await.is_ok() && !*cancel.borrow() {}
            started.notify_one();
            release.notified().await;
        });
        let previous = manager.packagers.lock().await.insert(
            ("reopen".into(), "same".into()),
            HlsEntry { media, control },
        );
        drop(previous);
        let pulls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let initial = bytes::Bytes::from_static(b"initial media");
        sources
            .send(Ok(Box::new(ImmediateSource {
                data: Some(initial.clone()),
                pulls: pulls.clone(),
            })))
            .unwrap();
        let replacement_request = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.get_or_start("reopen", "same").await })
        };
        tokio::time::timeout(Duration::from_secs(1), cleanup_started.notified())
            .await
            .expect("replacement must reach old-worker cleanup");
        assert!(!replacement_request.is_finished());
        // The ownership pin must protect the source without consuming it while the
        // previous worker's cleanup prevents this response from attaching a reader.
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(pulls.load(std::sync::atomic::Ordering::SeqCst), 0);
        manager.reap_idle_at(Instant::now()).await;
        let published = manager
            .get("reopen", "same")
            .await
            .expect("pending direct response must keep replacement registered");
        assert!(!Arc::ptr_eq(&old_session, &published));
        release_cleanup.notify_one();
        let returned = tokio::time::timeout(Duration::from_secs(1), replacement_request)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&published, &returned));
        assert_eq!(returned.subscriber_count(), 0);
        let mut reader = returned.subscribe();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), reader.rx.recv())
                .await
                .unwrap()
                .unwrap(),
            crate::session::StreamEvent::Data(initial)
        );
        wait_finished(&returned).await;
        assert_eq!(pulls.load(std::sync::atomic::Ordering::SeqCst), 2);
        drop(reader);
        manager.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn first_hls_receiver_is_attached_before_source_starts() {
        let (sources, rx) = tokio::sync::mpsc::unbounded_channel();
        let (opened, mut opens) = tokio::sync::mpsc::unbounded_channel();
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(ReopenProvider {
            sources: Mutex::new(rx),
            opened,
        }));
        let manager = StreamManager::with_config(
            registry,
            32,
            HlsConfig {
                segment_packets: 128,
                window_segments: 4,
                segment_duration_ms: 1000,
                startup_segments: 1,
                startup_timeout_ms: 5000,
            },
        );
        let pulls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        sources
            .send(Ok(Box::new(ImmediateSource {
                data: Some(bytes::Bytes::from(task4_input(true))),
                pulls: pulls.clone(),
            }) as Box<dyn TsSource>))
            .unwrap();
        // Hold exactly the lock between source creation and worker attachment. A
        // lease's temporary subscription must not let the source run across this wait.
        let packagers = manager.packagers.lock().await;
        let request = manager.get_or_start_compatibility_hls("reopen", "same");
        tokio::pin!(request);
        assert!(futures::poll!(request.as_mut()).is_pending());
        opens.try_recv().expect("source must have opened");
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(pulls.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(packagers);
        let (media, session, pin) = request.await.unwrap();
        assert_eq!(
            media.wait_available(Duration::from_secs(1)).await,
            crate::hls::HlsStatus::Ready
        );
        wait_finished(&session).await;
        assert_eq!(pulls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(media.segment(0).is_some(), "initial media must reach HLS");
        drop(pin);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn ended_session_reopen_is_singleflight_even_when_replacement_immediately_ends() {
        for first_hls in [false, true] {
            let (manager, sources, mut opened) = reopen_manager();
            sources.send(Ok(Box::new(FinishedSource))).unwrap();
            let (old_pkg, old_session, old_pin) = manager
                .get_or_start_compatibility_hls("reopen", "same")
                .await
                .unwrap();
            opened.recv().await.unwrap();
            wait_finished(&old_session).await;
            assert_ne!(
                old_pkg.wait_available(Duration::from_secs(1)).await,
                crate::hls::HlsStatus::Ready
            );

            let first = reopen_acquire(&manager, first_hls);
            let second = reopen_acquire(&manager, !first_hls);
            tokio::pin!(first, second);
            assert!(futures::poll!(first.as_mut()).is_pending());
            opened.try_recv().expect("one replacement provider open");
            assert!(futures::poll!(second.as_mut()).is_pending());
            assert!(opened.try_recv().is_err(), "second acquisition must wait");

            sources.send(Ok(Box::new(FinishedSource))).unwrap();
            let replacement = first.await.unwrap();
            assert!(!Arc::ptr_eq(&old_session, &replacement));
            if !first_hls {
                assert!(manager.get_hls("reopen", "same").await.is_none());
            }
            // A direct acquisition owns the source but does not consume it until a
            // receiver attaches. Deliberately consume this finite replacement to EOF.
            let _reader = (!first_hls).then(|| replacement.raw_receiver());
            wait_finished(&replacement).await;
            let joined = tokio::time::timeout(Duration::from_secs(1), second)
                .await
                .expect("queued caller must not open another finite source")
                .unwrap();
            assert!(Arc::ptr_eq(&replacement, &joined));
            assert!(opened.try_recv().is_err());
            let new_pkg = manager.get_hls("reopen", "same").await.unwrap();
            assert!(!Arc::ptr_eq(&old_pkg, &new_pkg));
            assert_eq!(old_session.subscriber_count(), 1);
            assert_eq!(replacement.subscriber_count(), 0);

            // A genuinely later request observes the completed replacement and may retry.
            sources.send(Ok(Box::new(IdleSource))).unwrap();
            let fresh = reopen_acquire(&manager, first_hls).await.unwrap();
            assert!(!Arc::ptr_eq(&replacement, &fresh));
            opened.try_recv().expect("fresh acquisition reopens once");
            assert!(!fresh.is_finished());
            drop(old_pin);
            assert_eq!(fresh.subscriber_count(), 0);
        }
    }

    #[tokio::test]
    async fn failed_reopen_preserves_old_generation_then_allows_retry() {
        for hls in [false, true] {
            let (manager, sources, mut opened) = reopen_manager();
            sources.send(Ok(Box::new(FinishedSource))).unwrap();
            let (old_pkg, old, _pin) = manager
                .get_or_start_compatibility_hls("reopen", "same")
                .await
                .unwrap();
            opened.recv().await.unwrap();
            wait_finished(&old).await;

            sources.send(Err(ProviderError::NotFound)).unwrap();
            assert!(reopen_acquire(&manager, hls).await.is_err());
            assert!(Arc::ptr_eq(
                &old,
                &manager.get("reopen", "same").await.unwrap()
            ));
            assert!(Arc::ptr_eq(
                &old_pkg,
                &manager.get_hls("reopen", "same").await.unwrap()
            ));
            assert!(manager.hls_transitions.lock().unwrap().pending.is_empty());

            sources.send(Ok(Box::new(IdleSource))).unwrap();
            let fresh = reopen_acquire(&manager, hls).await.unwrap();
            assert!(!Arc::ptr_eq(&old, &fresh));
            assert!(!fresh.is_finished());
        }
    }

    #[tokio::test]
    async fn cancelled_reopen_preserves_old_client_queue_and_allows_a_later_start() {
        let (manager, sources, mut opened) = reopen_manager();
        sources
            .send(TestProvider { chunks: 2 }.open("finite").await)
            .unwrap();
        let old = reopen_acquire(&manager, false).await.unwrap();
        let mut old_client = old.subscribe();
        opened.recv().await.unwrap();
        wait_finished(&old).await;

        let mut cancelled = Box::pin(reopen_acquire(&manager, true));
        assert!(futures::poll!(cancelled.as_mut()).is_pending());
        opened.try_recv().unwrap();
        drop(cancelled);
        assert!(manager.hls_transitions.lock().unwrap().pending.is_empty());
        assert!(manager.hls_transitions.lock().unwrap().cancelled.is_empty());
        assert!(Arc::ptr_eq(
            &old,
            &manager.get("reopen", "same").await.unwrap()
        ));

        sources.send(Ok(Box::new(IdleSource))).unwrap();
        let fresh = reopen_acquire(&manager, false).await.unwrap();
        assert!(!Arc::ptr_eq(&old, &fresh));
        for _ in 0..2 {
            assert!(matches!(
                old_client.rx.recv().await,
                Ok(crate::session::StreamEvent::Data(_))
            ));
        }
        assert!(matches!(
            old_client.rx.recv().await,
            Err(tokio::sync::broadcast::error::RecvError::Closed)
        ));
        assert_eq!(fresh.subscriber_count(), 0);
        drop(old_client);
        assert_eq!(fresh.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn stop_cancels_reopen_cohort_but_not_a_later_acquisition() {
        for first_hls in [false, true] {
            let (manager, sources, mut opened) = reopen_manager();
            sources.send(Ok(Box::new(FinishedSource))).unwrap();
            let old = reopen_acquire(&manager, true).await.unwrap();
            opened.recv().await.unwrap();
            wait_finished(&old).await;

            let first = reopen_acquire(&manager, first_hls);
            let queued = reopen_acquire(&manager, !first_hls);
            tokio::pin!(first, queued);
            assert!(futures::poll!(first.as_mut()).is_pending());
            opened.try_recv().unwrap();
            assert!(futures::poll!(queued.as_mut()).is_pending());
            assert!(manager.stop("reopen", "same").await);
            assert!(manager.get("reopen", "same").await.is_none());
            assert!(manager.get_hls("reopen", "same").await.is_none());

            let after_stop = reopen_acquire(&manager, !first_hls);
            tokio::pin!(after_stop);
            assert!(futures::poll!(after_stop.as_mut()).is_pending());
            sources.send(Ok(Box::new(IdleSource))).unwrap();
            assert!(first.await.is_err());
            assert!(queued.await.is_err());
            sources.send(Ok(Box::new(IdleSource))).unwrap();
            let fresh = after_stop.await.unwrap();
            assert!(!Arc::ptr_eq(&old, &fresh));
            assert!(Arc::ptr_eq(
                &fresh,
                &manager.get("reopen", "same").await.unwrap()
            ));
            assert!(manager.hls_transitions.lock().unwrap().pending.is_empty());
            assert!(manager.hls_transitions.lock().unwrap().cancelled.is_empty());
        }
    }

    struct ParallelVodProvider {
        entered: tokio::sync::mpsc::UnboundedSender<String>,
        release: Arc<Notify>,
    }

    struct EmptyParallelVod;

    #[async_trait]
    impl VodContent for EmptyParallelVod {
        fn content_length(&self) -> u64 {
            0
        }

        async fn open_range(
            &self,
            _: u64,
            _: u64,
        ) -> Result<Box<dyn crate::provider::VodByteSource>, ProviderError> {
            Err(ProviderError::NotFound)
        }
    }

    #[async_trait]
    impl StreamProvider for ParallelVodProvider {
        fn network(&self) -> &'static str {
            "parallel-vod"
        }

        async fn open(&self, _: &str) -> Result<Box<dyn TsSource>, ProviderError> {
            Ok(Box::new(IdleSource))
        }

        async fn resolve_vod(&self, id: &str) -> Result<Box<dyn VodContent>, ProviderError> {
            self.entered.send(id.to_owned()).unwrap();
            self.release.notified().await;
            Ok(Box::new(EmptyParallelVod))
        }
    }

    fn parallel_vod_manager() -> (
        Arc<StreamManager>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        Arc<Notify>,
    ) {
        let (entered, events) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(ParallelVodProvider {
            entered,
            release: release.clone(),
        }));
        (StreamManager::new(registry), events, release)
    }

    #[tokio::test]
    async fn distinct_vod_resolutions_enter_provider_concurrently() {
        let (manager, mut entered, release) = parallel_vod_manager();
        let first = manager.resolve_vod("parallel-vod", "first");
        let second = manager.resolve_vod("parallel-vod", "second");
        tokio::pin!(first, second);
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(futures::poll!(second.as_mut()).is_pending());
        assert_eq!(entered.try_recv().unwrap(), "first");
        assert_eq!(
            entered
                .try_recv()
                .expect("another key must not wait for the first provider"),
            "second"
        );
        release.notify_waiters();
        assert!(first.await.is_ok());
        assert!(second.await.is_ok());
    }

    #[tokio::test]
    async fn same_key_vod_resolutions_share_one_pending_provider_call() {
        let (manager, mut entered, release) = parallel_vod_manager();
        let first = manager.resolve_vod("parallel-vod", "same");
        let second = manager.resolve_vod("parallel-vod", "same");
        tokio::pin!(first, second);
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(futures::poll!(second.as_mut()).is_pending());
        assert_eq!(entered.try_recv().unwrap(), "same");
        assert!(entered.try_recv().is_err());
        release.notify_waiters();
        let first = first.await.unwrap();
        let second = second.await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(entered.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancelled_vod_resolution_releases_its_key_lock_and_retry_is_not_blocked() {
        let (manager, mut entered, release) = parallel_vod_manager();
        let mut cancelled = Box::pin(manager.resolve_vod("parallel-vod", "cancelled"));
        assert!(futures::poll!(cancelled.as_mut()).is_pending());
        assert_eq!(entered.try_recv().unwrap(), "cancelled");
        drop(cancelled);
        assert!(manager
            .vod_resolve_locks
            .lock()
            .unwrap()
            .values()
            .all(|lock| lock.upgrade().is_none()));

        let mut retry = Box::pin(manager.resolve_vod("parallel-vod", "cancelled"));
        assert!(futures::poll!(retry.as_mut()).is_pending());
        assert_eq!(entered.try_recv().unwrap(), "cancelled");
        // VOD and live protocols do not share their per-key start mutexes.
        assert!(manager
            .get_or_start("parallel-vod", "cancelled")
            .await
            .is_ok());
        release.notify_waiters();
        assert!(retry.await.is_ok());
        assert!(manager
            .vod_resolve_locks
            .lock()
            .unwrap()
            .values()
            .all(|lock| lock.upgrade().is_none()));
    }

    #[tokio::test]
    async fn idle_reaper_releases_vod_cache_without_another_request() {
        use crate::provider::{VodByteSource, VodContent};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct TrackedVod(Arc<AtomicUsize>);
        impl Drop for TrackedVod {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        #[async_trait]
        impl VodContent for TrackedVod {
            fn content_length(&self) -> u64 {
                0
            }
            async fn open_range(
                &self,
                _: u64,
                _: u64,
            ) -> Result<Box<dyn VodByteSource>, ProviderError> {
                Err(ProviderError::NotFound)
            }
        }
        let manager = StreamManager::new(ProviderRegistry::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let now = Instant::now();
        let idle: Arc<dyn VodContent> = Arc::new(TrackedVod(drops.clone()));
        let idle_weak = Arc::downgrade(&idle);
        let active: Arc<dyn VodContent> = Arc::new(TrackedVod(drops.clone()));
        let active_weak = Arc::downgrade(&active);
        let in_use: Arc<dyn VodContent> = Arc::new(TrackedVod(drops.clone()));
        manager.vod_cache.lock().await.extend([
            (
                ("test".to_owned(), "idle".to_owned()),
                (idle, now - manager.grace),
            ),
            (("test".to_owned(), "active".to_owned()), (active, now)),
            (
                ("test".to_owned(), "in-use".to_owned()),
                (in_use.clone(), now - manager.grace),
            ),
        ]);
        manager.reap_idle_at(now).await;
        assert!(
            idle_weak.upgrade().is_none(),
            "the unused cache must be freed without any new request"
        );
        assert!(
            active_weak.upgrade().is_some(),
            "recent access retains its cache"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            Arc::strong_count(&in_use),
            1,
            "expiry removes only the cache owner, preserving an active range reader"
        );
        drop(in_use);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn with_buffer_sets_the_fanout_depth() {
        let reg = ProviderRegistry::new();
        let mgr = StreamManager::with_buffer(reg, 4);
        assert_eq!(mgr.buffer(), 4);
    }

    #[test]
    fn with_config_sets_buffer_and_hls_config() {
        let reg = ProviderRegistry::new();
        let hls = HlsConfig {
            segment_packets: 64,
            window_segments: 4,
            segment_duration_ms: 1500,
            ..HlsConfig::default()
        };
        let mgr = StreamManager::with_config(reg, 8, hls);

        assert_eq!(mgr.buffer(), 8);
        assert_eq!(mgr.hls_config(), hls);
        assert_eq!(
            mgr.hls_startup_timeout(),
            Duration::from_millis(hls.startup_timeout_ms)
        );
    }

    #[tokio::test]
    async fn same_key_returns_same_session() {
        let m = StreamManager::new(registry());
        let s1 = m.get_or_start("test", "abc").await.unwrap();
        let s2 = m.get_or_start("test", "abc").await.unwrap();
        assert!(Arc::ptr_eq(&s1, &s2));
        let s3 = m.get_or_start("test", "different").await.unwrap();
        assert!(!Arc::ptr_eq(&s1, &s3));
    }

    #[tokio::test]
    async fn concurrent_first_requests_start_the_session_once() {
        // A provider that counts how many times `open` is called.
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingProvider(Arc<AtomicUsize>);
        #[async_trait]
        impl StreamProvider for CountingProvider {
            fn network(&self) -> &'static str {
                "count"
            }
            async fn open(&self, _id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                // A small yield so concurrent callers genuinely overlap inside `open`.
                tokio::task::yield_now().await;
                Ok(Box::new(IdleSource))
            }
        }

        let opens = Arc::new(AtomicUsize::new(0));
        let mut r = ProviderRegistry::new();
        r.register(Arc::new(CountingProvider(opens.clone())));
        let m = StreamManager::new(r);

        // Fire many concurrent first requests for the same id.
        let mut handles = Vec::new();
        for _ in 0..16 {
            let m = m.clone();
            handles.push(tokio::spawn(async move {
                m.get_or_start("count", "x")
                    .await
                    .map(|s| Arc::as_ptr(&s) as usize)
            }));
        }
        let mut ptrs = Vec::new();
        for h in handles {
            ptrs.push(h.await.unwrap().unwrap());
        }
        // Exactly one open, and everyone got the same session.
        assert_eq!(
            opens.load(Ordering::SeqCst),
            1,
            "session must be started exactly once"
        );
        assert!(
            ptrs.iter().all(|p| *p == ptrs[0]),
            "all callers share one session"
        );
    }

    #[tokio::test]
    async fn resolve_vod_is_cached_per_key_and_shared() {
        // A provider that counts how many times `resolve_vod` runs, so we can prove one playback's
        // manifest + segment reads resolve the descriptor once and share one resolved VOD.
        use crate::provider::{ProviderError, StreamProvider, TsSource, VodByteSource, VodContent};
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingVodProvider(Arc<AtomicUsize>);
        struct EmptyVod;
        #[async_trait]
        impl VodContent for EmptyVod {
            fn content_length(&self) -> u64 {
                0
            }
            async fn open_range(
                &self,
                _start: u64,
                _end: u64,
            ) -> Result<Box<dyn VodByteSource>, ProviderError> {
                Err(ProviderError::NotFound)
            }
        }
        #[async_trait]
        impl StreamProvider for CountingVodProvider {
            fn network(&self) -> &'static str {
                "cvod"
            }
            async fn open(&self, _id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
                Err(ProviderError::NotFound)
            }
            async fn resolve_vod(&self, _id: &str) -> Result<Box<dyn VodContent>, ProviderError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(EmptyVod))
            }
        }

        let resolves = Arc::new(AtomicUsize::new(0));
        let mut r = ProviderRegistry::new();
        r.register(Arc::new(CountingVodProvider(resolves.clone())));
        let m = StreamManager::new(r);

        let a = m.resolve_vod("cvod", "x").await.unwrap();
        let b = m.resolve_vod("cvod", "x").await.unwrap();
        assert!(Arc::ptr_eq(&a, &b), "same key shares one resolved VOD");
        assert_eq!(
            resolves.load(Ordering::SeqCst),
            1,
            "resolved once for one id"
        );

        // A different id resolves independently.
        let _c = m.resolve_vod("cvod", "y").await.unwrap();
        assert_eq!(resolves.load(Ordering::SeqCst), 2);

        // Unknown network is NotFound, not a cache entry.
        assert!(matches!(
            m.resolve_vod("nope", "x").await,
            Err(ProviderError::NotFound)
        ));
    }

    #[tokio::test]
    async fn get_hls_only_returns_an_already_running_packager() {
        let m = StreamManager::new(registry());
        // Nothing running yet: peeking must not start a session or packager.
        assert!(m.get_hls("test", "a").await.is_none());
        assert!(
            m.get("test", "a").await.is_none(),
            "peeking must not have started a session"
        );
        // Once the packager exists (e.g. after a playlist fetch), peeking finds it.
        m.get_or_start_hls("test", "a").await.unwrap();
        assert!(m.get_hls("test", "a").await.is_some());
    }

    #[tokio::test]
    async fn unknown_network_is_not_found() {
        let m = StreamManager::new(registry());
        assert!(matches!(
            m.get_or_start("nope", "x").await,
            Err(ProviderError::NotFound)
        ));
    }

    #[tokio::test]
    async fn check_openable_is_not_found_for_an_unregistered_network() {
        let manager = StreamManager::new(ProviderRegistry::new());
        assert!(matches!(
            manager.check_openable("nope", "x"),
            Err(ProviderError::NotFound)
        ));
    }

    #[tokio::test]
    async fn check_openable_delegates_to_the_provider_default() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(TestProvider { chunks: 1 }));
        let manager = StreamManager::new(registry);
        assert!(manager.check_openable("test", "anything").is_ok());
    }

    #[tokio::test]
    async fn list_reports_active_sessions() {
        let m = StreamManager::new(registry());
        m.get_or_start("test", "a").await.unwrap();
        let list = m.list().await;
        assert!(list.iter().any(|(n, i, _, metadata)| {
            n == "test" && i == "a" && metadata == &ace_swarm::types::StreamMetadata::default()
        }));
    }

    #[tokio::test]
    async fn hls_owned_stale_media_arc_does_not_keep_worker_alive_after_stop_and_reap() {
        let manager = StreamManager::new(registry());
        let stale_media = manager
            .get_or_start_hls("test", "owned-stale")
            .await
            .unwrap();
        manager.stop("test", "owned-stale").await;

        assert_eq!(stale_media.status(), crate::hls::HlsStatus::Stopped);
        tokio::time::timeout(Duration::from_secs(1), manager.shutdown())
            .await
            .expect("stale media references must not own worker cleanup");
        assert!(manager.get_hls("test", "owned-stale").await.is_none());
    }

    #[tokio::test]
    async fn hls_owned_reap_detaches_worker_even_with_stale_media_arc() {
        let manager = StreamManager::new(registry());
        let stale_media = manager
            .get_or_start_hls("test", "reap-stale")
            .await
            .unwrap();
        manager
            .reap_idle_at(Instant::now() + manager.grace + Duration::from_secs(1))
            .await;
        assert_eq!(stale_media.status(), crate::hls::HlsStatus::Stopped);
        assert!(manager.get_hls("test", "reap-stale").await.is_none());
    }

    #[tokio::test]
    async fn hls_owned_drop_releases_weak_reaper_reference() {
        let manager = StreamManager::new(registry());
        manager.spawn_reaper();
        let weak = Arc::downgrade(&manager);
        drop(manager);
        tokio::task::yield_now().await;
        assert!(
            weak.upgrade().is_none(),
            "reaper must not keep manager alive while sleeping"
        );
    }

    #[tokio::test]
    async fn hls_owned_shutdown_fences_pending_initializer_before_publication() {
        let (manager, started, release) = blocking_manager();
        let initializing = {
            let manager = manager.clone();
            tokio::spawn(async move {
                manager
                    .get_or_start_hls("blocking", "shutdown-pending")
                    .await
            })
        };
        started.notified().await;
        manager.shutdown().await;
        release.notify_one();
        assert!(initializing.await.unwrap().is_err());
        assert!(manager
            .get_hls("blocking", "shutdown-pending")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn hls_owned_native_and_compatibility_starts_share_one_entry() {
        let manager = StreamManager::new(registry());
        let native_start = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.get_or_start_hls("test", "shared-hls").await })
        };
        let compatibility_start = {
            let manager = manager.clone();
            tokio::spawn(async move {
                manager
                    .get_or_start_compatibility_hls("test", "shared-hls")
                    .await
            })
        };
        let native = native_start.await.unwrap().unwrap();
        let (compat, _session, _pin) = compatibility_start.await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&native, &compat));
    }

    #[tokio::test]
    async fn hls_owned_stop_during_delayed_initialization_never_publishes_entry() {
        let (manager, started, release) = blocking_manager();
        let initializing = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.get_or_start_hls("blocking", "delayed").await })
        };
        started.notified().await;
        assert!(!manager.stop("blocking", "delayed").await);
        release.notify_one();
        assert!(initializing.await.unwrap().is_err());
        assert!(manager.get_hls("blocking", "delayed").await.is_none());
    }

    #[tokio::test]
    async fn hls_owned_aborted_initializer_transfers_cleanup_to_manager_tracker() {
        let (manager, started, release) = blocking_manager();
        let initializing = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.get_or_start_hls("blocking", "abort-owned").await })
        };
        started.notified().await;
        initializing.abort();
        assert!(matches!(initializing.await, Err(error) if error.is_cancelled()));
        release.notify_one();

        tokio::time::timeout(Duration::from_secs(1), manager.shutdown())
            .await
            .expect("shutdown must drain cleanup transferred from an aborted initializer");
        assert!(manager.get_hls("blocking", "abort-owned").await.is_none());
    }

    #[tokio::test]
    async fn hls_owned_shutdown_joins_reaper_and_workers_before_returning() {
        let manager = StreamManager::new(registry());
        manager.spawn_reaper();
        manager
            .get_or_start_hls("test", "shutdown-worker")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), manager.shutdown())
            .await
            .expect("shutdown must join the reaper and all owned workers");
        assert!(manager.get_hls("test", "shutdown-worker").await.is_none());
    }

    #[tokio::test]
    async fn hls_owned_shutdown_closes_cleanup_coordinator() {
        let manager = StreamManager::new(registry());
        manager
            .get_or_start_hls("test", "shutdown-coordinator")
            .await
            .unwrap();
        manager.spawn_reaper();
        let coordinator = manager.cleanup.coordinator();

        tokio::time::timeout(Duration::from_secs(1), manager.shutdown())
            .await
            .expect("shutdown must finish before checking coordinator closure");

        assert!(
            coordinator.is_closed(),
            "cleanup coordinator channel must close after shutdown"
        );
    }

    #[tokio::test]
    async fn hls_owned_unrelated_start_progresses_while_cleanup_waits() {
        let (manager, cleanup_started, release_cleanup) =
            manager_with_blocking_worker_for_test().await;
        let stopping = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.stop("test", "slow-cleanup").await })
        };
        cleanup_started.notified().await;
        tokio::task::yield_now().await;
        assert!(
            !stopping.is_finished(),
            "stop must await the detached worker join"
        );
        tokio::time::timeout(
            Duration::from_millis(100),
            manager.get_or_start_hls("test", "unrelated-fast"),
        )
        .await
        .expect("unrelated-key start must proceed outside cleanup waits")
        .unwrap();
        release_cleanup.notify_one();
        stopping.await.unwrap();
    }

    #[tokio::test]
    async fn idle_sweeps_progress_while_cleanup_is_pending_and_shutdown_still_joins() {
        let (manager, cleanup_started, release_cleanup) =
            manager_with_blocking_worker_for_test().await;
        let stale = Instant::now() - manager.grace - Duration::from_secs(1);
        manager
            .get_hls("test", "slow-cleanup")
            .await
            .unwrap()
            .set_last_access_for_test(stale);

        let first_sweep =
            tokio::time::timeout(Duration::from_secs(1), manager.reap_idle_at(Instant::now()))
                .await;
        if first_sweep.is_err() {
            release_cleanup.notify_one();
            manager.shutdown().await;
            panic!("idle sweep must transfer cleanup without waiting for child reap");
        }
        tokio::time::timeout(Duration::from_secs(1), cleanup_started.notified())
            .await
            .expect("removed worker must receive cancellation");
        assert!(manager.get_hls("test", "slow-cleanup").await.is_none());
        let pending = manager.cleanup.pending.lock().unwrap().clone();
        assert!(
            pending.iter().any(|done| !*done.borrow()),
            "the coordinator must retain unfinished cleanup ownership"
        );

        let second = manager
            .get_or_start_hls("test", "later-idle")
            .await
            .unwrap();
        second.set_last_access_for_test(stale);
        tokio::time::timeout(Duration::from_secs(1), manager.reap_idle_at(Instant::now()))
            .await
            .expect("a later idle sweep must progress while prior cleanup is pending");
        assert!(manager.get("test", "later-idle").await.is_none());
        assert!(manager.get_hls("test", "later-idle").await.is_none());
        assert_eq!(second.status(), crate::hls::HlsStatus::Stopped);

        // Cancelling a shutdown waiter must not abandon the transferred join.
        {
            let mut shutdown = Box::pin(manager.shutdown());
            assert!(futures::poll!(shutdown.as_mut()).is_pending());
        }
        assert!(pending.iter().any(|done| !*done.borrow()));
        release_cleanup.notify_one();
        tokio::time::timeout(Duration::from_secs(1), manager.shutdown())
            .await
            .expect("shutdown must join transferred workers after cleanup is released");
        assert!(pending.iter().all(|done| *done.borrow()));
        assert!(manager.cleanup.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn hls_owned_aborted_stop_leaves_join_with_manager_tracker() {
        let (manager, cleanup_started, release_cleanup) =
            manager_with_blocking_worker_for_test().await;
        let stopping = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.stop("test", "slow-cleanup").await })
        };
        cleanup_started.notified().await;
        assert!(
            !stopping.is_finished(),
            "stop should still be joining cleanup"
        );
        stopping.abort();
        assert!(matches!(stopping.await, Err(error) if error.is_cancelled()));
        release_cleanup.notify_one();
        tokio::time::timeout(Duration::from_secs(1), manager.shutdown())
            .await
            .expect("aborting stop must not abandon the manager-tracked join");
    }

    #[tokio::test]
    async fn stop_removes_session_and_is_idempotent() {
        let m = StreamManager::new(registry());
        m.get_or_start("test", "a").await.unwrap();
        assert!(
            m.stop("test", "a").await,
            "first stop removes the running session"
        );
        assert!(
            m.get("test", "a").await.is_none(),
            "session is gone afterwards"
        );
        assert!(
            !m.stop("test", "a").await,
            "stopping a missing session is a no-op"
        );
    }

    #[tokio::test]
    async fn stop_cancels_pending_hls_without_waiting_for_provider() {
        let (m, started, release) = blocking_manager();
        let start = {
            let m = m.clone();
            tokio::spawn(async move { m.get_or_start_hls("blocking", "stopped").await })
        };
        started.notified().await;

        let stopped =
            tokio::time::timeout(Duration::from_millis(50), m.stop("blocking", "stopped"))
                .await
                .expect("stop must not wait for pending provider work");
        assert!(!stopped);

        release.notify_one();
        assert!(start.await.unwrap().is_err());

        assert!(m.get("blocking", "stopped").await.is_none());
        assert!(m.get_hls("blocking", "stopped").await.is_none());
    }

    #[tokio::test]
    async fn stop_cancels_only_hls_generations_already_pending() {
        let (m, started, release) = blocking_manager();
        let old_start = {
            let m = m.clone();
            tokio::spawn(async move { m.get_or_start_hls("blocking", "generation").await })
        };
        started.notified().await;
        assert!(!m.stop("blocking", "generation").await);

        let new_start = {
            let m = m.clone();
            tokio::spawn(async move { m.get_or_start_hls("blocking", "generation").await })
        };
        release.notify_one();
        assert!(old_start.await.unwrap().is_err());

        started.notified().await;
        release.notify_one();
        new_start.await.unwrap().unwrap();

        assert!(m.get("blocking", "generation").await.is_some());
        assert!(m.get_hls("blocking", "generation").await.is_some());
    }

    #[tokio::test]
    async fn aborted_hls_start_does_not_leave_the_key_permanently_pending() {
        let (m, started, release) = blocking_manager();
        let start = {
            let m = m.clone();
            tokio::spawn(async move { m.get_or_start_hls("blocking", "aborted").await })
        };
        started.notified().await;
        start.abort();
        assert!(matches!(start.await, Err(error) if error.is_cancelled()));

        release.notify_one();
        let pkg = m.get_or_start_hls("blocking", "aborted").await.unwrap();
        pkg.set_last_access_for_test(Instant::now() - m.grace - Duration::from_secs(1));
        m.reap_idle_at(Instant::now()).await;

        assert!(m.get("blocking", "aborted").await.is_none());
        assert!(m.get_hls("blocking", "aborted").await.is_none());
    }

    #[tokio::test]
    async fn direct_and_hls_first_start_share_one_provider_open_and_session() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingBlockingProvider {
            opens: Arc<AtomicUsize>,
            first_open: Arc<Notify>,
            second_open: Arc<Notify>,
            release: Arc<Notify>,
        }

        #[async_trait]
        impl StreamProvider for CountingBlockingProvider {
            fn network(&self) -> &'static str {
                "shared-start"
            }

            async fn open(&self, _id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
                let open = self.opens.fetch_add(1, Ordering::SeqCst) + 1;
                if open == 1 {
                    self.first_open.notify_one();
                } else {
                    self.second_open.notify_one();
                }
                self.release.notified().await;
                Ok(Box::new(IdleSource))
            }
        }

        let opens = Arc::new(AtomicUsize::new(0));
        let first_open = Arc::new(Notify::new());
        let second_open = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(CountingBlockingProvider {
            opens: opens.clone(),
            first_open: first_open.clone(),
            second_open: second_open.clone(),
            release: release.clone(),
        }));
        let m = StreamManager::new(registry);

        let direct = {
            let m = m.clone();
            tokio::spawn(async move { m.get_or_start("shared-start", "same").await })
        };
        first_open.notified().await;

        let hls_entered = Arc::new(Notify::new());
        let hls = {
            let m = m.clone();
            let hls_entered = hls_entered.clone();
            tokio::spawn(async move {
                hls_entered.notify_one();
                m.get_or_start_hls("shared-start", "same").await
            })
        };
        hls_entered.notified().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second_open.notified())
                .await
                .is_err(),
            "same-key direct and HLS starts must not open the provider twice"
        );

        release.notify_one();
        release.notify_one();
        let direct_session = direct.await.unwrap().unwrap();
        hls.await.unwrap().unwrap();
        let managed_session = m.get("shared-start", "same").await.unwrap();

        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&direct_session, &managed_session));
    }

    #[tokio::test]
    async fn recent_hls_playlist_access_survives_idle_reap() {
        let m = StreamManager::new(registry());
        let pkg = m.get_or_start_hls("test", "active-hls").await.unwrap();
        let before_access = Instant::now();
        pkg.set_last_access_for_test(before_access - m.grace);
        let _playlist = pkg.playlist("test", "active-hls");

        m.reap_idle_at(before_access + m.grace / 2).await;

        assert!(m.get("test", "active-hls").await.is_some());
        assert!(m.get_hls("test", "active-hls").await.is_some());
    }

    #[tokio::test]
    async fn native_hls_creation_survives_reap_before_playlist_rendering() {
        let m = StreamManager::new(registry());
        m.get_or_start_hls("test", "new-native-hls").await.unwrap();

        m.reap_idle_at(Instant::now()).await;

        assert!(m.get("test", "new-native-hls").await.is_some());
        assert!(m.get_hls("test", "new-native-hls").await.is_some());
    }

    #[tokio::test]
    async fn unrelated_hls_start_does_not_wait_for_pending_provider() {
        let (m, started, release) = blocking_manager();
        let blocked = {
            let m = m.clone();
            tokio::spawn(async move { m.get_or_start_hls("blocking", "blocked").await })
        };
        started.notified().await;

        tokio::time::timeout(
            Duration::from_millis(50),
            m.get_or_start_hls("blocking", "fast"),
        )
        .await
        .expect("an unrelated HLS start must not wait for pending provider work")
        .unwrap();

        release.notify_one();
        blocked.await.unwrap().unwrap();
        assert!(m.get_hls("blocking", "blocked").await.is_some());
        assert!(m.get_hls("blocking", "fast").await.is_some());
    }

    #[tokio::test]
    async fn reap_returns_while_native_hls_is_pending_then_observes_activity() {
        let (m, started, release) = blocking_manager();
        let start = {
            let m = m.clone();
            tokio::spawn(async move { m.get_or_start_hls("blocking", "native").await })
        };
        started.notified().await;

        tokio::time::timeout(Duration::from_millis(50), m.reap_idle_at(Instant::now()))
            .await
            .expect("reap must not wait for pending native provider work");

        release.notify_one();
        start.await.unwrap().unwrap();

        assert!(m.get("blocking", "native").await.is_some());
        assert!(m.get_hls("blocking", "native").await.is_some());
    }

    #[tokio::test]
    async fn compatibility_hls_is_retained_only_by_returned_pin() {
        let m = StreamManager::new(registry());
        let (_pkg, _session, pin) = m
            .get_or_start_compatibility_hls("test", "compatibility-only-hls")
            .await
            .unwrap();

        m.reap_idle_at(Instant::now()).await;
        assert!(m.get("test", "compatibility-only-hls").await.is_some());
        assert!(m.get_hls("test", "compatibility-only-hls").await.is_some());

        drop(pin);
        m.reap_idle_at(Instant::now()).await;

        assert!(m.get("test", "compatibility-only-hls").await.is_none());
        assert!(m.get_hls("test", "compatibility-only-hls").await.is_none());
    }

    #[tokio::test]
    async fn reap_returns_while_compatibility_hls_is_pending_then_observes_pin() {
        let (m, started, release) = blocking_manager();
        let start = {
            let m = m.clone();
            tokio::spawn(async move {
                m.get_or_start_compatibility_hls("blocking", "compatibility")
                    .await
            })
        };
        started.notified().await;

        tokio::time::timeout(Duration::from_millis(50), m.reap_idle_at(Instant::now()))
            .await
            .expect("reap must not wait for pending compatibility provider work");

        release.notify_one();
        let (_pkg, _session, pin) = start.await.unwrap().unwrap();

        assert!(m.get("blocking", "compatibility").await.is_some());
        assert!(m.get_hls("blocking", "compatibility").await.is_some());

        drop(pin);
        m.reap_idle_at(Instant::now()).await;
        assert!(m.get("blocking", "compatibility").await.is_none());
        assert!(m.get_hls("blocking", "compatibility").await.is_none());
    }

    #[tokio::test]
    async fn inactive_hls_session_and_packager_are_reaped_together() {
        let m = StreamManager::new(registry());
        m.get_or_start_hls("test", "stale-hls").await.unwrap();

        m.reap_idle_at(Instant::now() + m.grace + Duration::from_secs(1))
            .await;

        assert!(m.get("test", "stale-hls").await.is_none());
        assert!(m.get_hls("test", "stale-hls").await.is_none());
    }
}
