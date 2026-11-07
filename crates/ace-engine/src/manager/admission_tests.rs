use super::*;
use crate::provider::{SourceStats, StreamProvider};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Default)]
struct Probe {
    opens: AtomicUsize,
    vod_opens: AtomicUsize,
    entered: Notify,
}

struct Source;
#[async_trait]
impl TsSource for Source {
    async fn next(&mut self) -> Option<bytes::Bytes> {
        std::future::pending().await
    }
    fn stats(&self) -> SourceStats {
        SourceStats::default()
    }
}
struct Content;
struct Range;
#[async_trait]
impl VodContent for Content {
    fn content_length(&self) -> u64 {
        4
    }
    async fn open_range(&self, _: u64, _: u64) -> Result<Box<dyn VodByteSource>, ProviderError> {
        Ok(Box::new(Range))
    }
}
#[async_trait]
impl VodByteSource for Range {
    fn content_length(&self) -> u64 {
        4
    }
    async fn next(&mut self) -> Option<bytes::Bytes> {
        std::future::pending().await
    }
}
#[async_trait]
impl StreamProvider for Probe {
    fn network(&self) -> &'static str {
        "synthetic"
    }
    async fn open(&self, id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        if id == "pending" {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        if id == "failed" {
            return Err(ProviderError::NotFound);
        }
        Ok(Box::new(Source))
    }
    async fn resolve_vod(&self, id: &str) -> Result<Box<dyn VodContent>, ProviderError> {
        self.vod_opens.fetch_add(1, Ordering::SeqCst);
        if id == "pending" {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(Box::new(Content))
    }
}
fn manager(max: usize) -> (Arc<StreamManager>, Arc<Probe>) {
    let probe = Arc::new(Probe::default());
    let mut registry = ProviderRegistry::new();
    registry.register(probe.clone());
    (StreamManager::with_session_limit(registry, max), probe)
}

#[tokio::test]
async fn pending_open_occupies_capacity_and_cancellation_releases_it() {
    let (manager, probe) = manager(1);
    let pending = tokio::spawn({
        let manager = manager.clone();
        async move { manager.get_or_start("synthetic", "pending").await }
    });
    probe.entered.notified().await;
    assert_eq!(manager.admission_status(), (1, Some(1)));
    assert!(matches!(
        manager.get_or_start("synthetic", "other").await,
        Err(ProviderError::CapacityExhausted)
    ));
    assert_eq!(
        probe.opens.load(Ordering::SeqCst),
        1,
        "reject before provider work"
    );
    pending.abort();
    assert!(matches!(pending.await, Err(error) if error.is_cancelled()));
    assert_eq!(manager.admission_status(), (0, Some(1)));
    assert!(manager.get_or_start("synthetic", "new").await.is_ok());
    manager.shutdown().await;
}

#[tokio::test]
async fn existing_direct_and_hls_viewers_share_one_admission() {
    let (manager, probe) = manager(1);
    let first = manager.get_or_start("synthetic", "shared").await.unwrap();
    let again = manager.get_or_start("synthetic", "shared").await.unwrap();
    assert!(Arc::ptr_eq(&first, &again));
    let (_hls, same, pin) = manager
        .get_or_start_compatibility_hls("synthetic", "shared")
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&first, &same));
    assert_eq!(manager.admission_status(), (1, Some(1)));
    assert_eq!(probe.opens.load(Ordering::SeqCst), 1);
    assert!(matches!(
        manager.resolve_vod("synthetic", "other").await,
        Err(ProviderError::CapacityExhausted)
    ));
    assert_eq!(probe.vod_opens.load(Ordering::SeqCst), 0);
    drop(pin);
    manager.shutdown().await;
}

#[tokio::test]
async fn vod_range_keeps_admission_after_cache_eviction_and_content_drop() {
    let (manager, probe) = manager(1);
    let content = manager.resolve_vod("synthetic", "movie").await.unwrap();
    let repeated = manager.resolve_vod("synthetic", "movie").await.unwrap();
    assert!(Arc::ptr_eq(&content, &repeated));
    drop(repeated);
    let range = content.open_range(0, 3).await.unwrap();
    manager.reap_idle_at(Instant::now() + manager.grace).await;
    drop(content);
    assert_eq!(manager.admission_status(), (1, Some(1)));
    assert!(matches!(
        manager.resolve_vod("synthetic", "second").await,
        Err(ProviderError::CapacityExhausted)
    ));
    assert_eq!(probe.vod_opens.load(Ordering::SeqCst), 1);
    drop(range);
    assert_eq!(manager.admission_status(), (0, Some(1)));
    assert!(manager.resolve_vod("synthetic", "second").await.is_ok());
    manager.shutdown().await;
}

#[tokio::test]
async fn failed_open_releases_and_zero_limit_keeps_existing_unlimited_behavior() {
    let (limited, _) = manager(1);
    assert!(matches!(
        limited.get_or_start("synthetic", "failed").await,
        Err(ProviderError::NotFound)
    ));
    assert_eq!(limited.admission_status(), (0, Some(1)));
    limited.shutdown().await;
    let (unlimited, probe) = manager(0);
    for id in ["one", "two", "three"] {
        unlimited.get_or_start("synthetic", id).await.unwrap();
    }
    assert_eq!(unlimited.admission_status(), (3, None));
    assert_eq!(probe.opens.load(Ordering::SeqCst), 3);
    unlimited.shutdown().await;
}

#[tokio::test]
async fn shutdown_cancels_pending_vod_without_releasing_a_live_range_early() {
    let (manager, probe) = manager(2);
    let content = manager.resolve_vod("synthetic", "movie").await.unwrap();
    let mut range = manager
        .open_vod_range(content.as_ref(), 0, 3)
        .await
        .unwrap();
    drop(content);
    let pending = tokio::spawn({
        let manager = manager.clone();
        async move { manager.resolve_vod("synthetic", "pending").await }
    });
    probe.entered.notified().await;
    assert_eq!(manager.admission_status(), (2, Some(2)));
    manager.shutdown().await;
    assert!(matches!(
        pending.await.unwrap(),
        Err(ProviderError::NotFound)
    ));
    assert_eq!(manager.admission_status(), (1, Some(2)));
    assert!(range.next().await.is_none());
    assert_eq!(manager.admission_status(), (0, Some(2)));
    assert!(matches!(
        manager.get_or_start("synthetic", "new").await,
        Err(ProviderError::NotFound)
    ));
}
