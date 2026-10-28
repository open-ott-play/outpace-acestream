use super::*;

async fn server_api_json(state: AppState, query: &str) -> (StatusCode, serde_json::Value) {
    control_api_json(state, "/server/api", query).await
}

async fn control_api_json(
    state: AppState,
    path: &str,
    query: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = router(state)
        .oneshot(
            Request::get(format!("{path}?{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, json)
}

#[tokio::test]
async fn server_api_get_version_returns_the_result_envelope() {
    let (status, json) = server_api_json(ace_compat_state(0), "method=get_version").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["error"], serde_json::Value::Null);
    assert_eq!(json["result"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(json["result"]["code"].is_u64());
    assert_eq!(json["result"]["platform"], std::env::consts::OS);
}

#[tokio::test(start_paused = true)]
async fn server_api_get_status_counts_managed_hls_until_stop_or_expiry() {
    let mut state = ace_compat_state(0);
    let relay = crate::origin_hls::OriginHls::new(ace_swarm::http_fetch::FetchPolicy::default());
    state.origin_hls = Some(relay.clone());
    let assert_count = async |expected| {
        for path in ["/server/api", "/webui/api/service"] {
            let (status, json) = control_api_json(state.clone(), path, "method=get_status").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(json["error"], serde_json::Value::Null);
            assert_eq!(json["result"]["active_sessions"], expected);
            assert_eq!(
                json["result"]["status"],
                if expected == 0 { "idle" } else { "dl" }
            );
        }
    };
    assert_count(0).await;
    // Admission is lazy and does not resolve DNS or fetch this synthetic URL.
    let playback = relay
        .start("https://origin.example.test/synthetic.m3u8")
        .await
        .unwrap();
    assert_count(1).await;
    let response = router(state.clone())
        .oneshot(
            Request::delete(playback.stop_url)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_count(0).await;
    relay
        .start("https://origin.example.test/synthetic.m3u8")
        .await
        .unwrap();
    assert_count(1).await;
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    assert_count(0).await;

    let _session = state
        .manager
        .get_or_start("fix", "synthetic-status")
        .await
        .unwrap();
    assert_count(1).await;
    relay
        .start("https://origin.example.test/synthetic.m3u8")
        .await
        .unwrap();
    assert_count(2).await;
}

#[tokio::test]
async fn get_version_success_bytes_include_the_healthcheck_spacing_on_both_aliases() {
    for (path, query) in [
        ("/server/api", "method=get_version"),
        ("/webui/api/service", "method=get_version"),
    ] {
        let response = router(ace_compat_state(0))
            .oneshot(
                Request::get(format!("{path}?{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("\"error\": null"), "{path}: {text}");
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], serde_json::Value::Null);
        assert_eq!(json["result"]["version"], env!("CARGO_PKG_VERSION"));
    }
}

#[tokio::test]
async fn webui_service_get_version_matches_server_api() {
    let (status, json) = control_api_json(
        ace_compat_state(0),
        "/webui/api/service",
        "method=get_version",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, server_json) = server_api_json(ace_compat_state(0), "method=get_version").await;
    assert_eq!(json, server_json);
}

#[tokio::test]
async fn server_api_analyze_content_echoes_a_direct_infohash_lowercased() {
    let upper = "50E93529D3EB46A50506B14464185A15292D6E47";
    let (status, json) = server_api_json(
        ace_compat_state(0),
        &format!("method=analyze_content&infohash={upper}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["error"], serde_json::Value::Null);
    assert_eq!(json["result"]["infohash"], upper.to_lowercase());
    assert_eq!(json["result"]["is_live"], -1);
    assert_eq!(json["result"]["is_encrypted"], -1);
}

#[tokio::test]
async fn server_api_get_content_id_echoes_the_supplied_content_id() {
    let (_, json) = server_api_json(
        ace_compat_state(0),
        "method=get_content_id&content_id=abc123",
    )
    .await;
    assert_eq!(json["error"], serde_json::Value::Null);
    assert_eq!(json["result"]["content_id"], "abc123");
}

#[tokio::test]
async fn server_api_get_content_id_errors_when_it_cannot_be_derived() {
    let (status, json) = server_api_json(
        ace_compat_state(0),
        "method=get_content_id&infohash=50e93529d3eb46a50506b14464185a15292d6e47",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["result"], serde_json::Value::Null);
    assert!(json["error"].is_string());
}

#[tokio::test]
async fn server_api_get_media_files_does_not_invent_files_for_an_unresolved_infohash() {
    let ih = "aa".repeat(20);
    for path in ["/server/api", "/webui/api/service"] {
        let (_, json) = control_api_json(
            ace_compat_state(0),
            path,
            &format!("method=get_media_files&infohash={ih}&mode=full&expand_wrapper=1"),
        )
        .await;
        assert_eq!(json["error"], serde_json::Value::Null);
        assert_eq!(json["result"]["infohash"], ih);
        assert_eq!(json["result"]["files"], serde_json::json!([]));
        assert_eq!(json["result"]["is_live"], -1);
        assert_eq!(json["result"]["is_encrypted"], -1);
        for unknown in ["type", "mime", "size", "wrapper_data"] {
            assert!(json["result"].get(unknown).is_none(), "{path}: {unknown}");
        }
    }
}

#[tokio::test]
async fn pinned_sdk_default_selection_allows_a_json_playback_request() {
    // Android SDK PlaybackData (65a6e53) initializes streamIndex=-1. The Android
    // engine client (3798163) emits both _idx and stream_id even without a variant.
    let id = "0123456789abcdef0123456789abcdef01234567";
    let (status, json) = control_api_json(
        ace_compat_state(0),
        "/ace/getstream",
        &format!("format=json&sid=synthetic-player&_idx=0&stream_id=-1&infohash={id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["error"], serde_json::Value::Null);
    assert!(json["response"]["playback_url"].is_string());
    assert!(json["response"]["command_url"].is_string());

    for selection in ["_idx=1&stream_id=-1", "_idx=0&stream_id=0"] {
        let (_, json) = control_api_json(
            ace_compat_state(0),
            "/ace/getstream",
            &format!("format=json&sid=synthetic-player&{selection}&infohash={id}"),
        )
        .await;
        assert!(json["error"].is_string());
        assert_eq!(json["response"], serde_json::Value::Null);
    }
}

#[tokio::test]
async fn server_api_get_network_connection_status_reports_connected() {
    let (_, json) =
        server_api_json(ace_compat_state(0), "method=get_network_connection_status").await;
    assert_eq!(json["error"], serde_json::Value::Null);
    assert_eq!(json["result"]["connected"], true);
}

#[tokio::test]
async fn server_api_unknown_method_returns_error_with_http_200() {
    for path in ["/server/api", "/webui/api/service"] {
        let (status, json) =
            control_api_json(ace_compat_state(0), path, "method=get_available_channels").await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(json["result"], serde_json::Value::Null, "{path}");
        assert!(
            json["error"].as_str().unwrap().contains("unknown method"),
            "{path}"
        );
    }
}

#[tokio::test]
async fn server_api_analyze_content_without_a_selector_errors() {
    let (_, json) = server_api_json(ace_compat_state(0), "method=analyze_content").await;
    assert_eq!(json["result"], serde_json::Value::Null);
    assert!(json["error"].is_string());
}

#[tokio::test]
async fn server_api_content_id_resolution_errors_when_catalog_lookup_is_disabled() {
    // ace_compat_state keeps resolve_content_ids_in_getstream = false, so a content_id that
    // needs a live catalog lookup returns a structured error instead of touching the network.
    let (status, json) = server_api_json(
        ace_compat_state(0),
        "method=analyze_content&content_id=2123456789abcdef0123456789abcdef01234567",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["result"], serde_json::Value::Null);
    assert!(json["error"].is_string());
}

#[tokio::test]
async fn server_api_is_gated_by_experimental_ace_compat() {
    // fixture_state leaves the compat surface off, so neither control route is registered.
    for path in ["/server/api", "/webui/api/service"] {
        let (status, _) = control_api_json(fixture_state(0), path, "method=get_version").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}
