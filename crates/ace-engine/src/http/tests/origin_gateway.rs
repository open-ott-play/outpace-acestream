use super::*;
use ace_swarm::http_fetch::FetchPolicy;
use std::sync::Mutex;

struct Origin {
    url: String,
    calls: Arc<Mutex<Vec<String>>>,
    task: tokio::task::AbortHandle,
}

impl Origin {
    async fn start() -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let captured = calls.clone();
        let origin = Router::new().fallback(axum::routing::any(move |request: Request<Body>| {
            let calls = captured.clone();
            async move {
                calls.lock().unwrap().push(request.uri().to_string());
                match request.uri().path() {
                    "/entry" => (StatusCode::FOUND, [(header::LOCATION, "/dir/master.m3u8")]).into_response(),
                    "/dir/master.m3u8" => (
                        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
                        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000\nmedia.m3u8?auth=x%2Fy+%2b\n",
                    ).into_response(),
                    "/dir/media.m3u8" => (
                        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
                        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\nsegment.ts\n#EXT-X-ENDLIST\n",
                    ).into_response(),
                    "/dir/segment.ts" => ([(header::CONTENT_TYPE, "video/mp2t")], FIXTURE).into_response(),
                    "/ranges.m3u8" => (
                        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
                        "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:2\n#EXT-X-MAP:URI=\"packed.mp4\",BYTERANGE=\"4@0\"\n#EXTINF:2,\n#EXT-X-BYTERANGE:4@4\npacked.mp4\n#EXT-X-ENDLIST\n",
                    ).into_response(),
                    // Protocol fixture for MAP/segment ranges, not a decoder qualification.
                    "/packed.mp4" => match request.headers().get(header::RANGE).and_then(|v| v.to_str().ok()) {
                        Some("bytes=0-3") => (StatusCode::PARTIAL_CONTENT, [(header::CONTENT_TYPE, "video/mp4"), (header::CONTENT_RANGE, "bytes 0-3/8")], "INIT").into_response(),
                        Some("bytes=4-7") => (StatusCode::PARTIAL_CONTENT, [(header::CONTENT_TYPE, "video/mp4"), (header::CONTENT_RANGE, "bytes 4-7/8")], "DATA").into_response(),
                        _ => StatusCode::RANGE_NOT_SATISFIABLE.into_response(),
                    },
                    "/unsupported" => ([(header::CONTENT_TYPE, "text/plain")], "not HLS").into_response(),
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, origin).await.unwrap();
        });
        Self {
            url,
            calls,
            task: task.abort_handle(),
        }
    }
}
impl Drop for Origin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn gateway_state(allow_non_global: bool) -> AppState {
    let mut state = ace_compat_state(0);
    state.origin_hls = Some(crate::origin_hls::OriginHls::new(FetchPolicy {
        allow_non_global,
    }));
    state
}

fn encode(value: &str) -> String {
    value.bytes().map(|b| format!("%{b:02X}")).collect()
}

async fn request(app: &Router, path: &str) -> Response {
    app.clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn text(response: Response) -> String {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    String::from_utf8(
        axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

async fn start(app: &Router, url: &str) -> String {
    let response = request(
        app,
        &format!("/hls/manifest.m3u8?manifest_url={}", encode(url)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let location = response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(location.starts_with("/origin-hls/"));
    location
}

fn media_uri(playlist: &str) -> &str {
    playlist
        .lines()
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap()
}

#[tokio::test]
async fn gateway_requires_both_gates_and_rejects_bad_queries_before_admission() {
    let source = "https://origin.example.test/synthetic.m3u8";
    let path = format!("/hls/manifest.m3u8?manifest_url={}", encode(source));
    let mut disabled = gateway_state(false);
    disabled.experimental_ace_compat = false;
    let relay = disabled.origin_hls.clone().unwrap();
    assert_eq!(
        request(&router(disabled), &path).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(relay.active_count(), 0);
    let off = request(&router(ace_compat_state(0)), &path).await;
    assert_eq!(off.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(off.headers()[header::CACHE_CONTROL], "no-store");

    let origin = Origin::start().await;
    let state = gateway_state(true);
    let relay = state.origin_hls.clone().unwrap();
    let app = router(state.clone());
    let value = encode(&format!("{}/entry", origin.url));
    for query in [
        String::new(),
        "manifest_url=".into(),
        "url=x".into(),
        format!("manifest_url={value}&manifest_url={value}"),
        format!("manifest_url={value}&infohash=unused"),
        format!("manifest_url={value}&format=json"),
        "manifest_url=file%3A%2F%2F%2Fsynthetic".into(),
        "manifest_url=https%3A%2F%2Fuser%3Apass%40example.test%2Fa".into(),
        "manifest_url=%FF".into(),
        format!(
            "manifest_url={}",
            encode(&format!("https://example.test/{}", "x".repeat(8192)))
        ),
    ] {
        let response = request(&app, &format!("/hls/manifest.m3u8?{query}")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(relay.active_count(), 0);
    }
    assert!(origin.calls.lock().unwrap().is_empty());
    assert!(state.manager.list().await.is_empty());
    assert_eq!(state.ace_sessions.active_count(), 0);
    let denied = gateway_state(false);
    let relay = denied.origin_hls.clone().unwrap();
    let response = request(
        &router(denied),
        &format!("/hls/manifest.m3u8?manifest_url={value}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(relay.active_count(), 0);
}

#[tokio::test]
async fn gateway_follows_signed_origin_redirect_and_serves_rewritten_ts_without_swarm() {
    let origin = Origin::start().await;
    let state = gateway_state(true);
    let relay = state.origin_hls.clone().unwrap();
    let app = router(state.clone());
    let root = start(&app, &format!("{}/entry?sig=a%2Fb+%2b&x=1&x=2", origin.url)).await;
    assert!(origin.calls.lock().unwrap().is_empty(), "admission is lazy");
    let master = text(request(&app, &root).await).await;
    let playlist = text(request(&app, media_uri(&master)).await).await;
    let segment = request(&app, media_uri(&playlist)).await;
    assert_eq!(segment.status(), StatusCode::OK);
    assert_eq!(segment.headers()[header::CONTENT_TYPE], "video/mp2t");
    assert_eq!(
        axum::body::to_bytes(segment.into_body(), 1 << 20)
            .await
            .unwrap()
            .as_ref(),
        FIXTURE
    );
    assert_eq!(
        *origin.calls.lock().unwrap(),
        vec![
            "/entry?sig=a%2Fb+%2b&x=1&x=2",
            "/dir/master.m3u8",
            "/dir/media.m3u8?auth=x%2Fy+%2b",
            "/dir/segment.ts"
        ]
    );
    assert!(state.manager.list().await.is_empty());
    assert_eq!(state.ace_sessions.active_count(), 0);
    assert_eq!(relay.active_count(), 1);
    let stop = root.rsplit_once('/').unwrap().0;
    let response = app
        .clone()
        .oneshot(Request::delete(stop).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(relay.active_count(), 0);
    assert_eq!(request(&app, &root).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn gateway_preserves_map_and_segment_ranges_and_rejects_non_hls_body() {
    let origin = Origin::start().await;
    let app = router(gateway_state(true));
    let root = start(&app, &format!("{}/ranges.m3u8", origin.url)).await;
    let playlist = text(request(&app, &root).await).await;
    let map_uri = playlist
        .split("URI=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert_eq!(map_uri, media_uri(&playlist));
    assert!(playlist.contains("BYTERANGE=\"4@0\""));
    assert!(playlist.contains("#EXT-X-BYTERANGE:4@4"));
    for (range, content_range, expected) in [
        ("bytes=0-3", "bytes 0-3/8", "INIT"),
        ("bytes=4-7", "bytes 4-7/8", "DATA"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get(map_uri)
                    .header(header::RANGE, range)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], content_range);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
            expected
        );
    }
    let invalid = start(&app, &format!("{}/unsupported", origin.url)).await;
    let response = request(&app, &invalid).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
}
