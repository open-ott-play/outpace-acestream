use super::*;
use ace_wire::{bencode::Bencode, protection::Protection};

fn hls_descriptor(premium: Bencode) -> Vec<u8> {
    let mut root = Bencode::parse(
        include_bytes!("../../../../../tests/vectors/hls-descriptor.bencode")
            .strip_suffix(b"\n")
            .unwrap(),
    )
    .unwrap();
    let Bencode::Dict(fields) = &mut root else {
        unreachable!()
    };
    fields.insert(b"premium".to_vec(), premium);
    ace_wire::transport::encode_transport(&root)
}

async fn response_body(state: AppState, url: &str) -> (StatusCode, serde_json::Value) {
    let response = router(state)
        .oneshot(Request::get(url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn protected_hls_url_cannot_create_a_playback_or_origin_lease() {
    for (premium, message) in [
        (Bencode::Int(1), "encrypted content is not supported"),
        (Bencode::Int(2), "invalid content protection metadata"),
    ] {
        for route in ["/ace/getstream", "/ace/manifest.m3u8"] {
            let mut state = ace_compat_state(0);
            state.resolve_content_ids_in_getstream = true;
            let relay =
                crate::origin_hls::OriginHls::new(ace_swarm::http_fetch::FetchPolicy::default());
            state.origin_hls = Some(relay.clone());
            let query =
                format!("{route}?url=https%3A%2F%2Fdescriptor.example.test%2Ffixture&format=json");
            let (_, body) = URL_DESCRIPTOR_BYTES
                .scope(
                    hls_descriptor(premium.clone()),
                    response_body(state.clone(), &query),
                )
                .await;
            assert_eq!(body["response"], serde_json::Value::Null);
            assert_eq!(body["error"], message);
            assert_eq!(relay.active_count(), 0);
            assert!(state.manager.list().await.is_empty());
        }
    }
}

#[tokio::test]
async fn protected_hls_analysis_reports_flag_without_a_playback_url() {
    let mut state = ace_compat_state(0);
    state.resolve_content_ids_in_getstream = true;
    let query =
        "/server/api?method=analyze_content&url=https%3A%2F%2Fdescriptor.example.test%2Ffixture";
    let (_, body) = URL_DESCRIPTOR_BYTES
        .scope(
            hls_descriptor(Bencode::Int(1)),
            response_body(state.clone(), query),
        )
        .await;
    assert_eq!(body["error"], serde_json::Value::Null);
    assert_eq!(body["result"]["is_encrypted"], 1);
    assert_eq!(body["result"]["playback_url"], serde_json::Value::Null);
    assert!(state.manager.list().await.is_empty());
    let (_, public) = URL_DESCRIPTOR_BYTES
        .scope(hls_descriptor(Bencode::Int(0)), response_body(state, query))
        .await;
    assert_eq!(
        public["result"]["is_encrypted"], -1,
        "a public descriptor does not establish manifest encryption"
    );
    assert!(public["result"]["playback_url"].as_str().is_some());
}

struct RefusingProvider;
#[async_trait]
impl StreamProvider for RefusingProvider {
    fn network(&self) -> &'static str {
        "ace"
    }
    async fn open(&self, id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        Err(if id == "invalid" {
            ProviderError::InvalidProtection
        } else if id == "capacity" {
            ProviderError::CapacityExhausted
        } else {
            ProviderError::ProtectedContent
        })
    }
    async fn resolve_vod(&self, id: &str) -> Result<Box<dyn VodContent>, ProviderError> {
        Err(if id == "invalid" {
            ProviderError::InvalidProtection
        } else if id == "capacity" {
            ProviderError::CapacityExhausted
        } else {
            ProviderError::ProtectedContent
        })
    }
}

#[tokio::test]
async fn native_protection_and_capacity_errors_have_stable_status_and_retry_hint() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RefusingProvider));
    let state = AppState {
        manager: StreamManager::new(registry),
        networks: vec!["ace".into()],
        ..ace_compat_state(0)
    };
    for prefix in ["/streams/ace", "/vod/ace"] {
        for (id, expected, message) in [
            (
                "protected",
                StatusCode::UNPROCESSABLE_ENTITY,
                "encrypted content is not supported",
            ),
            (
                "invalid",
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid content protection metadata",
            ),
            (
                "capacity",
                StatusCode::SERVICE_UNAVAILABLE,
                "session capacity reached",
            ),
        ] {
            let response = router(state.clone())
                .oneshot(
                    Request::get(format!("{prefix}/{id}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            if id == "capacity" {
                assert_eq!(response.headers()[header::RETRY_AFTER], "1");
            }
            let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            assert_eq!(std::str::from_utf8(&body).unwrap(), message);
        }
    }
}

#[test]
fn protected_swarm_analysis_preserves_authoritative_metadata() {
    let content = crate::server_api::ResolvedContent::Swarm {
        protection: Some(Protection::Protected),
        infohash: "0123456789abcdef0123456789abcdef01234567".into(),
        content_id: None,
        is_live: true,
        total_length: None,
        filename: None,
    };
    assert_eq!(
        crate::server_api::analyze_result(&content)["is_encrypted"],
        1
    );
}

struct CachedPolicyProvider(std::sync::atomic::AtomicBool);
#[async_trait]
impl StreamProvider for CachedPolicyProvider {
    fn network(&self) -> &'static str {
        "ace"
    }
    fn check_openable(&self, _: &str) -> Result<(), ProviderError> {
        if self.0.load(Ordering::SeqCst) {
            Err(ProviderError::ProtectedContent)
        } else {
            Ok(())
        }
    }
    async fn open(&self, _: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        Ok(Box::new(FixtureSource { pos: 0 }))
    }
}

