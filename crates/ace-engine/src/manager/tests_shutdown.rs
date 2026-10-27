use super::*;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;

struct PendingProvider(mpsc::UnboundedSender<&'static str>);

#[async_trait]
impl crate::provider::StreamProvider for PendingProvider {
    fn network(&self) -> &'static str {
        "pending"
    }
    async fn open(&self, _: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        self.0.send("open").unwrap();
        std::future::pending().await
    }
    async fn resolve_vod(&self, _: &str) -> Result<Box<dyn VodContent>, ProviderError> {
        self.0.send("resolve").unwrap();
        std::future::pending().await
    }
}

fn pending_manager() -> (Arc<StreamManager>, mpsc::UnboundedReceiver<&'static str>) {
    let (entered, events) = mpsc::unbounded_channel();
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(PendingProvider(entered)));
    (StreamManager::new(registry), events)
}

struct RetainedSessionProvider {
    reads: Arc<Notify>,
    drops: Arc<AtomicUsize>,
}

struct RetainedSessionSource {
    reads: Arc<Notify>,
    drops: Arc<AtomicUsize>,
}

#[async_trait]
impl crate::provider::StreamProvider for RetainedSessionProvider {
    fn network(&self) -> &'static str {
        "retained"
    }

    async fn open(&self, _: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        Ok(Box::new(RetainedSessionSource {
            reads: self.reads.clone(),
            drops: self.drops.clone(),
        }))
    }
}

#[async_trait]
impl TsSource for RetainedSessionSource {
    async fn next(&mut self) -> Option<bytes::Bytes> {
        self.reads.notify_one();
        std::future::pending().await
    }

    fn stats(&self) -> crate::provider::SourceStats {
        Default::default()
    }
}

impl Drop for RetainedSessionSource {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn teardown_ends_source_even_when_session_handle_survives() {
    for action in ["stop", "shutdown", "drop"] {
        let reads = Arc::new(Notify::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(RetainedSessionProvider {
            reads: reads.clone(),
            drops: drops.clone(),
        }));
        let manager = StreamManager::new(registry);
        let session = manager.get_or_start("retained", "pending").await.unwrap();
        // A retained handle and an internal receiver do not constitute a viewer.
        // Neither may prevent an explicit stop or manager teardown.
        let mut receiver = session.raw_receiver();
        timeout(Duration::from_secs(1), reads.notified())
            .await
            .expect("source should be waiting for media");
        match action {
            "stop" => assert!(manager.stop("retained", "pending").await),
            "shutdown" => manager.shutdown().await,
            "drop" => drop(manager),
            _ => unreachable!(),
        }
        assert_eq!(
            timeout(Duration::from_secs(1), receiver.recv()).await,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)),
            "{action} must close the source without waiting for the last session handle"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1, "{action}");
        assert!(session.is_finished(), "{action}");
    }
}

