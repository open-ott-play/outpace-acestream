use super::*;

struct FakeVod {
    chunks: std::collections::VecDeque<Bytes>,
    total: u64,
}

#[async_trait]
impl VodByteSource for FakeVod {
    fn content_length(&self) -> u64 {
        self.total
    }
    async fn next(&mut self) -> Option<Bytes> {
        self.chunks.pop_front()
    }
}

#[tokio::test]
async fn vod_response_streams_body_with_content_length() {
    let src = FakeVod {
        chunks: [Bytes::from_static(b"abc"), Bytes::from_static(b"de")]
            .into_iter()
            .collect(),
        total: 5,
    };
    let resp = vod_response_from_source(Box::new(src));
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_LENGTH], "5");
    assert_eq!(resp.headers()[header::ACCEPT_RANGES], "bytes");
    let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
    assert_eq!(&body[..], b"abcde");
}

#[test]
fn parse_byte_range_covers_the_range_header_forms() {
    use RangeOutcome::*;
    // No / non-bytes / unparseable / multi-range specs are ignored -> serve the whole file.
    assert!(matches!(parse_byte_range(None, 10), Full));
    assert!(matches!(parse_byte_range(Some("items=0-1"), 10), Full));
    assert!(matches!(parse_byte_range(Some("bytes=abc"), 10), Full));
    assert!(matches!(parse_byte_range(Some("bytes=-"), 10), Full));
    assert!(matches!(parse_byte_range(Some("bytes=0-1,3-4"), 10), Full));
    // Explicit range, clamped to the last byte.
    assert!(matches!(
        parse_byte_range(Some("bytes=2-4"), 10),
        Satisfiable(2, 4)
    ));
    assert!(matches!(
        parse_byte_range(Some("bytes=8-100"), 10),
        Satisfiable(8, 9)
    ));
    // Open-ended: from N to the end.
    assert!(matches!(
        parse_byte_range(Some("bytes=5-"), 10),
        Satisfiable(5, 9)
    ));
    // Suffix: the last N bytes (and a suffix longer than the file clamps to the whole file).
    assert!(matches!(
        parse_byte_range(Some("bytes=-3"), 10),
        Satisfiable(7, 9)
    ));
    assert!(matches!(
        parse_byte_range(Some("bytes=-100"), 10),
        Satisfiable(0, 9)
    ));
    // Out-of-bounds / inverted / zero-suffix are unsatisfiable -> 416.
    assert!(matches!(
        parse_byte_range(Some("bytes=100-200"), 10),
        Unsatisfiable
    ));
    assert!(matches!(
        parse_byte_range(Some("bytes=5-3"), 10),
        Unsatisfiable
    ));
    assert!(matches!(
        parse_byte_range(Some("bytes=-0"), 10),
        Unsatisfiable
    ));
}

// An in-memory VOD provider for exercising the HTTP `/vod` range contract without a swarm.
// (Real SHA-1 verification of served ranges is covered by the ace-swarm vod tests.)
struct MemVodProvider {
    data: Vec<u8>,
}
struct MemVod {
    data: Vec<u8>,
}
struct MemVodSource {
    chunk: Option<Bytes>,
    len: u64,
}
#[async_trait]
impl VodByteSource for MemVodSource {
    fn content_length(&self) -> u64 {
        self.len
    }
    async fn next(&mut self) -> Option<Bytes> {
        self.chunk.take()
    }
}
#[async_trait]
impl VodContent for MemVod {
    fn content_length(&self) -> u64 {
        self.data.len() as u64
    }
    async fn open_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Box<dyn VodByteSource>, ProviderError> {
        let slice = self.data[start as usize..=end as usize].to_vec();
        let len = slice.len() as u64;
        Ok(Box::new(MemVodSource {
            chunk: Some(Bytes::from(slice)),
            len,
        }))
    }
}
#[async_trait]
impl StreamProvider for MemVodProvider {
    fn network(&self) -> &'static str {
        "memvod"
    }
    async fn open(&self, _id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        Err(ProviderError::NotFound)
    }
    async fn resolve_vod(&self, _id: &str) -> Result<Box<dyn VodContent>, ProviderError> {
        Ok(Box::new(MemVod {
            data: self.data.clone(),
        }))
    }
}