#[tokio::test]
async fn cached_native_and_bound_compat_sources_recheck_protection() {
    let provider = Arc::new(CachedPolicyProvider(std::sync::atomic::AtomicBool::new(
        false,
    )));
    let mut registry = ProviderRegistry::new();
    registry.register(provider.clone());
    let state = AppState {
        manager: StreamManager::new(registry),
        networks: vec!["ace".into()],
        ..ace_compat_state(0)
    };
    let session = state
        .manager
        .get_or_start("ace", "synthetic")
        .await
        .unwrap();
    let token = state
        .ace_sessions
        .mint("synthetic".into(), "synthetic".into());
    assert!(state
        .ace_sessions
        .bind_generation("synthetic", &token, &session, None));
    provider.0.store(true, Ordering::SeqCst);
    for url in [
        "/streams/ace/synthetic".to_string(),
        format!("/ace/r/synthetic/{token}"),
    ] {
        let response = router(state.clone())
            .oneshot(Request::get(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            "encrypted content is not supported"
        );
    }
    state.manager.stop("ace", "synthetic").await;
}

#[tokio::test]
async fn trusted_protected_inspection_survives_infohash_and_magnet_lookup() {
    let (bytes, _) = crate::ace_provider::test_support::synthetic_live_transport(65536);
    let mut root = ace_wire::transport::decode_transport(&bytes).unwrap().raw;
    let Bencode::Dict(fields) = &mut root else {
        unreachable!()
    };
    fields.insert(b"premium".to_vec(), Bencode::Int(1));
    let bytes = ace_wire::transport::encode_transport(&root);
    let provider = Arc::new(crate::ace_provider::AceProvider::new(
        Arc::new(ace_wire::identity::Identity::generate()),
        0,
    ));
    let mut registry = ProviderRegistry::new();
    registry.register(provider);
    let state = AppState {
        manager: StreamManager::new(registry),
        networks: vec!["ace".into()],
        resolve_content_ids_in_getstream: true,
        ..ace_compat_state(0)
    };
    let (_, first) = CATALOG_DESCRIPTOR_BYTES.scope(bytes, response_body(state.clone(), "/server/api?method=analyze_content&content_id=0123456789abcdef0123456789abcdef01234567")).await;
    assert_eq!(first["error"], serde_json::Value::Null);
    assert_eq!(first["result"]["is_encrypted"], 1);
    let hash = first["result"]["infohash"].as_str().unwrap();
    for selector in [
        format!("infohash={hash}"),
        format!("magnet=magnet%3A%3Fxt%3Durn%3Abtih%3A{hash}"),
    ] {
        let (_, body) = response_body(
            state.clone(),
            &format!("/server/api?method=analyze_content&{selector}"),
        )
        .await;
        assert_eq!(body["error"], serde_json::Value::Null);
        assert_eq!(body["result"]["is_encrypted"], 1);
        assert_eq!(body["result"]["infohash"], hash);
        let (_, denied) = response_body(
            state.clone(),
            &format!("/ace/getstream?format=json&{selector}"),
        )
        .await;
        assert_eq!(denied["response"], serde_json::Value::Null);
        assert_eq!(denied["error"], "encrypted content is not supported");
    }
    assert!(state.manager.list().await.is_empty());
}

#[tokio::test]
async fn warm_hls_segments_recheck_protection_for_native_compat_and_timeshift() {
    struct PolicyHlsProvider(std::sync::atomic::AtomicBool);
    #[async_trait]
    impl StreamProvider for PolicyHlsProvider {
        fn network(&self) -> &'static str {
            "ace"
        }
        fn check_openable(&self, _: &str) -> Result<(), ProviderError> {
            if self.0.load(Ordering::SeqCst) {
                Err(ProviderError::ProtectedContent)
            } else {
                Ok(())
            }
        }
        async fn open(&self, _: &str) -> Result<Box<dyn TsSource>, ProviderError> {
            Ok(Box::new(PacedPacketSource { packet_index: 0 }))
        }
    }
    let provider = Arc::new(PolicyHlsProvider(std::sync::atomic::AtomicBool::new(false)));
    let mut registry = ProviderRegistry::new();
    registry.register(provider.clone());
    let state = AppState {
        manager: StreamManager::with_config(
            registry,
            32,
            crate::config::HlsConfig {
                segment_packets: 4,
                window_segments: 3,
                segment_duration_ms: 1000,
                startup_segments: 1,
                ..crate::config::HlsConfig::default()
            },
        ),
        networks: vec!["ace".into()],
        ..ace_compat_state(0)
    };
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let app = router(state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/ace/manifest.m3u8?infohash={hash}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    let playlist_path = response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .strip_prefix("http://127.0.0.1:6878")
        .unwrap()
        .to_owned();
    let playlist = wait_for_hls_playlist(&app, &playlist_path).await;
    let segment_path = playlist_segment_path(&playlist).to_owned();
    let segment = segment_path.rsplit('/').next().unwrap();
    let token = segment_path.split('/').nth(3).unwrap();
    let paths = [
        segment_path.clone(),
        format!("/ace/tc/{token}/0/{segment}"),
        format!("/streams/ace/ih:{hash}/seg/{segment}"),
    ];
    for path in &paths {
        let positive = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            positive.status(),
            StatusCode::OK,
            "public segment path: {path}"
        );
    }
    provider.0.store(true, Ordering::SeqCst);
    for path in paths {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "encrypted content is not supported"
        );
    }
    state.manager.stop("ace", hash).await;
}
