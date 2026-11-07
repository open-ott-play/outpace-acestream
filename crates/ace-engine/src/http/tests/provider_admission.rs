//! Provider-owner admission through real router paths; no external sources or sleeps.
use super::*;
use crate::provider::VodByteSource;
use std::sync::atomic::AtomicUsize;
use tokio::sync::Notify;

const SYNTHETIC_ID: &str = "0123456789abcdef0123456789abcdef01234567";

#[derive(Default)]
struct ProviderProbe {
    opens: AtomicUsize,
    vod_opens: AtomicUsize,
    entered: Notify,
}

#[async_trait]
impl StreamProvider for ProviderProbe {
    fn network(&self) -> &'static str {
        "synthetic"
    }
    async fn open(&self, id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        if id == "pending" {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(Box::new(StalledSource))
    }
    async fn resolve_vod(&self, _: &str) -> Result<Box<dyn VodContent>, ProviderError> {
        self.vod_opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Content))
    }
}
struct Content;
struct ContentRange(bool);
#[async_trait]
impl VodContent for Content {
    fn content_length(&self) -> u64 {
        1
    }
    async fn open_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Box<dyn VodByteSource>, ProviderError> {
        assert_eq!((start, end), (0, 0));
        Ok(Box::new(ContentRange(false)))
    }
}
#[async_trait]
impl VodByteSource for ContentRange {
    fn content_length(&self) -> u64 {
        1
    }
    async fn next(&mut self) -> Option<Bytes> {
        (!std::mem::replace(&mut self.0, true)).then_some(Bytes::from_static(b"x"))
    }
}

fn capped_state(max: usize) -> (AppState, Arc<ProviderProbe>) {
    let provider = Arc::new(ProviderProbe::default());
    let mut registry = ProviderRegistry::new();
    registry.register(provider.clone());
    (
        AppState {
            manager: StreamManager::with_session_limit(registry, max),
            networks: vec!["synthetic".into()],
            resolve_content_ids_in_getstream: false,
            experimental_ace_compat: true,
            ace_sessions: Arc::new(AceSessionStore::default()),
            origin_hls: None,
            broadcasts: None,
        },
        provider,
    )
}
async fn request(app: &Router, path: &str) -> Response {
    app.clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}
async fn json_response(response: Response) -> serde_json::Value {
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 16 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}
async fn assert_capacity(response: Response) {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 128)
            .await
            .unwrap()
            .as_ref(),
        b"session capacity reached"
    );
}

#[tokio::test]
async fn pending_provider_cap_has_consistent_http_errors_without_extra_opens() {
    let (state, probe) = capped_state(1);
    let manager = state.manager.clone();
    let app = router(state);
    let pending = tokio::spawn({
        let manager = manager.clone();
        async move { manager.get_or_start("synthetic", "pending").await }
    });
    probe.entered.notified().await;
    assert_eq!(manager.admission_status(), (1, Some(1)));
    for path in [
        "/streams/synthetic/other.ts".to_owned(),
        "/streams/synthetic/other.m3u8".to_owned(),
        "/vod/synthetic/other".to_owned(),
        "/vod/synthetic/other/manifest.m3u8".to_owned(),
        format!("/ace/getstream?infohash={SYNTHETIC_ID}"),
        format!("/ace/manifest.m3u8?infohash={SYNTHETIC_ID}"),
    ] {
        assert_capacity(request(&app, &path).await).await;
    }
    let json = json_response(
        request(
            &app,
            &format!("/ace/manifest.m3u8?infohash={SYNTHETIC_ID}&format=json"),
        )
        .await,
    )
    .await;
    assert_eq!(
        json,
        json!({"response": null, "error": "session capacity reached"})
    );

    // Direct JSON leases remain lazy: admission happens when their playback URL is fetched.
    let lazy = json_response(
        request(
            &app,
            &format!("/ace/getstream?infohash={SYNTHETIC_ID}&format=json"),
        )
        .await,
    )
    .await;
    assert!(lazy["error"].is_null());
    let playback = lazy["response"]["playback_url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://127.0.0.1:6878")
        .unwrap();
    assert_capacity(request(&app, playback).await).await;
    assert_eq!(probe.opens.load(Ordering::SeqCst), 1);
    assert_eq!(probe.vod_opens.load(Ordering::SeqCst), 0);

    let listed = json_response(request(&app, "/streams").await).await;
    assert_eq!(listed["streams"], json!([]));
    assert_eq!(listed["occupied_sessions"], 1);
    assert_eq!(listed["max_sessions"], 1);
    let status = json_response(request(&app, "/server/api?method=get_status").await).await;
    assert!(status["error"].is_null());
    assert_eq!(status["result"]["status"], "idle");
    assert_eq!(status["result"]["active_sessions"], 0);
    assert_eq!(status["result"]["occupied_sessions"], 1);
    assert_eq!(status["result"]["max_sessions"], 1);
    pending.abort();
    assert!(matches!(pending.await, Err(error) if error.is_cancelled()));
    assert_eq!(manager.admission_status(), (0, Some(1)));
    manager.shutdown().await;
}

