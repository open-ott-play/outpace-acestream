use super::*;
use std::sync::atomic::AtomicUsize;

const SYNTHETIC_ID: &str = "0123456789abcdef0123456789abcdef01234567";

struct OpenGuard(Arc<AtomicUsize>);

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct StartupProvider {
    opens: Arc<AtomicUsize>,
    completed: Arc<AtomicUsize>,
    release_after: Option<Duration>,
    resume_waiter: bool,
}

#[async_trait]
impl StreamProvider for StartupProvider {
    fn network(&self) -> &'static str {
        "fix"
    }

    async fn open(&self, _id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        let previous = self.opens.fetch_add(1, Ordering::SeqCst);
        let _guard = OpenGuard(self.completed.clone());
        if previous == 0 || !self.resume_waiter {
            match self.release_after {
                Some(delay) => tokio::time::sleep(delay).await,
                None => std::future::pending().await,
            }
        }
        Ok(Box::new(StalledSource))
    }
}

fn startup_state(
    release_after: Option<Duration>,
    resume_waiter: bool,
) -> (AppState, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let opens = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(StartupProvider {
        opens: opens.clone(),
        completed: completed.clone(),
        release_after,
        resume_waiter,
    }));
    let mut state = state();
    state.manager = StreamManager::with_config(
        registry,
        32,
        crate::config::HlsConfig {
            segment_duration_ms: 1000,
            startup_timeout_ms: 1000,
            startup_segments: 1,
            ..crate::config::HlsConfig::default()
        },
    );
    state.networks = vec!["fix".into()];
    state.experimental_ace_compat = true;
    (state, opens, completed)
}

fn native_path() -> String {
    format!("/streams/fix/{SYNTHETIC_ID}.m3u8")
}
fn compatibility_path() -> String {
    format!("/ace/manifest.m3u8?infohash={SYNTHETIC_ID}&format=json")
}
fn assert_startup_timeout(response: Response) {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
}

async fn assert_pending_open_times_out(path: String) {
    let (state, opens, completed) = startup_state(None, false);
    let mut request =
        Box::pin(router(state.clone()).oneshot(Request::get(path).body(Body::empty()).unwrap()));
    assert!(futures::poll!(request.as_mut()).is_pending());
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    let response = tokio::time::timeout(Duration::from_millis(1), request)
        .await
        .expect("startup budget must include provider.open")
        .unwrap();
    assert_startup_timeout(response);
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert!(state.manager.list().await.is_empty());
    assert_eq!(state.ace_sessions.active_count(), 0);
    state.manager.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn native_hls_startup_budget_cancels_pending_provider_open() {
    assert_pending_open_times_out(native_path()).await;
}

#[tokio::test(start_paused = true)]
async fn compatibility_hls_startup_budget_cancels_pending_provider_open() {
    assert_pending_open_times_out(compatibility_path()).await;
}

#[tokio::test(start_paused = true)]
async fn native_hls_startup_budget_is_shared_with_media_readiness() {
    let (state, opens, completed) = startup_state(Some(Duration::from_millis(600)), false);
    let mut request = Box::pin(
        router(state.clone()).oneshot(Request::get(native_path()).body(Body::empty()).unwrap()),
    );
    assert!(futures::poll!(request.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(600)).await;
    assert!(futures::poll!(request.as_mut()).is_pending());
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert!(state.manager.get_hls("fix", SYNTHETIC_ID).await.is_some());
    tokio::time::advance(Duration::from_millis(400)).await;
    let response = tokio::time::timeout(Duration::from_millis(1), request)
        .await
        .expect("media readiness must not get a fresh full startup budget")
        .unwrap();
    assert_startup_timeout(response);
    state.manager.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn hls_startup_budget_of_waiter_does_not_cancel_other_opener() {
    let (state, opens, completed) = startup_state(None, false);
    let mut owner = Box::pin(state.manager.get_or_start("fix", SYNTHETIC_ID));
    assert!(futures::poll!(owner.as_mut()).is_pending());
    let mut request = Box::pin(
        router(state.clone()).oneshot(
            Request::get(compatibility_path())
                .body(Body::empty())
                .unwrap(),
        ),
    );
    assert!(futures::poll!(request.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    let response = tokio::time::timeout(Duration::from_millis(1), request)
        .await
        .expect("waiting for another opener must consume the startup budget")
        .unwrap();
    assert_startup_timeout(response);
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    assert!(futures::poll!(owner.as_mut()).is_pending());
    drop(owner);
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert_eq!(state.ace_sessions.active_count(), 0);
    state.manager.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn hls_startup_budget_releases_opener_for_surviving_waiter() {
    let (state, opens, completed) = startup_state(None, true);
    let app = router(state.clone());
    let mut first = Box::pin(
        app.clone()
            .oneshot(Request::get(native_path()).body(Body::empty()).unwrap()),
    );
    assert!(futures::poll!(first.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(500)).await;
    let mut second = Box::pin(
        app.oneshot(
            Request::get(compatibility_path())
                .body(Body::empty())
                .unwrap(),
        ),
    );
    assert!(futures::poll!(second.as_mut()).is_pending());
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(500)).await;
    assert_startup_timeout(
        tokio::time::timeout(Duration::from_millis(1), first)
            .await
            .unwrap()
            .unwrap(),
    );
    let response = tokio::time::timeout(Duration::from_millis(1), second)
        .await
        .expect("the surviving waiter must acquire the cancelled opener's lock")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(opens.load(Ordering::SeqCst), 2);
    assert_eq!(completed.load(Ordering::SeqCst), 2);
    assert_eq!(state.ace_sessions.active_count(), 1);
    state.manager.shutdown().await;
}