#[tokio::test(start_paused = true)]
async fn idle_reap_preserves_the_acquisition_to_subscription_handoff() {
    let reads = Arc::new(Notify::new());
    let drops = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RetainedSessionProvider {
        reads: reads.clone(),
        drops: drops.clone(),
    }));
    let manager = StreamManager::new(registry);
    let session = manager.get_or_start("retained", "handoff").await.unwrap();
    // The reaper can run on another executor thread between acquisition and subscribe.
    // An owned handle must still be usable even if the idle map entry was removed.
    manager.reap_idle_at(Instant::now()).await;
    let mut subscription = session.subscribe();
    timeout(Duration::from_secs(1), reads.notified())
        .await
        .expect("the acquired source must still start when its receiver attaches");
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(session);
    assert_eq!(
        timeout(Duration::from_secs(1), subscription.rx.recv()).await,
        Ok(Err(tokio::sync::broadcast::error::RecvError::Closed))
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_cancels_live_open_and_same_key_waiters() {
    for hls in [false, true] {
        let (manager, mut entered) = pending_manager();
        let acquire = || async {
            if hls {
                manager.get_or_start_hls("pending", "same").await.map(drop)
            } else {
                manager.get_or_start("pending", "same").await.map(drop)
            }
        };
        let owner = acquire();
        let waiter = acquire();
        tokio::pin!(owner, waiter);
        assert!(futures::poll!(owner.as_mut()).is_pending());
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        assert_eq!(entered.try_recv().unwrap(), "open");
        assert!(entered.try_recv().is_err(), "same key opens only once");
        manager.shutdown().await;
        assert!(timeout(Duration::from_secs(1), owner)
            .await
            .unwrap()
            .is_err());
        assert!(timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .is_err());
        assert!(manager.list().await.is_empty());
        assert!(manager.packagers.lock().await.is_empty());
        assert!(manager.hls_transitions.lock().unwrap().pending.is_empty());
        assert!(manager.hls_transitions.lock().unwrap().cancelled.is_empty());
        assert!(manager
            .session_start_locks
            .lock()
            .unwrap()
            .values()
            .all(|lock| lock.upgrade().is_none()));
        assert!(acquire().await.is_err());
        assert!(entered.try_recv().is_err(), "closed admissions do no I/O");
    }
}

#[tokio::test]
async fn shutdown_cancels_vod_resolution_and_same_key_waiters() {
    let (manager, mut entered) = pending_manager();
    let owner = manager.resolve_vod("pending", "same");
    let waiter = manager.resolve_vod("pending", "same");
    tokio::pin!(owner, waiter);
    assert!(futures::poll!(owner.as_mut()).is_pending());
    assert!(futures::poll!(waiter.as_mut()).is_pending());
    assert_eq!(entered.try_recv().unwrap(), "resolve");
    assert!(entered.try_recv().is_err());
    manager.shutdown().await;
    assert!(timeout(Duration::from_secs(1), owner)
        .await
        .unwrap()
        .is_err());
    assert!(timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .is_err());
    assert!(manager.vod_cache.lock().await.is_empty());
    assert!(manager
        .vod_resolve_locks
        .lock()
        .unwrap()
        .values()
        .all(|lock| lock.upgrade().is_none()));
    assert!(manager.resolve_vod("pending", "later").await.is_err());
    assert!(entered.try_recv().is_err());
}

struct CachedVodProvider(Arc<AtomicUsize>);
struct CachedVod(Arc<AtomicUsize>);
impl Drop for CachedVod {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl crate::provider::StreamProvider for CachedVodProvider {
    fn network(&self) -> &'static str {
        "cached"
    }
    async fn open(&self, _: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        Err(ProviderError::NotFound)
    }
    async fn resolve_vod(&self, _: &str) -> Result<Box<dyn VodContent>, ProviderError> {
        Ok(Box::new(CachedVod(self.0.clone())))
    }
}
#[async_trait]
impl VodContent for CachedVod {
    fn content_length(&self) -> u64 {
        1
    }
    async fn open_range(&self, _: u64, _: u64) -> Result<Box<dyn VodByteSource>, ProviderError> {
        Err(ProviderError::NotFound)
    }
}

#[tokio::test]
async fn concurrent_shutdown_releases_vod_cache_and_keeps_admissions_closed() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CachedVodProvider(drops.clone())));
    let manager = StreamManager::new(registry);
    let content = manager.resolve_vod("cached", "one").await.unwrap();
    let weak = Arc::downgrade(&content);
    drop(content);
    assert!(
        weak.upgrade().is_some(),
        "the cache owns the idle descriptor"
    );
    tokio::join!(manager.shutdown(), manager.shutdown());
    assert!(weak.upgrade().is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(manager.resolve_vod("cached", "one").await.is_err());
    assert!(manager.vod_cache.lock().await.is_empty());
    manager.shutdown().await;
}

