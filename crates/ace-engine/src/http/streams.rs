use super::*;

pub(super) async fn networks(State(s): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "networks": s.networks }))
}

/// `GET /streams` — active sessions and their client counts.
pub(super) async fn list_streams(State(s): State<AppState>) -> Json<serde_json::Value> {
    let streams: Vec<_> = s
        .manager
        .list()
        .await
        .into_iter()
        .map(|(network, id, clients, metadata)| {
            json!({
                "network": network,
                "id": native_public_stream_id(&network, &id),
                "clients": clients,
                "metadata": stream_metadata_json(&metadata),
            })
        })
        .collect();
    let (occupied_sessions, max_sessions) = s.manager.admission_status();
    Json(
        json!({ "streams": streams, "occupied_sessions": occupied_sessions, "max_sessions": max_sessions }),
    )
}

/// Normalize native Ace selectors once, before any session lookup. Provider-facing bare
/// hashes remain infohash keys so compat and CLI semantics do not change.
fn native_stream_id<'a>(network: &str, id: &'a str) -> Result<Cow<'a, str>, &'static str> {
    if network != "ace" {
        return Ok(Cow::Borrowed(id));
    }
    if let Some(hex) = id.strip_prefix("ih:") {
        return ace_normalized_hex_id(hex)
            .map(Cow::Owned)
            .ok_or("ih: requires a 40-character hexadecimal infohash");
    }
    if let Some(hex) = id.strip_prefix("cid:") {
        return ace_normalized_hex_id(hex)
            .map(|hex| Cow::Owned(format!("cid:{hex}")))
            .ok_or("cid: requires a 40-character hexadecimal content id");
    }
    Ok(match ace_normalized_hex_id(id) {
        Some(hex) => Cow::Owned(format!("cid:{hex}")),
        None => Cow::Borrowed(id),
    })
}

/// Public native identifiers must round-trip through the native selector, including ids
/// originating in compat sessions whose internal infohash key has no prefix.
fn native_public_stream_id<'a>(network: &str, id: &'a str) -> Cow<'a, str> {
    if network == "ace" {
        if let Some(hex) = ace_normalized_hex_id(id) {
            return Cow::Owned(format!("ih:{hex}"));
        }
    }
    Cow::Borrowed(id)
}