#[tokio::test]
async fn native_and_compatibility_viewers_reuse_full_provider_capacity() {
    let (state, probe) = capped_state(1);
    let manager = state.manager.clone();
    let app = router(state);
    let path = format!("/streams/synthetic/{SYNTHETIC_ID}.ts");
    let first = request(&app, &path).await;
    assert_eq!(first.status(), StatusCode::OK);
    let second = request(&app, &path).await;
    assert_eq!(second.status(), StatusCode::OK);
    let direct = request(&app, &format!("/ace/getstream?infohash={SYNTHETIC_ID}")).await;
    assert_eq!(direct.status(), StatusCode::OK);
    let hls = json_response(
        request(
            &app,
            &format!("/ace/manifest.m3u8?infohash={SYNTHETIC_ID}&format=json"),
        )
        .await,
    )
    .await;
    assert!(hls["error"].is_null());
    assert!(hls["response"]["playback_url"]
        .as_str()
        .unwrap()
        .contains("/ace/m/"));
    assert_eq!(manager.admission_status(), (1, Some(1)));
    assert_eq!(probe.opens.load(Ordering::SeqCst), 1);
    let status = json_response(request(&app, "/server/api?method=get_status").await).await;
    assert_eq!(status["result"]["status"], "dl");
    assert_eq!(status["result"]["active_sessions"], 1);
    assert_eq!(status["result"]["occupied_sessions"], 1);
    assert_capacity(request(&app, "/streams/synthetic/other.ts").await).await;
    drop((first, second, direct));
    manager.shutdown().await;
}

#[tokio::test]
async fn cached_vod_reuses_a_slot_and_zero_limit_keeps_null_status_maximum() {
    let (state, probe) = capped_state(1);
    let manager = state.manager.clone();
    let app = router(state);
    for _ in 0..2 {
        let response = request(&app, "/vod/synthetic/movie").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "1");
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 4)
                .await
                .unwrap()
                .as_ref(),
            b"x"
        );
    }
    assert_eq!(probe.vod_opens.load(Ordering::SeqCst), 1);
    assert_eq!(manager.admission_status(), (1, Some(1)));
    assert_capacity(request(&app, "/vod/synthetic/other").await).await;
    manager.shutdown().await;

    let (state, probe) = capped_state(0);
    let manager = state.manager.clone();
    let app = router(state);
    for path in ["/streams/synthetic/one.ts", "/streams/synthetic/two.ts"] {
        assert_eq!(request(&app, path).await.status(), StatusCode::OK);
    }
    assert_eq!(probe.opens.load(Ordering::SeqCst), 2);
    let listed = json_response(request(&app, "/streams").await).await;
    assert_eq!(listed["streams"].as_array().unwrap().len(), 2);
    assert_eq!(listed["occupied_sessions"], 2);
    assert!(listed["max_sessions"].is_null());
    let status = json_response(request(&app, "/server/api?method=get_status").await).await;
    assert_eq!(status["result"]["active_sessions"], 2);
    assert_eq!(status["result"]["occupied_sessions"], 2);
    assert!(status["result"]["max_sessions"].is_null());
    manager.shutdown().await;
}