#[tokio::test]
async fn shutdown_fences_a_resolved_vod_waiting_to_publish() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CachedVodProvider(drops.clone())));
    let manager = StreamManager::new(registry);
    let finalizing = manager.hls_finalization.lock().await;
    let shutdown = manager.shutdown();
    tokio::pin!(shutdown);
    assert!(futures::poll!(shutdown.as_mut()).is_pending());

    // Resolution completes while shutdown is queued first at the publication fence.
    let resolving = manager.resolve_vod("cached", "late");
    tokio::pin!(resolving);
    assert!(futures::poll!(resolving.as_mut()).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(finalizing);
    timeout(Duration::from_secs(1), shutdown).await.unwrap();
    assert!(timeout(Duration::from_secs(1), resolving)
        .await
        .unwrap()
        .is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(manager.vod_cache.lock().await.is_empty());
}

struct PendingRange {
    block_open: bool,
    entered: Arc<Notify>,
    reads: Arc<Notify>,
    drops: Arc<AtomicUsize>,
}
struct PendingReader {
    reads: Arc<Notify>,
    drops: Arc<AtomicUsize>,
}
#[async_trait]
impl VodContent for PendingRange {
    fn content_length(&self) -> u64 {
        16
    }
    async fn open_range(&self, _: u64, _: u64) -> Result<Box<dyn VodByteSource>, ProviderError> {
        self.entered.notify_one();
        if self.block_open {
            std::future::pending::<()>().await;
        }
        Ok(Box::new(PendingReader {
            reads: self.reads.clone(),
            drops: self.drops.clone(),
        }))
    }
}
#[async_trait]
impl VodByteSource for PendingReader {
    fn content_length(&self) -> u64 {
        16
    }
    async fn next(&mut self) -> Option<bytes::Bytes> {
        self.reads.notify_one();
        std::future::pending().await
    }
}
impl Drop for PendingReader {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn shutdown_cancels_pending_vod_range_open_and_body_read() {
    for block_open in [true, false] {
        let manager = StreamManager::new(ProviderRegistry::new());
        let content = PendingRange {
            block_open,
            entered: Arc::new(Notify::new()),
            reads: Arc::new(Notify::new()),
            drops: Arc::new(AtomicUsize::new(0)),
        };
        if block_open {
            let opening = manager.open_vod_range(&content, 0, 15);
            tokio::pin!(opening);
            assert!(futures::poll!(opening.as_mut()).is_pending());
            content.entered.notified().await;
            manager.shutdown().await;
            assert!(timeout(Duration::from_secs(1), opening)
                .await
                .unwrap()
                .is_err());
        } else {
            let mut source = manager.open_vod_range(&content, 0, 15).await.unwrap();
            assert_eq!(source.content_length(), 16);
            {
                let reading = source.next();
                tokio::pin!(reading);
                assert!(futures::poll!(reading.as_mut()).is_pending());
                content.reads.notified().await;
                manager.shutdown().await;
                assert!(timeout(Duration::from_secs(1), reading)
                    .await
                    .unwrap()
                    .is_none());
            }
            assert_eq!(content.drops.load(Ordering::SeqCst), 1);
            assert!(source.next().await.is_none());
        }
        assert!(manager.open_vod_range(&content, 0, 15).await.is_err());
    }
}

struct FiniteRange(Arc<AtomicUsize>);
struct FiniteReader {
    chunks: std::collections::VecDeque<bytes::Bytes>,
    drops: Arc<AtomicUsize>,
}

#[async_trait]
impl VodContent for FiniteRange {
    fn content_length(&self) -> u64 {
        6
    }
    async fn open_range(&self, _: u64, _: u64) -> Result<Box<dyn VodByteSource>, ProviderError> {
        Ok(Box::new(FiniteReader {
            chunks: [
                bytes::Bytes::from_static(b"abc"),
                bytes::Bytes::from_static(b"def"),
            ]
            .into_iter()
            .collect(),
            drops: self.0.clone(),
        }))
    }
}

#[async_trait]
impl VodByteSource for FiniteReader {
    fn content_length(&self) -> u64 {
        6
    }
    async fn next(&mut self) -> Option<bytes::Bytes> {
        self.chunks.pop_front()
    }
}

impl Drop for FiniteReader {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn vod_reader_owns_manager_until_eof_or_body_drop() {
    for consume in [true, false] {
        let manager = StreamManager::new(ProviderRegistry::new());
        let weak = Arc::downgrade(&manager);
        let drops = Arc::new(AtomicUsize::new(0));
        let content = FiniteRange(drops.clone());
        let mut source = manager.open_vod_range(&content, 0, 5).await.unwrap();
        drop(manager);
        assert!(weak.upgrade().is_some(), "the open body owns its manager");
        assert_eq!(source.content_length(), 6);
        if consume {
            assert_eq!(source.next().await.unwrap().as_ref(), b"abc");
            assert_eq!(source.next().await.unwrap().as_ref(), b"def");
            assert!(weak.upgrade().is_some());
            assert!(source.next().await.is_none());
            assert!(weak.upgrade().is_none(), "EOF releases the manager owner");
            assert!(source.next().await.is_none());
        }
        drop(source);
        assert!(
            weak.upgrade().is_none(),
            "body drop releases the manager owner"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn explicit_shutdown_cancels_vod_reader_and_releases_its_manager_owner() {
    let manager = StreamManager::new(ProviderRegistry::new());
    let weak = Arc::downgrade(&manager);
    let content = PendingRange {
        block_open: false,
        entered: Arc::new(Notify::new()),
        reads: Arc::new(Notify::new()),
        drops: Arc::new(AtomicUsize::new(0)),
    };
    let mut source = manager.open_vod_range(&content, 0, 15).await.unwrap();
    drop(manager);
    {
        let reading = source.next();
        tokio::pin!(reading);
        assert!(futures::poll!(reading.as_mut()).is_pending());
        content.reads.notified().await;
        let controller = weak.upgrade().expect("the open reader keeps its manager");
        timeout(Duration::from_secs(1), controller.shutdown())
            .await
            .expect("the body owner must not block explicit shutdown");
        drop(controller);
        assert!(timeout(Duration::from_secs(1), reading)
            .await
            .unwrap()
            .is_none());
    }
    assert!(weak.upgrade().is_none());
    assert_eq!(content.drops.load(Ordering::SeqCst), 1);
    assert!(source.next().await.is_none());
}
