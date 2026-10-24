use super::*;
use ace_swarm::resolve::{
    resolve_playback_from_url, resolve_playback_via_catalog, ResolveError, ResolvedTransport,
};

pub(super) async fn ace_getstream(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mode = match params.get("format").map(String::as_str) {
        None | Some("") => AceGetstreamMode::Direct,
        Some("json") => AceGetstreamMode::Json,
        Some(_) => {
            return Json(json!({ "response": null, "error": "unsupported format" }))
                .into_response();
        }
    };
    let selection = match resolve_ace_selection(&s, &params).await {
        Ok(selection) => selection,
        Err(error) => {
            return Json(json!({ "response": null, "error": error })).into_response();
        }
    };
    if selection.hls_manifest_url.is_some() {
        return origin_hls_response(&selection, mode == AceGetstreamMode::Json);
    }
    let Some(network) = ace_network(&s) else {
        return Json(json!({ "response": null, "error": "no ace network registered" }))
            .into_response();
    };
    let token = s
        .ace_sessions
        .mint(selection.playback_id.clone(), selection.session_key.clone());
    let base = request_base(&headers);
    let playback_id = selection.playback_id;
    let public_id = selection.public_id;
    let metadata = selection.metadata;
    if mode == AceGetstreamMode::Json {
        return Json(json!({
            "response": {
                "infohash": public_id,
                "playback_url": format!("{base}/ace/r/{playback_id}/{token}"),
                "stat_url": format!("{base}/ace/stat/{playback_id}/{token}"),
                "command_url": format!("{base}/ace/cmd/{playback_id}/{token}"),
                "playback_session_id": token,
                "client_session_id": -1,
                "is_live": 1,
                "is_encrypted": 0,
                "metadata": stream_metadata_json(&metadata)
            },
            "error": null
        }))
        .into_response();
    }

    // A direct request owns an ordinary compatibility lease, just like a caller that follows
    // the JSON playback URL. The body holds exactly one StreamManager subscription, so dropping
    // it detaches this caller without stopping a shared source or another compatibility client.
    let Some((session_key, expires_at, cancel)) = s.ace_sessions.playback(&playback_id, &token)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match s.manager.get_or_start(&network, &session_key).await {
        Ok(session) => {
            if !s
                .ace_sessions
                .bind_generation(&playback_id, &token, &session, None)
            {
                return StatusCode::NOT_FOUND.into_response();
            }
            ace_stream_session_response(
                session,
                expires_at,
                cancel,
                Some(AceDirectLeaseGuard {
                    sessions: s.ace_sessions.clone(),
                    playback_id,
                    token,
                }),
            )
        }
        Err(_) => {
            s.ace_sessions.revoke(&playback_id, &token);
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// `GET /ace/manifest.m3u8` — start one native live HLS session. The default/`redirect`
/// response points at a stable, token-authenticated playlist URL; `format=json` returns the
/// official documented control-URL shape. Both paths use the same native `HlsPackager`.
pub(super) async fn ace_manifest(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let format = params
        .get("format")
        .map(String::as_str)
        .unwrap_or("redirect");
    if !matches!(format, "" | "redirect" | "json") {
        return Json(json!({ "response": null, "error": "unsupported format" })).into_response();
    }
    let selection = match resolve_ace_selection(&s, &params).await {
        Ok(selection) => selection,
        Err(error) => {
            return Json(json!({ "response": null, "error": error })).into_response();
        }
    };
    if selection.hls_manifest_url.is_some() {
        return origin_hls_response(&selection, format == "json");
    }
    let Some(network) = ace_network(&s) else {
        return Json(json!({ "response": null, "error": "no ace network registered" }))
            .into_response();
    };

    let (pkg, session, pin) = match s
        .manager
        .get_or_start_compatibility_hls(&network, &selection.session_key)
        .await
    {
        Ok(started) => started,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let token = s
        .ace_sessions
        .mint_hls(selection.playback_id.clone(), selection.session_key, pin);
    if !s
        .ace_sessions
        .bind_generation(&selection.playback_id, &token, &session, Some(&pkg))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let base = request_base(&headers);
    let playback_url = format!("{base}/ace/m/{}/{}.m3u8", selection.playback_id, token);

    if format == "json" {
        return Json(json!({
            "response": {
                "playback_url": playback_url,
                "stat_url": format!("{base}/ace/stat/{}/{token}", selection.playback_id),
                "command_url": format!("{base}/ace/cmd/{}/{token}", selection.playback_id),
                "infohash": selection.public_id,
                "playback_session_id": token,
                "is_live": 1,
                "is_encrypted": 0,
                "client_session_id": -1,
                "metadata": stream_metadata_json(session.metadata()),
                "timeshift": timeshift_status(&s, &selection.playback_id, &token).await
            },
            "error": null
        }))
        .into_response();
    }

    // The documented engine default is a redirect. This also gives media players a stable URL
    // to reload, avoiding a fresh lease on every HLS playlist refresh.
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, playback_url),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
    )
        .into_response()
}

/// Tokenized HLS playlist returned by `/ace/manifest.m3u8` JSON/redirect modes.
pub(super) async fn ace_hls_playback(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path((id, manifest)): Path<(String, String)>,
) -> Response {
    let timeout = s.manager.hls_startup_timeout();
    ace_hls_playback_with_timeout(State(s), headers, Path((id, manifest)), timeout).await
}

pub(super) async fn ace_hls_playback_with_timeout(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path((id, manifest)): Path<(String, String)>,
    timeout: Duration,
) -> Response {
    let Some(token) = manifest.strip_suffix(".m3u8") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ace_hls_view_playback(s, headers, id, token.to_string(), 0, timeout, true).await
}

/// A generation-specific Outpace replay/live view. These are not official Ace API URLs.
pub(super) async fn ace_timeshift_playback(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path((id, token, manifest)): Path<(String, String, String)>,
) -> Response {
    let Some(generation) = manifest
        .strip_suffix(".m3u8")
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let timeout = s.manager.hls_startup_timeout();
    ace_hls_view_playback(s, headers, id, token, generation, timeout, false).await
}

async fn ace_hls_view_playback(
    s: AppState,
    headers: HeaderMap,
    id: String,
    token: String,
    generation: u64,
    timeout: Duration,
    legacy: bool,
) -> Response {
    let token = token.as_str();
    let Some((view, mut changed)) = s.ace_sessions.hls_view(&id, token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if view.generation != generation {
        return StatusCode::GONE.into_response();
    }
    let Some(_request) = s.ace_sessions.begin_hls_request(&id, token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((session_key, expires_at, mut cancel)) = s.ace_sessions.playback(&id, token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(network) = ace_network(&s) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(pkg) = s.manager.get_hls(&network, &session_key).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !s.ace_sessions.matches_hls(&id, token, &pkg) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Sampled proxies rewrite absolute engine URLs into their own segment catch-all route.
    // Reuse the playback/control URL policy without trusting additional proxy headers.
    let prefix = if legacy {
        format!("{}/ace/c/{token}", request_base(&headers))
    } else {
        format!("{}/ace/tc/{token}/{generation}", request_base(&headers))
    };
    let render = async {
        match &view.history {
            Some(history) => pkg
                .history_playlist(history, &prefix)
                .map_err(|error| Box::new(history_error_response(error))),
            None => await_hls_playlist(&pkg, timeout, &prefix, false).await,
        }
    };
    let playlist = tokio::select! {
        biased;
        _ = cancel.changed() => return StatusCode::NOT_FOUND.into_response(),
        _ = changed.changed() => return StatusCode::GONE.into_response(),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(expires_at)) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        playlist = render => playlist,
    };
    // Removal/replacement and authorization take precedence over both snapshots and 503s.
    // Revalidate the lease after the asynchronous manager lookup as well.
    let current = s.manager.get_hls(&network, &session_key).await;
    if !current.is_some_and(|current| Arc::ptr_eq(&current, &pkg))
        || *cancel.borrow()
        || Instant::now() >= expires_at
        || s.ace_sessions.validate(&id, token).as_deref() != Some(session_key.as_str())
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !s
        .ace_sessions
        .hls_view(&id, token)
        .is_some_and(|(view, _)| view.generation == generation)
    {
        return StatusCode::GONE.into_response();
    }
    let playlist = match playlist {
        Ok(playlist) => playlist,
        Err(response) => return *response,
    };
    (
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        playlist,
    )
        .into_response()
}

/// Authenticated view of a retained native live HLS segment. Probes never start work.
pub(super) async fn ace_hls_segment(
    State(s): State<AppState>,
    Path((token, segment)): Path<(String, String)>,
) -> Response {
    ace_hls_view_segment(s, token, 0, segment).await
}

pub(super) async fn ace_timeshift_segment(
    State(s): State<AppState>,
    Path((token, generation, segment)): Path<(String, u64, String)>,
) -> Response {
    ace_hls_view_segment(s, token, generation, segment).await
}

async fn ace_hls_view_segment(
    s: AppState,
    token: String,
    generation: u64,
    segment: String,
) -> Response {
    let Some(seq) = segment
        .strip_suffix(".ts")
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((playback_id, session_key)) = s.ace_sessions.validate_token(&token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((view, _)) = s.ace_sessions.hls_view(&playback_id, &token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if view.generation != generation {
        return StatusCode::GONE.into_response();
    }
    let Some(network) = ace_network(&s) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(pkg) = s.manager.get_hls(&network, &session_key).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if s.ace_sessions
        .validate_token(&token)
        .map(|(_, key)| key)
        .as_deref()
        != Some(session_key.as_str())
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !s.ace_sessions.matches_hls(&playback_id, &token, &pkg) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !s
        .ace_sessions
        .hls_view(&playback_id, &token)
        .is_some_and(|(view, _)| view.generation == generation)
    {
        return StatusCode::GONE.into_response();
    }
    let bytes = match &view.history {
        Some(history) => {
            if !(history.first_seq..=history.last_seq).contains(&seq) {
                return StatusCode::NOT_FOUND.into_response();
            }
            pkg.history_segment(history, seq)
        }
        None => pkg
            .compatibility_segment(seq)
            .ok_or(crate::hls::HlsHistoryError::Unavailable),
    };
    match bytes {
        Ok(bytes) => {
            let Some(_request) = s.ace_sessions.begin_hls_request(&playback_id, &token) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            (
                [
                    (header::CONTENT_TYPE, "video/mp2t"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                bytes,
            )
                .into_response()
        }
        Err(error) => history_error_response(error),
    }
}

fn history_error_response(error: crate::hls::HlsHistoryError) -> Response {
    use crate::hls::HlsHistoryError;
    let status = match error {
        HlsHistoryError::Gone => StatusCode::GONE,
        HlsHistoryError::NotReady => StatusCode::SERVICE_UNAVAILABLE,
        HlsHistoryError::Disabled => StatusCode::CONFLICT,
        HlsHistoryError::Unavailable => StatusCode::NOT_FOUND,
        HlsHistoryError::InvalidTarget => StatusCode::BAD_REQUEST,
    };
    (status, error.to_string()).into_response()
}

pub(super) async fn ace_playback(
    State(s): State<AppState>,
    Path((id, token)): Path<(String, String)>,
) -> Response {
    let Some(network) = ace_network(&s) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((session_key, expires_at, cancel)) = s.ace_sessions.playback(&id, &token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let opened = match s.ace_sessions.bound_session(&id, &token) {
        Err(()) => return StatusCode::NOT_FOUND.into_response(),
        Ok(Some(bound)) => {
            let current = s.manager.get(&network, &session_key).await;
            if !current.is_some_and(|current| Arc::ptr_eq(&current, &bound)) {
                return StatusCode::NOT_FOUND.into_response();
            }
            Ok(bound)
        }
        Ok(None) => s.manager.get_or_start(&network, &session_key).await,
    };
    match opened {
        Ok(session) => {
            if !s.ace_sessions.bind_generation(&id, &token, &session, None) {
                return StatusCode::NOT_FOUND.into_response();
            }
            ace_stream_session_response(session, expires_at, cancel, None)
        }
        Err(error) => {
            // Provider messages may embed identifiers or credentials. Log only the typed
            // category, and keep both logs and response bodies independent of that text.
            let (status, category, reason) = match error {
                ProviderError::NotFound => (StatusCode::NOT_FOUND, "NotFound", ""),
                ProviderError::Backend(_) => {
                    (StatusCode::BAD_GATEWAY, "Backend", "stream start failed")
                }
                ProviderError::Unresolvable(_) => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Unresolvable",
                    "stream metadata unavailable",
                ),
            };
            crate::alog!(
                "[ace-compat] playback start failed: error={category} status={}",
                status.as_u16()
            );
            if status == StatusCode::NOT_FOUND {
                status.into_response()
            } else {
                (status, reason).into_response()
            }
        }
    }
}

pub(super) fn live_position_json(
    position: Option<&crate::provider::LivePositionStats>,
) -> serde_json::Value {
    match position {
        Some(pos) => json!({
            "pos": pos.pos, "last": pos.last,
            "live_first": pos.live_first, "live_last": pos.live_last,
            "buffer_pieces": pos.buffer_pieces,
            "first_ts": null, "last_ts": null, "is_live": 1,
        }),
        None => serde_json::Value::Null,
    }
}

pub(super) async fn ace_stat(
    State(s): State<AppState>,
    Path((id, token)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let Some(network) = ace_network(&s) else {
        return Json(json!({ "response": null, "error": "no ace network registered" }));
    };
    let Some(session_key) = s.ace_sessions.validate(&id, &token) else {
        return Json(json!({ "response": null, "error": "invalid or expired playback session" }));
    };
    let public_id = ace_public_id(&id);
    let Some(session) = s.manager.get(&network, &session_key).await else {
        return Json(json!({
            "response": {
                "status": "idle",
                "speed_down": 0, "speed_up": 0, "livepos": null,
                "peers": 0,
                "downloaded": 0,
                "uploaded": 0,
                "infohash": public_id,
                "is_live": 1,
                "is_encrypted": 0,
                "playback_session_id": token,
                "client_session_id": -1,
                "metadata": stream_metadata_json(&StreamMetadata::default())
            },
            "error": null
        }));
    };

    match s.ace_sessions.bound_session(&id, &token) {
        Err(()) => return Json(json!({"response": null, "error": "playback generation expired"})),
        Ok(Some(bound)) if !Arc::ptr_eq(&bound, &session) => {
            return Json(json!({"response": null, "error": "playback generation expired"}));
        }
        _ => {}
    }
    let stats = session.stats().await;
    let timeshift = timeshift_status(&s, &id, &token).await;
    Json(json!({
        "response": {
            "status": "dl",
            "speed_down": stats.download_rate as f64 / 1024.0,
            "speed_up": stats.upload_rate as f64 / 1024.0,
            "livepos": live_position_json(stats.livepos.as_ref()),
            "peers": stats.peers,
            "downloaded": stats.downloaded,
            "uploaded": stats.uploaded,
            "infohash": public_id,
            "is_live": 1,
            "is_encrypted": 0,
            "playback_session_id": token,
            "client_session_id": -1,
            "metadata": stream_metadata_json(session.metadata()),
            "timeshift": timeshift
        },
        "error": null
    }))
}

pub(super) async fn ace_command(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path((id, token)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        ace_command_json(s, headers, id, token, params).await,
    )
        .into_response()
}

async fn ace_command_json(
    s: AppState,
    headers: HeaderMap,
    id: String,
    token: String,
    params: HashMap<String, String>,
) -> Json<serde_json::Value> {
    if s.ace_sessions.validate(&id, &token).is_none() {
        return Json(json!({ "response": null, "error": "invalid or expired playback session" }));
    }
    let method = params.get("method").map(String::as_str).unwrap_or("");
    if method == "stop" {
        if !s.ace_sessions.revoke(&id, &token) {
            return Json(
                json!({ "response": null, "error": "invalid or expired playback session" }),
            );
        }
        return Json(json!({ "response": "ok", "error": null }));
    }
    if method == "timeshift_status" {
        return Json(json!({ "response": timeshift_status(&s, &id, &token).await, "error": null }));
    }
    if !matches!(method, "timeshift_seek" | "timeshift_live") {
        return Json(json!({ "response": null, "error": "unknown or missing method" }));
    }
    let Some((view, _)) = s.ace_sessions.hls_view(&id, &token) else {
        return Json(
            json!({ "response": null, "error": "timeshift requires an HLS playback session" }),
        );
    };
    if let Some(expected) = params.get("generation") {
        if expected.parse::<u64>().ok() != Some(view.generation) {
            return Json(json!({ "response": null, "error": "playback generation changed" }));
        }
    }
    let Some(network) = ace_network(&s) else {
        return Json(json!({ "response": null, "error": "no ace network registered" }));
    };
    let Some(session_key) = s.ace_sessions.validate(&id, &token) else {
        return Json(json!({ "response": null, "error": "invalid or expired playback session" }));
    };
    let Some(pkg) = s.manager.get_hls(&network, &session_key).await else {
        return Json(json!({ "response": null, "error": "playback generation expired" }));
    };
    if !s.ace_sessions.matches_hls(&id, &token, &pkg) {
        return Json(json!({ "response": null, "error": "playback generation expired" }));
    }
    // Preparation never changes the existing viewer. In this beta a seek is a finite
    // replay of already-completed, retained segments, not a peer-network archive.
    let history = if method == "timeshift_seek" {
        let Some(back) = params
            .get("back")
            .and_then(|value| value.parse::<f64>().ok())
        else {
            return Json(
                json!({ "response": null, "error": "back must be finite positive seconds" }),
            );
        };
        if !back.is_finite() || back <= 0.0 || back > 31_536_000.0 {
            return Json(
                json!({ "response": null, "error": "back must be finite positive seconds" }),
            );
        }
        match pkg.prepare_history(Duration::from_secs_f64(back)) {
            Ok(history) => Some(history),
            Err(error) => return Json(json!({ "response": null, "error": error.to_string() })),
        }
    } else {
        if pkg.playlist_snapshot("", false).is_err() {
            return Json(json!({ "response": null, "error": "live playback is not ready" }));
        }
        None
    };
    let Some(view) = s
        .ace_sessions
        .replace_hls_view(&id, &token, &pkg, view.generation, history)
    else {
        return Json(
            json!({ "response": null, "error": "playback generation changed or expired" }),
        );
    };
    let playback_url = format!(
        "{}/ace/t/{id}/{token}/{}.m3u8",
        request_base(&headers),
        view.generation
    );
    Json(json!({
        "response": {
            "playback_url": playback_url,
            "reload_required": true,
            "view": timeshift_view_json(&view),
            "history": pkg.history_status(),
        },
        "error": null
    }))
}

fn timeshift_view_json(view: &super::leases::AceHlsView) -> serde_json::Value {
    json!({
        "generation": view.generation,
        "mode": if view.history.is_some() { "replay" } else { "live" },
        "snapshot": view.history.as_ref().map(|history| json!({
            "first_seq": history.first_seq,
            "last_seq": history.last_seq,
            "start_ms": history.start_ms,
            "end_ms": history.end_ms,
        })),
    })
}

async fn timeshift_status(s: &AppState, id: &str, token: &str) -> serde_json::Value {
    if s.ace_sessions.hls_view(id, token).is_none() {
        return json!({ "supported": false, "reason": "requires_hls_session" });
    }
    let Some(network) = ace_network(s) else {
        return json!({ "supported": false, "reason": "no_ace_network" });
    };
    let Some(key) = s.ace_sessions.validate(id, token) else {
        return json!({ "supported": false, "reason": "session_expired" });
    };
    let Some(pkg) = s.manager.get_hls(&network, &key).await else {
        return json!({ "supported": false, "reason": "source_expired" });
    };
    if !s.ace_sessions.matches_hls(id, token, &pkg) {
        return json!({ "supported": false, "reason": "source_expired" });
    }
    let Some((view, _)) = s.ace_sessions.hls_view(id, token) else {
        return json!({ "supported": false, "reason": "session_expired" });
    };
    json!({
        "supported": true,
        "kind": "finite_memory_replay",
        "persistent": false,
        "peer_archive": false,
        "utc_seek": false,
        "view": timeshift_view_json(&view),
        "history": pkg.history_status(),
    })
}

/// `GET /server/api` — the official-engine JSON control API (compatibility subset). Dispatches on
/// `?method=` and always answers HTTP 200 with a `{ "result", "error" }` envelope (note 10). Only
/// the methods in [`crate::server_api::Method`] are served; see `docs/protocol/compat-matrix.md`.
pub(super) async fn server_api(
    State(s): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    use crate::server_api as api;

    let method = params.get("method").map(String::as_str).unwrap_or("");
    let body = match api::parse_method(method) {
        api::Method::GetVersion => {
            // The sampled Compose healthcheck greps for the literal `"error": null`.
            // Pretty-print only this success envelope; its parsed fields stay identical.
            let body = serde_json::to_string_pretty(&api::ok(api::version_result()))
                .expect("version JSON is serializable");
            return ([(header::CONTENT_TYPE, "application/json")], body).into_response();
        }
        api::Method::GetStatus => api::ok(api::status_result(s.manager.list().await.len())),
        api::Method::GetNetworkConnectionStatus => api::ok(api::network_status_result(&s.networks)),
        // `get_content_id` only echoes an id the caller already gave; it can't derive one from a
        // bare infohash/url, so it never resolves.
        api::Method::GetContentId => match api::selector(&params) {
            api::Selector::ContentId(cid) => api::ok(api::content_id_result(&cid)),
            api::Selector::Missing => api::err("missing content_id"),
            _ => api::err("content_id unavailable for this selector"),
        },
        method @ (api::Method::AnalyzeContent | api::Method::GetMediaFiles) => {
            match resolve_server_api_selector(&s, api::selector(&params)).await {
                Ok(content) => match method {
                    api::Method::AnalyzeContent => api::ok(api::analyze_result(&content)),
                    api::Method::GetMediaFiles => api::ok(api::media_files_result(&content)),
                    _ => unreachable!("outer match restricts these two variants"),
                },
                Err(e) => api::err(e),
            }
        }
        api::Method::Unknown(name) => api::err(format!("unknown method: {name}")),
    };
    Json(body).into_response()
}

/// Resolve a `/server/api` content [`crate::server_api::Selector`] to its infohash. Direct
/// infohashes and magnets resolve offline; `content_id`/`url` need a catalog/transport fetch,
/// which is gated by the same `resolve_content_ids_in_getstream` switch tests use to stay offline
/// (production enables it). A best-effort failure is reported in-band, never as an HTTP error.
async fn resolve_server_api_selector(
    s: &AppState,
    sel: crate::server_api::Selector,
) -> Result<crate::server_api::ResolvedContent, String> {
    use crate::server_api::{ResolvedContent, Selector};

    if let Some(resolved) = crate::server_api::resolve_offline(&sel) {
        return resolved;
    }
    match sel {
        Selector::ContentId(cid) => {
            if !s.resolve_content_ids_in_getstream {
                return Err("content-id catalog resolution is disabled".to_string());
            }
            match resolve_via_catalog(&cid).await {
                Ok(info) => {
                    // `resolve_via_catalog` is the signed catalog, so this descriptor is indexable.
                    if let Some(network) = ace_network(s) {
                        s.manager.remember_live_descriptor(&network, &info);
                    }
                    Ok(ResolvedContent {
                        infohash: infohash_hex(&info.infohash),
                        content_id: Some(cid),
                        is_live: true,
                    })
                }
                Err(e) => Err(format!("content-id resolution failed: {e:?}")),
            }
        }
        Selector::Url(url) => {
            if !s.resolve_content_ids_in_getstream {
                return Err("transport-url resolution is disabled".to_string());
            }
            match stream_info_from_transport_url(&url).await {
                Ok(info) => Ok(ResolvedContent {
                    infohash: infohash_hex(&info.infohash),
                    content_id: None,
                    is_live: true,
                }),
                Err(e) => Err(format!("transport-url resolution failed: {e:?}")),
            }
        }
        Selector::Missing => Err("missing content_id/infohash/url/magnet".to_string()),
        Selector::Infohash(_) | Selector::Magnet(_) => {
            unreachable!("infohash/magnet selectors resolve offline")
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct AceStreamSelection {
    pub(super) public_id: String,
    pub(super) playback_id: String,
    pub(super) session_key: String,
    pub(super) content_id: Option<String>,
    pub(super) metadata: StreamMetadata,
    pub(super) hls_manifest_url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AceGetstreamMode {
    Direct,
    Json,
}

impl AceStreamSelection {
    pub(super) fn with_resolved_stream(
        mut self,
        infohash: String,
        metadata: StreamMetadata,
    ) -> Self {
        self.public_id = infohash.clone();
        self.playback_id = infohash;
        self.content_id = None;
        self.metadata = metadata;
        self
    }
}

async fn resolve_ace_selection(
    s: &AppState,
    params: &HashMap<String, String>,
) -> Result<AceStreamSelection, String> {
    let mut selection = ace_selected_stream(params)?;
    let network = ace_network(s);
    if s.resolve_content_ids_in_getstream {
        let from_catalog = selection.content_id.is_some();
        let resolution = if let Some(content_id) = selection.content_id.as_deref() {
            Some(resolve_playback_via_catalog(content_id).await)
        } else if let Some(url) = crate::transport_url::decode_transport_url(&selection.session_key)
        {
            Some(resolve_playback_from_url(&url).await)
        } else {
            None
        };
        if let Some(resolution) = resolution {
            if from_catalog {
                if let Ok(ResolvedTransport::Swarm(info)) = &resolution {
                    if let Some(network) = network.as_deref() {
                        s.manager.remember_live_descriptor(network, info);
                        // Open the indexed, validated descriptor without a second catalog fetch.
                        selection.session_key = infohash_hex(&info.infohash);
                    }
                }
            }
            selection =
                apply_transport_resolution(selection, resolution).map_err(str::to_string)?;
            if selection.hls_manifest_url.is_some() {
                return Ok(selection);
            }
        }
    }
    // The provider may reject a selector synchronously. Cold Ace infohashes proceed to
    // asynchronous authenticated catalog resolution when the playback URL is opened.
    if let Some(network) = network.as_deref() {
        if let Err(ProviderError::Unresolvable(reason)) =
            s.manager.check_openable(network, &selection.session_key)
        {
            return Err(reason);
        }
    }
    Ok(selection)
}

fn apply_transport_resolution(
    mut selection: AceStreamSelection,
    resolution: Result<ResolvedTransport, ResolveError>,
) -> Result<AceStreamSelection, &'static str> {
    match resolution {
        Ok(ResolvedTransport::Swarm(info)) => {
            Ok(selection.with_resolved_stream(infohash_hex(&info.infohash), info.metadata))
        }
        Ok(ResolvedTransport::Hls {
            manifest_url,
            metadata,
        }) => {
            crate::alog!("[ace] HLS descriptor: handing playback to the original manifest");
            selection.hls_manifest_url = Some(manifest_url);
            selection.metadata = metadata;
            Ok(selection)
        }
        // Recognized HLS descriptors do not describe a piece-based swarm, even if their
        // origin is missing or invalid. Retrying as cid would hide the actual error.
        Err(ResolveError::Hls(message)) => {
            crate::alog!("[ace] HLS descriptor rejected: {message}");
            Err(message)
        }
        Err(error) => {
            crate::alog!("[ace] transport resolution failed, falling back to provider: {error:?}");
            Ok(selection)
        }
    }
}

/// HLS descriptors refer to an existing origin. Do not mint fake swarm/control leases or
/// relabel an HLS manifest as MPEG-TS. The player follows the original URL and owns buffering.
fn origin_hls_response(selection: &AceStreamSelection, json_mode: bool) -> Response {
    let url = selection.hls_manifest_url.as_ref().expect("HLS selection");
    if json_mode {
        return (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({
                "response": {
                    "playback_url": url,
                    "content_id": selection.content_id,
                    "transport": "hls",
                    "metadata": stream_metadata_json(&selection.metadata),
                },
                "error": null
            })),
        )
            .into_response();
    }
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, url.clone()),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
    )
        .into_response()
}

pub(super) fn ace_selected_stream(
    params: &HashMap<String, String>,
) -> Result<AceStreamSelection, &'static str> {
    if let Some(content_id) = ace_nonempty_param(params, "content_id") {
        let content_id = ace_normalized_hex_id(content_id).ok_or("invalid content_id")?;
        return Ok(AceStreamSelection {
            public_id: content_id.clone(),
            playback_id: format!("cid:{content_id}"),
            session_key: format!("cid:{content_id}"),
            content_id: Some(content_id),
            metadata: StreamMetadata::default(),
            hls_manifest_url: None,
        });
    }
    if let Some(infohash) = ace_nonempty_param(params, "infohash") {
        let infohash = ace_normalized_hex_id(infohash).ok_or("invalid infohash")?;
        return Ok(AceStreamSelection {
            public_id: infohash.clone(),
            playback_id: infohash.clone(),
            session_key: infohash,
            content_id: None,
            metadata: StreamMetadata::default(),
            hls_manifest_url: None,
        });
    }
    // The legacy `id=` spelling denotes an AceStream content id, not a swarm infohash.
    if let Some(content_id) = ace_nonempty_param(params, "id") {
        let content_id = ace_normalized_hex_id(content_id).ok_or("invalid id")?;
        return Ok(AceStreamSelection {
            public_id: content_id.clone(),
            playback_id: format!("cid:{content_id}"),
            session_key: format!("cid:{content_id}"),
            content_id: Some(content_id),
            metadata: StreamMetadata::default(),
            hls_manifest_url: None,
        });
    }
    if let Some(url) = ace_nonempty_param(params, "url") {
        // The id is a reversible, path-safe encoding of the URL, so it is both the route-safe
        // playback_id and the session_key the provider decodes — no server-side alias table, so
        // playback survives a restart or a direct `/ace/r/{id}` hit.
        let token =
            crate::transport_url::encode_transport_url(url).map_err(|_| "invalid transport url")?;
        return Ok(AceStreamSelection {
            public_id: token.clone(),
            playback_id: token.clone(),
            session_key: token,
            content_id: None,
            metadata: StreamMetadata::default(),
            hls_manifest_url: None,
        });
    }
    if let Some(magnet) = ace_nonempty_param(params, "magnet") {
        let hex = crate::magnet::parse_magnet_infohash(magnet).map_err(|_| "invalid magnet")?;
        return Ok(AceStreamSelection {
            public_id: hex.clone(),
            playback_id: hex.clone(),
            session_key: hex,
            content_id: None,
            metadata: StreamMetadata::default(),
            hls_manifest_url: None,
        });
    }
    Err("missing content_id/infohash/id/url/magnet")
}

pub(super) fn ace_normalized_hex_id(id: &str) -> Option<String> {
    (id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_ascii_lowercase())
}

fn ace_nonempty_param<'a>(params: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .map(String::as_str)
        .filter(|id| !id.is_empty())
}

fn ace_public_id(id: &str) -> &str {
    id.strip_prefix("cid:").unwrap_or(id)
}

fn ace_network(s: &AppState) -> Option<String> {
    if s.networks.iter().any(|n| n == "ace") {
        Some("ace".to_string())
    } else {
        s.networks.first().cloned()
    }
}

fn request_base(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| !h.is_empty())
        .unwrap_or("127.0.0.1:6878");
    format!("http://{host}")
}

#[cfg(test)]
mod origin_tests {
    use super::*;
    fn params(values: &[(&str, &str)]) -> HashMap<String, String> {
        values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    fn synthetic_hls_selection() -> AceStreamSelection {
        let selection = ace_selected_stream(&params(&[(
            "id",
            "0123456789abcdef0123456789abcdef01234567",
        )]))
        .unwrap();
        let raw = ace_wire::bencode::Bencode::parse(
            include_bytes!("../../../../tests/vectors/hls-descriptor.bencode")
                .strip_suffix(b"\n")
                .unwrap(),
        )
        .unwrap();
        let bytes = ace_wire::transport::encode_transport(&raw);
        apply_transport_resolution(
            selection,
            ace_swarm::resolve::playback_from_transport(&bytes),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn hls_descriptor_redirect_preserves_signed_origin_and_disables_caching() {
        let selection = synthetic_hls_selection();
        let response = origin_hls_response(&selection, false);
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://cdn.example.test/live/master.m3u8?token=a%2Fb&expires=123"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        // A redirect lets the player resolve relative variants/segments/AES keys against
        // the original URL, without the engine rewriting signed URL/query bytes.
        assert!(axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn hls_descriptor_json_has_actual_transport_without_fake_swarm_controls() {
        let response = origin_hls_response(&synthetic_hls_selection(), true);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], serde_json::Value::Null);
        assert_eq!(json["response"]["transport"], "hls");
        assert_eq!(
            json["response"]["playback_url"],
            "https://cdn.example.test/live/master.m3u8?token=a%2Fb&expires=123"
        );
        assert_eq!(
            json["response"]["metadata"]["title"],
            "Synthetic HLS fixture"
        );
        for key in [
            "infohash",
            "stat_url",
            "command_url",
            "playback_session_id",
            "is_encrypted",
        ] {
            assert!(json["response"].get(key).is_none(), "must not invent {key}");
        }
    }

    #[test]
    fn recognized_hls_failures_never_fall_back_to_a_piece_based_provider() {
        let selection = ace_selected_stream(&params(&[(
            "id",
            "0123456789abcdef0123456789abcdef01234567",
        )]))
        .unwrap();
        assert_eq!(
            apply_transport_resolution(selection, Err(ResolveError::Hls("missing manifest"))),
            Err("missing manifest")
        );
    }
}