fn memvod_router(data: Vec<u8>) -> Router {
    let mut r = ProviderRegistry::new();
    r.register(std::sync::Arc::new(MemVodProvider { data }));
    router(AppState {
        manager: StreamManager::new(r),
        networks: vec!["memvod".into()],
        resolve_content_ids_in_getstream: false,
        ace_sessions: Arc::new(AceSessionStore::default()),
        experimental_ace_compat: false,
        origin_hls: None,
        broadcasts: None,
    })
}

#[tokio::test]
async fn vod_without_range_serves_200_and_advertises_accept_ranges() {
    let data: Vec<u8> = (0..20u8).collect();
    let resp = memvod_router(data.clone())
        .oneshot(Request::get("/vod/memvod/x").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(resp.headers()[header::CONTENT_LENGTH], "20");
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&body[..], &data[..]);
}

#[tokio::test]
async fn vod_range_serves_206_partial_content() {
    let data: Vec<u8> = (0..20u8).collect();
    let resp = memvod_router(data.clone())
        .oneshot(
            Request::get("/vod/memvod/x")
                .header(header::RANGE, "bytes=5-9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes 5-9/20");
    assert_eq!(resp.headers()[header::CONTENT_LENGTH], "5");
    assert_eq!(resp.headers()[header::ACCEPT_RANGES], "bytes");
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&body[..], &data[5..=9]);
}

#[tokio::test]
async fn vod_suffix_range_serves_the_last_bytes() {
    let data: Vec<u8> = (0..20u8).collect();
    let resp = memvod_router(data.clone())
        .oneshot(
            Request::get("/vod/memvod/x")
                .header(header::RANGE, "bytes=-4")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes 16-19/20");
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&body[..], &data[16..=19]);
}