/// `DELETE /streams/{network}/{id}` — force-stop a running session (admin). 204 if it was
/// running, 404 otherwise. The `{id}` may carry a `.ts`/`.m3u8` suffix (mirroring the GET
/// URL); it's stripped so either form stops the same session.
pub(super) async fn delete_stream(
    State(s): State<AppState>,
    Path((network, file)): Path<(String, String)>,
) -> Response {
    let id = file
        .strip_suffix(".ts")
        .or_else(|| file.strip_suffix(".m3u8"))
        .unwrap_or(&file);
    let id = match native_stream_id(&network, id) {
        Ok(id) => id,
        Err(reason) => return (StatusCode::BAD_REQUEST, reason).into_response(),
    };
    if s.manager.stop(&network, &id).await {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// `GET /streams/{network}/{id}/status` — stats for a running session (404 if not active).
pub(super) async fn stream_status(
    State(s): State<AppState>,
    Path((network, id)): Path<(String, String)>,
) -> Response {
    let id = match native_stream_id(&network, &id) {
        Ok(id) => id,
        Err(reason) => return (StatusCode::BAD_REQUEST, reason).into_response(),
    };
    // Read the observation before the session map: publication inserts the
    // session before dropping the attempt, so handoff never creates a false 404.
    let attempt = s.manager.start_status(&network, &id);
    match s.manager.get(&network, &id).await {
        Some(session) if attempt.is_none() || !session.is_finished() => {
            let stats = session.stats().await;
            let (phase, error) = match s
                .manager
                .get_hls(&network, &id)
                .await
                .map(|pkg| pkg.status())
            {
                Some(crate::hls::HlsStatus::Failed(failure)) if !session.produced_media() => (
                    crate::start_status::StartPhase::Failed,
                    Some(crate::start_status::StartError::from_normalization(failure)),
                ),
                _ if session.produced_media() => (crate::start_status::StartPhase::Playing, None),
                _ => (crate::start_status::StartPhase::Buffering, None),
            };
            Json(json!({
                "network": network,
                "id": native_public_stream_id(&network, &id),
                "phase": phase.as_str(),
                "error": error.map(|error| error.to_json()),
                "clients": session.subscriber_count(),
                "peers": stats.peers,
                "bitrate": stats.bitrate,
                "livepos": native_live_position_json(stats.livepos.as_ref()),
                "recovery": stats.recovery,
                "buffer_ms": stats.buffer_ms,
                "uploaded": stats.uploaded,
                "peers_served": stats.peers_served,
                "metadata": stream_metadata_json(session.metadata()),
            }))
            .into_response()
        }
        _ => match attempt {
            Some(attempt) => Json(json!({
                "network": network,
                "id": native_public_stream_id(&network, &id),
                "phase": attempt.phase().as_str(),
                "error": attempt.error().map(|error| error.to_json()),
            }))
            .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
    }
}

/// `GET /streams/{network}/{id}/seg/{n}.ts` — a retained HLS segment.
pub(super) async fn stream_segment(
    State(s): State<AppState>,
    Path((network, id, seg)): Path<(String, String, String)>,
) -> Response {
    let Some(seq) = seg.strip_suffix(".ts").and_then(|n| n.parse::<u64>().ok()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let id = match native_stream_id(&network, &id) {
        Ok(id) => id,
        Err(reason) => return (StatusCode::BAD_REQUEST, reason).into_response(),
    };
    if let Err(error @ (ProviderError::ProtectedContent | ProviderError::InvalidProtection)) =
        s.manager.check_openable(&network, &id)
    {
        return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response();
    }
    // Segment probes must NOT start a stream: only serve from an already-running packager
    // (started when the playlist was fetched). Unknown streams 404 without any provider work.
    let Some(pkg) = s.manager.get_hls(&network, &id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match pkg.segment(seq) {
        Some(bytes) => ([(header::CONTENT_TYPE, "video/mp2t")], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Provider-source capacity is distinct from the separately bounded origin HLS relay.
pub(super) fn capacity_error() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "1")],
        "session capacity reached",
    )
        .into_response()
}

/// Preserve native live status codes and use a typed body for provider failures.
async fn stream_start_error(state: &AppState, error: ProviderError) -> Response {
    let status = match &error {
        ProviderError::DirectHls { manifest_url } => {
            return resolved_origin_redirect(state, manifest_url).await;
        }
        ProviderError::Invalid(reason) => {
            return (StatusCode::BAD_REQUEST, reason.clone()).into_response();
        }
        ProviderError::CapacityExhausted => StatusCode::SERVICE_UNAVAILABLE,
        ProviderError::ProtectedContent
        | ProviderError::InvalidProtection
        | ProviderError::Unresolvable(_)
        | ProviderError::UnresolvableNetwork(_) => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::NOT_FOUND,
    };
    start_failure_response(status, &error)
}

pub(super) fn start_failure_response(status: StatusCode, error: &ProviderError) -> Response {
    let mut response = (
        status,
        Json(json!({
            "error": crate::start_status::StartError::from_provider(error).to_json()
        })),
    )
        .into_response();
    if matches!(error, ProviderError::CapacityExhausted) {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
    }
    response
}

/// `GET /streams/{network}/{id}` or `{id}.ts` (continuous MPEG-TS), or `.m3u8` (live HLS
/// playlist).
pub(super) async fn stream_file(
    State(s): State<AppState>,
    Path((network, file)): Path<(String, String)>,
) -> Response {
    let timeout = s.manager.hls_startup_timeout();
    stream_file_with_hls_timeout(State(s), Path((network, file)), timeout).await
}

pub(super) async fn stream_file_with_hls_timeout(
    State(s): State<AppState>,
    Path((network, file)): Path<(String, String)>,
    timeout: Duration,
) -> Response {
    if let Some(id) = file.strip_suffix(".m3u8") {
        let id = match native_stream_id(&network, id) {
            Ok(id) => id,
            Err(reason) => return (StatusCode::BAD_REQUEST, reason).into_response(),
        };
        return hls_startup_response(timeout, async {
            match s.manager.get_or_start_hls_with_pin(&network, &id).await {
                Ok((pkg, _request_pin)) => {
                    let prefix = format!(
                        "/streams/{network}/{}/seg",
                        native_public_stream_id(&network, &id)
                    );
                    let playlist = await_hls_playlist(&pkg, timeout, &prefix, true).await;
                    if !s
                        .manager
                        .get_hls(&network, &id)
                        .await
                        .is_some_and(|current| Arc::ptr_eq(&current, &pkg))
                    {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    let playlist = match playlist {
                        Ok(playlist) => playlist,
                        Err(response) => {
                            let mut response = *response;
                            if let crate::hls::HlsStatus::Failed(failure) = pkg.status() {
                                let error =
                                    crate::start_status::StartError::from_normalization(failure);
                                *response.body_mut() =
                                    Body::from(json!({"error": error.to_json()}).to_string());
                                response.headers_mut().remove(header::CONTENT_LENGTH);
                                response.headers_mut().insert(
                                    header::CONTENT_TYPE,
                                    "application/json".parse().unwrap(),
                                );
                            }
                            return response;
                        }
                    };
                    // Deliberately no `icy-*` header here: on an HLS manifest it makes VLC's HTTP
                    // access promote the URL to the `icyx://` (Icecast) scheme and treat the body as
                    // a raw audio stream, so the HLS demux never loads and playback busy-loops the
                    // playlist without fetching segments. The title reaches clients via the MPEG-TS
                    // SDT `service_name` embedded in the segments themselves.
                    (
                        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
                        playlist,
                    )
                        .into_response()
                }
                Err(e) => stream_start_error(&s, e).await,
            }
        })
        .await;
    }
    let id = if let Some(id) = file.strip_suffix(".ts") {
        if id.is_empty() {
            return StatusCode::NOT_FOUND.into_response();
        }
        id
    } else if file.contains('.') {
        return StatusCode::NOT_FOUND.into_response();
    } else {
        &file
    };
    let id = match native_stream_id(&network, id) {
        Ok(id) => id,
        Err(reason) => return (StatusCode::BAD_REQUEST, reason).into_response(),
    };
    let session = match s.manager.get_or_start(&network, &id).await {
        Ok(sess) => sess,
        Err(e) => return stream_start_error(&s, e).await,
    };
    stream_session_response(session)
}

fn stream_session_response(session: Arc<StreamSession>) -> Response {
    let sub = session.subscribe();
    // Per-client keyframe gate: holds a mid-GOP joiner until the first clean keyframe (with
    // PAT/PMT — and, when the stream has a resolved title, a synthesized SDT — prepended).
    let gate = ace_media::mpegts::KeyframeGate::with_service_name(session.metadata().title.clone());
    // Bridge the broadcast receiver to an HTTP body stream; the Subscription rides along so
    // its Drop (decrementing the client count) fires when the client disconnects.
    let stream = futures::stream::unfold((sub, gate), |(mut sub, mut gate)| async move {
        loop {
            match sub.rx.recv().await {
                Ok(StreamEvent::Data(chunk) | StreamEvent::StartupData(chunk)) => {
                    let out = gate.push(&chunk);
                    if out.is_empty() {
                        continue; // still waiting for the first keyframe
                    }
                    return Some((Ok::<_, std::io::Error>(Bytes::from(out)), (sub, gate)));
                }
                Ok(StreamEvent::Discontinuity) => {
                    reset_stream_keyframe_gate(&mut gate);
                    continue;
                }
                Err(RecvError::Lagged(_)) => {
                    reset_stream_keyframe_gate(&mut gate);
                    continue;
                }
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp2t")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub(super) fn ace_stream_session_response(
    session: Arc<StreamSession>,
    expires_at: Instant,
    cancel: tokio::sync::watch::Receiver<bool>,
    direct_lease: Option<AceDirectLeaseGuard>,
) -> Response {
    let sub = session.subscribe();
    let gate = ace_media::mpegts::KeyframeGate::with_service_name(session.metadata().title.clone());
    let stream = futures::stream::unfold(
        (sub, gate, cancel, expires_at, direct_lease),
        |(mut sub, mut gate, mut cancel, expires_at, direct_lease)| async move {
            loop {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(expires_at)) => {
                        return None;
                    }
                    changed = cancel.changed() => {
                        if changed.is_err() || *cancel.borrow() { return None; }
                    }
                    received = sub.rx.recv() => match received {
                        Ok(StreamEvent::Data(chunk) | StreamEvent::StartupData(chunk)) => {
                            let out = gate.push(&chunk);
                            if out.is_empty() { continue; }
                            return Some((Ok::<_, std::io::Error>(Bytes::from(out)), (sub, gate, cancel, expires_at, direct_lease)));
                        }
                        Ok(StreamEvent::Discontinuity) => {
                            reset_stream_keyframe_gate(&mut gate);
                        }
                        Err(RecvError::Lagged(_)) => { reset_stream_keyframe_gate(&mut gate); }
                        Err(RecvError::Closed) => return None,
                    }
                }
            }
        },
    );
    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp2t")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub(super) fn reset_stream_keyframe_gate(gate: &mut ace_media::mpegts::KeyframeGate) {
    gate.reset_for_discontinuity();
}

// Compatibility retains its existing numeric keys; native clients can distinguish an
// unfinished bounded measurement from a completed empty buffer.
fn native_live_position_json(
    position: Option<&crate::provider::LivePositionStats>,
) -> serde_json::Value {
    let mut value = live_position_json(position);
    if let (Some(position), Some(object)) = (position, value.as_object_mut()) {
        object.insert("buffer_complete".into(), json!(position.buffer_complete));
    }
    value
}

#[cfg(test)]
mod buffer_contract_tests {
    use super::*;

    #[test]
    fn bounded_measurement_freshness_is_native_only() {
        for complete in [false, true] {
            let position = crate::provider::LivePositionStats {
                buffer_complete: complete,
                ..Default::default()
            };
            assert_eq!(
                native_live_position_json(Some(&position))["buffer_complete"],
                complete
            );
            assert!(live_position_json(Some(&position))
                .get("buffer_complete")
                .is_none());
        }
        assert!(native_live_position_json(None).is_null());
    }
}

#[cfg(test)]
mod start_contract_tests {
    use super::*;
    use crate::provider::{ProviderRegistry, StreamProvider, TsSource};
    use async_trait::async_trait;
    use tokio::sync::Notify;
    use tower::ServiceExt;

    const SECRET: &str = "https://private.invalid/path?token=never-publish";
    #[derive(Default)]
    struct Gates {
        finite_first: std::sync::atomic::AtomicBool,
        entered: Notify,
        resolve: Notify,
        connected: Notify,
        fail: Notify,
    }
    struct ProgressProvider(Arc<Gates>);
    #[async_trait]
    impl StreamProvider for ProgressProvider {
        fn network(&self) -> &'static str {
            "synthetic"
        }
        async fn open(&self, _: &str) -> Result<Box<dyn TsSource>, ProviderError> {
            panic!("manager must use the progress-aware opening path")
        }
        async fn open_with_progress(
            &self,
            id: &str,
            progress: crate::start_status::StartProgress,
        ) -> Result<Box<dyn TsSource>, ProviderError> {
            if self
                .0
                .finite_first
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return crate::testprovider::TestProvider { chunks: 1 }
                    .open(id)
                    .await;
            }
            self.0.entered.notify_one();
            self.0.resolve.notified().await;
            progress.resolved();
            self.0.connected.notify_one();
            self.0.fail.notified().await;
            Err(ProviderError::Network(SECRET.into()))
        }
    }
    fn app() -> (axum::Router, Arc<StreamManager>, Arc<Gates>) {
        let gates = Arc::new(Gates::default());
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(ProgressProvider(gates.clone())));
        let manager = StreamManager::new(registry);
        let app = crate::http::router(AppState {
            manager: manager.clone(),
            networks: vec!["synthetic".into()],
            resolve_content_ids_in_getstream: false,
            ace_sessions: Arc::new(crate::http::AceSessionStore::default()),
            experimental_ace_compat: false,
            origin_hls: None,
            broadcasts: None,
        });
        (app, manager, gates)
    }
    async fn signal(notify: &Notify) {
        tokio::time::timeout(Duration::from_secs(1), notify.notified())
            .await
            .unwrap();
    }
    async fn get(app: &axum::Router, path: &str) -> Response {
        app.clone()
            .oneshot(axum::http::Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }
    async fn json_body(response: Response) -> serde_json::Value {
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains(SECRET));
        serde_json::from_slice(&body).unwrap()
    }
    #[tokio::test]
    async fn native_start_phases_and_failures_are_observable_without_a_session() {
        let (app, _, gates) = app();
        let client = app.clone();
        let opening =
            tokio::spawn(async move { get(&client, "/streams/synthetic/opening.ts").await });
        signal(&gates.entered).await;
        let status = get(&app, "/streams/synthetic/opening/status").await;
        assert_eq!(status.status(), StatusCode::OK);
        assert_eq!(json_body(status).await["phase"], "resolving");
        gates.resolve.notify_one();
        signal(&gates.connected).await;
        assert_eq!(
            json_body(get(&app, "/streams/synthetic/opening/status").await).await["phase"],
            "connecting"
        );
        gates.fail.notify_one();
        let response = opening.await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "preserve native Backend status"
        );
        assert_eq!(json_body(response).await["error"]["code"], "network");
        let status = json_body(get(&app, "/streams/synthetic/opening/status").await).await;
        assert_eq!(status["phase"], "failed");
        assert_eq!(status["error"]["code"], "network");
    }
    #[tokio::test]
    async fn reopening_finished_source_reports_new_progress_and_failure() {
        let (app, manager, gates) = app();
        gates
            .finite_first
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let previous = manager
            .get_or_start("synthetic", "reopening")
            .await
            .unwrap();
        let mut viewer = previous.subscribe();
        tokio::time::timeout(Duration::from_secs(1), async {
            while viewer.rx.recv().await.is_ok() {}
            while !previous.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(previous.produced_media());
        assert_eq!(
            json_body(get(&app, "/streams/synthetic/reopening/status").await).await["phase"],
            "playing"
        );

        let client = app.clone();
        let opening =
            tokio::spawn(async move { get(&client, "/streams/synthetic/reopening.ts").await });
        signal(&gates.entered).await;
        assert_eq!(
            json_body(get(&app, "/streams/synthetic/reopening/status").await).await["phase"],
            "resolving"
        );
        gates.resolve.notify_one();
        signal(&gates.connected).await;
        assert_eq!(
            json_body(get(&app, "/streams/synthetic/reopening/status").await).await["phase"],
            "connecting"
        );
        gates.fail.notify_one();
        assert_eq!(opening.await.unwrap().status(), StatusCode::NOT_FOUND);
        assert!(Arc::ptr_eq(
            &previous,
            &manager.get("synthetic", "reopening").await.unwrap()
        ));
        let status = json_body(get(&app, "/streams/synthetic/reopening/status").await).await;
        assert_eq!(status["phase"], "failed");
        assert_eq!(status["error"]["code"], "network");
    }

    #[tokio::test]
    async fn stop_fences_late_provider_failure_and_progress() {
        let (app, manager, gates) = app();
        let client = app.clone();
        let opening =
            tokio::spawn(async move { get(&client, "/streams/synthetic/stopped.ts").await });
        signal(&gates.entered).await;
        manager.stop("synthetic", "stopped").await;
        gates.resolve.notify_one();
        signal(&gates.connected).await;
        gates.fail.notify_one();
        assert_eq!(opening.await.unwrap().status(), StatusCode::NOT_FOUND);
        assert_eq!(
            get(&app, "/streams/synthetic/stopped/status")
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
}