#[tokio::test]
async fn vod_unsatisfiable_range_returns_416_with_content_range() {
    let data: Vec<u8> = (0..20u8).collect();
    let resp = memvod_router(data)
        .oneshot(
            Request::get("/vod/memvod/x")
                .header(header::RANGE, "bytes=50-60")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes */20");
}

// A memvod router whose HLS segments are `seg_packets` TS packets (188 bytes each), so small
// fixtures still span several segments.
fn memvod_hls_router(data: Vec<u8>, seg_packets: usize) -> Router {
    let mut r = ProviderRegistry::new();
    r.register(std::sync::Arc::new(MemVodProvider { data }));
    let hls = crate::config::HlsConfig {
        segment_packets: seg_packets,
        window_segments: 6,
        segment_duration_ms: 2000,
        ..crate::config::HlsConfig::default()
    };
    router(AppState {
        manager: StreamManager::with_config(r, 256, hls),
        networks: vec!["memvod".into()],
        resolve_content_ids_in_getstream: false,
        ace_sessions: Arc::new(AceSessionStore::default()),
        experimental_ace_compat: false,
        origin_hls: None,
        broadcasts: None,
    })
}

#[tokio::test]
async fn vod_manifest_serves_a_terminated_vod_playlist() {
    // Eight packets over three-packet segments => three segments.
    let resp = memvod_hls_router(synthetic_vod_ts(8), 3)
        .oneshot(
            Request::get("/vod/memvod/x/manifest.m3u8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "application/vnd.apple.mpegurl"
    );
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
    assert!(text.contains("/vod/memvod/x/seg/0.ts"));
    assert!(text.contains("/vod/memvod/x/seg/2.ts"));
    assert!(!text.contains("/vod/memvod/x/seg/3.ts"));
    assert!(text.trim_end().ends_with("#EXT-X-ENDLIST"));
}

#[tokio::test]
async fn vod_hls_segment_serves_its_verified_byte_range_as_ts() {
    let data = synthetic_vod_ts(8);
    // Segment 1 covers bytes [564, 1128).
    let resp = memvod_hls_router(data.clone(), 3)
        .oneshot(
            Request::get("/vod/memvod/x/seg/1.ts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "video/mp2t");
    assert_eq!(resp.headers()[header::CONTENT_LENGTH], "564");
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&body[..], &data[564..1128]);
}

#[tokio::test]
async fn vod_hls_final_segment_is_clamped_to_the_last_byte() {
    let data = synthetic_vod_ts(8);
    // Eight packets => final segment 2 covers [1128, 1504): 376 bytes.
    let resp = memvod_hls_router(data.clone(), 3)
        .oneshot(
            Request::get("/vod/memvod/x/seg/2.ts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_LENGTH], "376");
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(&body[..], &data[1128..1504]);
}

#[tokio::test]
async fn vod_hls_segment_past_the_end_404s() {
    let resp = memvod_hls_router(synthetic_vod_ts(8), 3)
        .oneshot(
            Request::get("/vod/memvod/x/seg/99.ts")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

fn synthetic_vod_ts(packets: usize) -> Vec<u8> {
    (0..packets)
        .flat_map(|index| {
            let mut packet = [0x55; 188];
            packet[..4].copy_from_slice(&[0x47, 0x01, 0x00, 0x10 | (index as u8 & 15)]);
            packet
        })
        .collect()
}

#[tokio::test]
async fn vod_hls_rejects_other_containers_without_changing_raw_ranges() {
    for signature in [
        b"\x00\x00\x00\x18ftyp".as_slice(),
        b"\x1a\x45\xdf\xa3".as_slice(),
    ] {
        let mut data = vec![0u8; 188 * 8];
        data[..signature.len()].copy_from_slice(signature);
        let app = memvod_hls_router(data.clone(), 3);
        for suffix in ["manifest.m3u8", "seg/0.ts", "seg/1.ts"] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(format!("/vod/memvod/x/{suffix}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{suffix}"
            );
        }
        let response = app
            .oneshot(
                Request::get("/vod/memvod/x")
                    .header(header::RANGE, "bytes=0-15")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 32)
                .await
                .unwrap()
                .as_ref(),
            &data[..16]
        );
    }
}

#[tokio::test]
async fn vod_hls_rejects_partial_packets_and_invalid_transport_headers() {
    let mut incomplete = synthetic_vod_ts(8);
    incomplete.push(0);
    let mut bad_sync = synthetic_vod_ts(8);
    bad_sync[188] = 0;
    let mut scrambled = synthetic_vod_ts(8);
    scrambled[3] |= 0x80;
    let mut invalid_adaptation = synthetic_vod_ts(8);
    invalid_adaptation[3] = 0x30;
    invalid_adaptation[4] = 183;
    for data in [incomplete, bad_sync, scrambled, invalid_adaptation] {
        let response = memvod_hls_router(data, 3)
            .oneshot(
                Request::get("/vod/memvod/x/manifest.m3u8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
}

#[tokio::test]
async fn vod_failures_map_to_the_documented_selector_contract() {
    // A well-formed id the provider will not open is client-actionable (`422`, like the live
    // `ih:` refusal), a missing network or target is `404`, and only an upstream failure is
    // `502`. Bodies carry the provider's reason without its outer error-variant wrapper.
    let invalid = vod_failure(ProviderError::Invalid("invalid selector".into()));
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(invalid.into_body(), 256)
        .await
        .unwrap();
    assert_eq!(&body[..], b"invalid selector");

    let unprocessable = vod_failure(ProviderError::Unresolvable("bare infohash".into()));
    assert_eq!(unprocessable.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = axum::body::to_bytes(unprocessable.into_body(), 256)
        .await
        .unwrap();
    assert_eq!(&body[..], b"bare infohash");

    let missing = vod_failure(ProviderError::NotFound);
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(missing.into_body(), 256)
        .await
        .unwrap();
    assert_eq!(&body[..], b"vod target not found");

    let upstream = vod_failure(ProviderError::Backend("catalog timeout".into()));
    assert_eq!(upstream.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(upstream.into_body(), 256)
        .await
        .unwrap();
    assert_eq!(&body[..], b"catalog timeout");

    let direct_hls = vod_failure(ProviderError::DirectHls {
        manifest_url: "https://origin.invalid/manifest.m3u8".into(),
    });
    assert_eq!(direct_hls.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

fn vod_provider_router(provider: Arc<dyn StreamProvider>) -> Router {
    let network = provider.network();
    let mut registry = ProviderRegistry::new();
    registry.register(provider);
    router(AppState {
        manager: StreamManager::with_config(
            registry,
            256,
            crate::config::HlsConfig {
                startup_timeout_ms: 100,
                ..Default::default()
            },
        ),
        networks: vec![network.into()],
        resolve_content_ids_in_getstream: false,
        ace_sessions: Arc::new(AceSessionStore::default()),
        experimental_ace_compat: false,
        origin_hls: None,
        broadcasts: None,
    })
}

#[tokio::test]
async fn native_vod_routes_reject_malformed_and_unsupported_ace_selectors_offline() {
    let app = vod_provider_router(Arc::new(crate::ace_provider::AceProvider::new(
        Arc::new(ace_wire::identity::Identity::generate()),
        0,
    )));
    for suffix in ["", "/manifest.m3u8", "/seg/0.ts"] {
        for (id, expected) in [
            ("cid:xyz", StatusCode::BAD_REQUEST),
            ("ih:xyz", StatusCode::BAD_REQUEST),
            ("turl-", StatusCode::BAD_REQUEST),
            ("invalid-selector", StatusCode::BAD_REQUEST),
            (
                "0123456789abcdef0123456789abcdef01234567",
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                "ih:0123456789abcdef0123456789abcdef01234567",
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(format!("/vod/ace/{id}{suffix}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{id}{suffix}");
        }
    }
}

#[tokio::test]
async fn vod_range_failures_use_the_same_status_contract_on_every_route() {
    struct FailingVodProvider;
    struct FailingVod(String);
    #[async_trait]
    impl StreamProvider for FailingVodProvider {
        fn network(&self) -> &'static str {
            "failure"
        }
        async fn open(&self, _: &str) -> Result<Box<dyn TsSource>, ProviderError> {
            Err(ProviderError::NotFound)
        }
        async fn resolve_vod(&self, id: &str) -> Result<Box<dyn VodContent>, ProviderError> {
            Ok(Box::new(FailingVod(id.into())))
        }
    }
    #[async_trait]
    impl VodContent for FailingVod {
        fn content_length(&self) -> u64 {
            188 * 8
        }
        async fn open_range(
            &self,
            _: u64,
            _: u64,
        ) -> Result<Box<dyn VodByteSource>, ProviderError> {
            Err(match self.0.as_str() {
                "invalid" => ProviderError::Invalid("invalid range".into()),
                "refused" => ProviderError::Unresolvable("range refused".into()),
                "missing" => ProviderError::NotFound,
                _ => ProviderError::Backend("range unavailable".into()),
            })
        }
    }
    let app = vod_provider_router(Arc::new(FailingVodProvider));
    for suffix in ["", "/manifest.m3u8", "/seg/0.ts"] {
        for (id, expected) in [
            ("invalid", StatusCode::BAD_REQUEST),
            ("refused", StatusCode::UNPROCESSABLE_ENTITY),
            ("missing", StatusCode::NOT_FOUND),
            ("backend", StatusCode::BAD_GATEWAY),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(format!("/vod/failure/{id}{suffix}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{id}{suffix}");
        }
    }
}

struct VodTiming {
    delays: [Duration; 3],
    bypass: std::sync::atomic::AtomicBool,
    active: std::sync::atomic::AtomicUsize,
    resolutions: std::sync::atomic::AtomicUsize,
    sources_dropped: std::sync::atomic::AtomicUsize,
}

struct VodOperation(Arc<VodTiming>);
impl Drop for VodOperation {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}
impl VodTiming {
    fn new(delays: [u64; 3]) -> Arc<Self> {
        Arc::new(Self {
            delays: delays.map(Duration::from_millis),
            bypass: false.into(),
            active: 0.into(),
            resolutions: 0.into(),
            sources_dropped: 0.into(),
        })
    }
    async fn delay(self: &Arc<Self>, phase: usize) {
        self.active.fetch_add(1, Ordering::SeqCst);
        let _operation = VodOperation(self.clone());
        if !self.bypass.load(Ordering::SeqCst) && !self.delays[phase].is_zero() {
            tokio::time::sleep(self.delays[phase]).await;
        }
    }
}

struct TimedVod(Arc<VodTiming>);
struct TimedVodSource {
    timing: Arc<VodTiming>,
    chunk: Option<Bytes>,
    length: u64,
}
impl Drop for TimedVodSource {
    fn drop(&mut self) {
        self.timing.sources_dropped.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl StreamProvider for TimedVod {
    fn network(&self) -> &'static str {
        "timed-vod"
    }
    async fn open(&self, _: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        Err(ProviderError::NotFound)
    }
    async fn resolve_vod(&self, _: &str) -> Result<Box<dyn VodContent>, ProviderError> {
        self.0.resolutions.fetch_add(1, Ordering::SeqCst);
        self.0.delay(0).await;
        Ok(Box::new(Self(self.0.clone())))
    }
}
#[async_trait]
impl VodContent for TimedVod {
    fn content_length(&self) -> u64 {
        188 * 8
    }
    async fn open_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Box<dyn VodByteSource>, ProviderError> {
        self.0.delay(1).await;
        Ok(Box::new(TimedVodSource {
            timing: self.0.clone(),
            chunk: Some(Bytes::copy_from_slice(
                &synthetic_vod_ts(8)[start as usize..=end as usize],
            )),
            length: end - start + 1,
        }))
    }
}
#[async_trait]
impl VodByteSource for TimedVodSource {
    fn content_length(&self) -> u64 {
        self.length
    }
    async fn next(&mut self) -> Option<Bytes> {
        let chunk = self.chunk.take()?;
        self.timing.delay(2).await;
        Some(chunk)
    }
}

#[tokio::test(start_paused = true)]
async fn vod_startup_deadline_cancels_each_phase_and_allows_retry() {
    for suffix in ["", "/manifest.m3u8", "/seg/0.ts"] {
        for phase in 0..3 {
            // Raw VOD returns headers before media, so reading its body is not startup work.
            if suffix.is_empty() && phase == 2 {
                continue;
            }
            let mut delays = [0; 3];
            delays[phase] = 200;
            let timing = VodTiming::new(delays);
            let app = vod_provider_router(Arc::new(TimedVod(timing.clone())));
            let uri = format!("/vod/timed-vod/x{suffix}");
            let started = tokio::time::Instant::now();
            let response = app
                .clone()
                .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::GATEWAY_TIMEOUT,
                "{suffix}, phase {phase}"
            );
            assert_eq!(response.headers()[header::RETRY_AFTER], "1");
            assert_eq!(started.elapsed(), Duration::from_millis(100));
            assert_eq!(timing.active.load(Ordering::SeqCst), 0);
            if phase == 2 {
                assert_eq!(timing.sources_dropped.load(Ordering::SeqCst), 1);
            }

            timing.bypass.store(true, Ordering::SeqCst);
            let response = app
                .oneshot(Request::get(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap()
                .is_empty());
            assert_eq!(
                timing.resolutions.load(Ordering::SeqCst),
                if phase == 0 { 2 } else { 1 }
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn vod_resolution_range_open_and_probe_share_one_deadline() {
    for (suffix, delays) in [
        ("", [60, 60, 0]),
        ("/manifest.m3u8", [40, 40, 40]),
        ("/seg/0.ts", [40, 40, 40]),
    ] {
        let timing = VodTiming::new(delays);
        let app = vod_provider_router(Arc::new(TimedVod(timing.clone())));
        let started = tokio::time::Instant::now();
        let response = app
            .oneshot(
                Request::get(format!("/vod/timed-vod/x{suffix}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(started.elapsed(), Duration::from_millis(100));
        assert_eq!(timing.active.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn vod_waiter_keeps_its_deadline_when_a_cancelled_resolver_is_replaced() {
    let timing = VodTiming::new([200, 0, 0]);
    let app = vod_provider_router(Arc::new(TimedVod(timing.clone())));
    let request = || {
        Request::get("/vod/timed-vod/x")
            .body(Body::empty())
            .unwrap()
    };
    let mut owner = Box::pin(app.clone().oneshot(request()));
    assert!(futures::poll!(owner.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(50)).await;
    let mut waiter = Box::pin(app.clone().oneshot(request()));
    assert!(futures::poll!(waiter.as_mut()).is_pending());
    assert_eq!(timing.resolutions.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(50)).await;
    assert_eq!(owner.await.unwrap().status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(futures::poll!(waiter.as_mut()).is_pending());
    assert_eq!(timing.resolutions.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_millis(50)).await;
    assert_eq!(waiter.await.unwrap().status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(timing.active.load(Ordering::SeqCst), 0);
    timing.bypass.store(true, Ordering::SeqCst);
    assert_eq!(
        app.oneshot(request()).await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test(start_paused = true)]
async fn raw_vod_body_can_outlive_the_startup_budget() {
    let timing = VodTiming::new([0, 0, 200]);
    let app = vod_provider_router(Arc::new(TimedVod(timing)));
    let response = app
        .oneshot(
            Request::get("/vod/timed-vod/x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), synthetic_vod_ts(8));
}
