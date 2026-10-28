use super::*;

/// `GET /vod/{network}/{id}` — resolve `id` as a single-file VOD, then stream its verified
/// bytes with a `Content-Length`. The resolved VOD is cached by the manager and shared with the
/// HLS routes, so a playback resolves the descriptor once and reuses downloaded pieces.
pub(super) async fn vod_stream(
    State(s): State<AppState>,
    Path((network, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let vod = match resolve_vod(&s, &network, &id).await {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let total = vod.content_length();
    if total == 0 {
        return empty_vod_response();
    }
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    match parse_byte_range(range, total) {
        // Whole file (no usable Range): 200 with Accept-Ranges so players know they *can* seek.
        RangeOutcome::Full => match s.manager.open_vod_range(vod.as_ref(), 0, total - 1).await {
            Ok(source) => vod_response_from_source(source),
            Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:?}")).into_response(),
        },
        RangeOutcome::Satisfiable(start, end) => {
            match s.manager.open_vod_range(vod.as_ref(), start, end).await {
                Ok(source) => vod_partial_response(source, start, end, total),
                Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:?}")).into_response(),
            }
        }
        // A well-formed range that can't be met: 416 with the resource's true length.
        RangeOutcome::Unsatisfiable => axum::http::Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_RANGE, format!("bytes */{total}"))
            .body(Body::empty())
            .expect("valid 416 response")
            .into_response(),
    }
}

/// How a `Range` request header resolves against a known content length.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RangeOutcome {
    /// No usable single-range spec — serve the whole file (`200`).
    Full,
    /// One satisfiable byte range `[start, end]` (inclusive) — serve `206`.
    Satisfiable(u64, u64),
    /// A well-formed range that lies outside the content — serve `416`.
    Unsatisfiable,
}

/// Interpret a `Range` header against `total` (> 0) content bytes. Only single byte-ranges are
/// honored; anything else (missing header, non-`bytes` unit, multi-range, or unparseable spec)
/// is ignored and treated as [`RangeOutcome::Full`] — RFC 7233 §3.1 lets a server ignore a
/// `Range` it doesn't act on. Only a syntactically valid but out-of-bounds range is
/// [`RangeOutcome::Unsatisfiable`].
pub(super) fn parse_byte_range(header: Option<&str>, total: u64) -> RangeOutcome {
    let Some(spec) = header.and_then(|h| h.strip_prefix("bytes=")) else {
        return RangeOutcome::Full;
    };
    let spec = spec.trim();
    // Single range only; a comma means multiple ranges, which we decline to honor (serve 200).
    if spec.contains(',') {
        return RangeOutcome::Full;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeOutcome::Full;
    };
    let (first, last) = (first.trim(), last.trim());
    match (first.is_empty(), last.is_empty()) {
        // `-N`: the last N bytes (clamped to the whole file).
        (true, false) => match last.parse::<u64>() {
            Ok(0) => RangeOutcome::Unsatisfiable,
            Ok(n) => RangeOutcome::Satisfiable(total.saturating_sub(n), total - 1),
            Err(_) => RangeOutcome::Full,
        },
        // `N-`: from byte N to the end.
        (false, true) => match first.parse::<u64>() {
            Ok(start) if start < total => RangeOutcome::Satisfiable(start, total - 1),
            Ok(_) => RangeOutcome::Unsatisfiable,
            Err(_) => RangeOutcome::Full,
        },
        // `N-M`: an explicit range (end clamped to the last byte).
        (false, false) => match (first.parse::<u64>(), last.parse::<u64>()) {
            (Ok(start), Ok(end)) if start <= end && start < total => {
                RangeOutcome::Satisfiable(start, end.min(total - 1))
            }
            (Ok(_), Ok(_)) => RangeOutcome::Unsatisfiable,
            _ => RangeOutcome::Full,
        },
        // A bare `-`: not a valid range, ignore it.
        (true, true) => RangeOutcome::Full,
    }
}

/// Build an empty `200` for a zero-length VOD (no range possible), advertising `Accept-Ranges`.
fn empty_vod_response() -> Response {
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, 0)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::empty())
        .expect("valid empty vod response")
        .into_response()
}

/// Bridge a [`VodByteSource`] to an HTTP body stream.
fn vod_body(source: Box<dyn VodByteSource>) -> Body {
    let stream = futures::stream::unfold(source, |mut src| async move {
        src.next()
            .await
            .map(|chunk| (Ok::<Bytes, std::io::Error>(chunk), src))
    });
    Body::from_stream(stream)
}

/// Build a streaming `200` response for a whole-file [`VodByteSource`], advertising its length
/// and that the resource is range-seekable.
pub(super) fn vod_response_from_source(source: Box<dyn VodByteSource>) -> Response {
    let total = source.content_length();
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, total)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(vod_body(source))
        .expect("valid vod response")
        .into_response()
}

/// Build a streaming `206 Partial Content` response for the inclusive byte range `[start, end]`
/// of a `total`-byte VOD.
fn vod_partial_response(
    source: Box<dyn VodByteSource>,
    start: u64,
    end: u64,
    total: u64,
) -> Response {
    axum::http::Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, end - start + 1)
        .header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}"),
        )
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(vod_body(source))
        .expect("valid vod partial response")
        .into_response()
}

/// Resolve `id` on `network` to a single-file VOD via the manager's shared cache, or the HTTP
/// error response to return: `404` for an unknown network, `502` if resolution fails.
pub(super) async fn resolve_vod(
    s: &AppState,
    network: &str,
    id: &str,
) -> Result<Arc<dyn VodContent>, Box<Response>> {
    match s.manager.resolve_vod(network, id).await {
        Ok(v) => Ok(v),
        Err(ProviderError::NotFound) => Err(Box::new(
            (StatusCode::NOT_FOUND, "unknown network").into_response(),
        )),
        Err(e) => Err(Box::new(
            (StatusCode::BAD_GATEWAY, format!("{e:?}")).into_response(),
        )),
    }
}

/// `GET /vod/{network}/{id}/manifest.m3u8` — a static VOD HLS media playlist for the resolved
/// single-file VOD. The whole file's length is known, so the playlist lists every byte-range
/// segment up front and ends with `#EXT-X-ENDLIST`; segments are fetched lazily via
/// [`vod_hls_segment`]. Segment geometry comes from the shared HLS config (same knobs as live).
pub(super) async fn vod_manifest(
    State(s): State<AppState>,
    Path((network, id)): Path<(String, String)>,
) -> Response {
    let vod = match resolve_vod(&s, &network, &id).await {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let layout = match crate::hls::VodHlsLayout::new(vod.content_length(), s.manager.hls_config()) {
        Ok(layout) => layout,
        Err(message) => return (StatusCode::UNSUPPORTED_MEDIA_TYPE, message).into_response(),
    };
    // Inspect a bounded, verified prefix before advertising any .ts URLs. A file name or
    // packet-aligned length does not identify its container. Drop the probe source here.
    let probe_length = (TS_PROBE_PACKETS * TS_PACKET_BYTES) as u64;
    if let Err(response) = open_ts_vod_range(&s, vod.as_ref(), 0, probe_length - 1).await {
        return *response;
    }
    (
        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
        layout.playlist(&network, &id),
    )
        .into_response()
}

/// `GET /vod/{network}/{id}/seg/{n}.ts` — HLS segment `n` of the resolved VOD, served as the
/// verified byte range the [`VodHlsLayout`](crate::hls::VodHlsLayout) assigns to it. A segment
/// index past the last segment (or a malformed name) 404s.
pub(super) async fn vod_hls_segment(
    State(s): State<AppState>,
    Path((network, id, seg)): Path<(String, String, String)>,
) -> Response {
    let Some(index) = seg.strip_suffix(".ts").and_then(|n| n.parse::<u64>().ok()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let vod = match resolve_vod(&s, &network, &id).await {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let layout = match crate::hls::VodHlsLayout::new(vod.content_length(), s.manager.hls_config()) {
        Ok(layout) => layout,
        Err(message) => return (StatusCode::UNSUPPORTED_MEDIA_TYPE, message).into_response(),
    };
    let Some((start, end)) = layout.segment_range(index) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match open_ts_vod_range(&s, vod.as_ref(), start, end).await {
        Ok(stream) => vod_hls_segment_response(stream),
        Err(response) => *response,
    }
}

/// Opening a range and probing its format share one deadline. Cancellation drops both the
/// manager's operation guard and any already-opened range reader.
async fn open_ts_vod_range(
    s: &AppState,
    vod: &dyn VodContent,
    start: u64,
    end: u64,
) -> Result<TsVodStream, Box<Response>> {
    let timeout = s.manager.hls_startup_timeout();
    tokio::time::timeout(timeout, async {
        let source = s
            .manager
            .open_vod_range(vod, start, end)
            .await
            .map_err(|error| {
                Box::new((StatusCode::BAD_GATEWAY, error.to_string()).into_response())
            })?;
        probe_ts_source(source, end - start + 1, timeout).await
    })
    .await
    .unwrap_or_else(|_| {
        Err(Box::new(
            (StatusCode::GATEWAY_TIMEOUT, "VOD container probe timed out").into_response(),
        ))
    })
}

const TS_PACKET_BYTES: usize = 188;
const TS_PROBE_PACKETS: usize = 3;

/// Container framing only: this does not certify PAT/PMT, timestamps, codecs or GOP boundaries.
fn valid_ts_packet(packet: &[u8]) -> bool {
    if packet.len() != TS_PACKET_BYTES
        || packet[0] != 0x47
        || packet[1] & 0x80 != 0
        || packet[3] & 0xc0 != 0
    {
        return false;
    }
    match (packet[3] >> 4) & 3 {
        1 => true,
        2 => packet[4] == 183,
        3 => packet[4] <= 182,
        _ => false,
    }
}

struct TsVodStream {
    source: Box<dyn VodByteSource>,
    prefix: std::collections::VecDeque<Bytes>,
    /// Only a packet spanning source chunks is copied. Whole chunks stay zero-copy.
    partial_packet: Vec<u8>,
    remaining: u64,
    timeout: Duration,
    failed: bool,
}

impl TsVodStream {
    fn validate(&mut self, mut chunk: &[u8]) -> bool {
        if !self.partial_packet.is_empty() {
            let take = (TS_PACKET_BYTES - self.partial_packet.len()).min(chunk.len());
            self.partial_packet.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if self.partial_packet.len() < TS_PACKET_BYTES {
                return true;
            }
            if !valid_ts_packet(&self.partial_packet) {
                return false;
            }
            self.partial_packet.clear();
        }
        let (packets, remainder) = chunk.as_chunks::<TS_PACKET_BYTES>();
        if !packets.iter().all(|packet| valid_ts_packet(packet)) {
            return false;
        }
        self.partial_packet.extend_from_slice(remainder);
        true
    }
}

/// Inspect at most three packets of framing, retaining the original source chunks for the body.
/// This bounded probe never downloads the complete VOD or starts a live session.
async fn probe_ts_source(
    mut source: Box<dyn VodByteSource>,
    expected_length: u64,
    timeout: Duration,
) -> Result<TsVodStream, Box<Response>> {
    if source.content_length() != expected_length {
        return Err(Box::new(
            (StatusCode::BAD_GATEWAY, "VOD range length mismatch").into_response(),
        ));
    }
    let wanted = expected_length.min((TS_PROBE_PACKETS * TS_PACKET_BYTES) as u64) as usize;
    let mut probe = Vec::with_capacity(wanted);
    let mut prefix = std::collections::VecDeque::new();
    let mut received = 0u64;
    let read = tokio::time::timeout(timeout, async {
        let mut empty_chunks = 0;
        while probe.len() < wanted {
            let Some(chunk) = source.next().await else {
                return false;
            };
            if chunk.is_empty() {
                empty_chunks += 1;
                if empty_chunks > 16 {
                    return false;
                }
                continue;
            }
            let Some(next_received) = received.checked_add(chunk.len() as u64) else {
                return false;
            };
            if next_received > expected_length {
                return false;
            }
            received = next_received;
            probe.extend_from_slice(&chunk[..chunk.len().min(wanted - probe.len())]);
            prefix.push_back(chunk);
        }
        true
    })
    .await;
    match read {
        Err(_) => {
            return Err(Box::new(
                (StatusCode::GATEWAY_TIMEOUT, "VOD container probe timed out").into_response(),
            ))
        }
        Ok(false) => {
            return Err(Box::new(
                (
                    StatusCode::BAD_GATEWAY,
                    "VOD range ended before its declared length",
                )
                    .into_response(),
            ))
        }
        Ok(true) => {}
    }
    if wanted == 0
        || !probe.len().is_multiple_of(TS_PACKET_BYTES)
        || !probe
            .as_chunks::<TS_PACKET_BYTES>()
            .0
            .iter()
            .all(|packet| valid_ts_packet(packet))
    {
        return Err(Box::new((StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "VOD HLS requires unencrypted 188-byte MPEG-TS; use the raw VOD URL for other containers").into_response()));
    }
    Ok(TsVodStream {
        source,
        prefix,
        partial_packet: Vec::with_capacity(TS_PACKET_BYTES),
        remaining: expected_length,
        timeout,
        failed: false,
    })
}

/// Probe framing before headers; validate every remaining packet during streaming. A late
/// truncated or corrupt range fails the HTTP body rather than silently publishing valid TS.
fn vod_hls_segment_response(stream: TsVodStream) -> Response {
    let expected_length = stream.remaining;
    let body = futures::stream::unfold(stream, |mut stream| async move {
        if stream.failed || stream.remaining == 0 {
            return None;
        }
        let chunk = if let Some(chunk) = stream.prefix.pop_front() {
            Ok(Some(chunk))
        } else {
            tokio::time::timeout(stream.timeout, stream.source.next()).await
        };
        let result = match chunk {
            Ok(Some(chunk))
                if !chunk.is_empty()
                    && chunk.len() as u64 <= stream.remaining
                    && stream.validate(&chunk) =>
            {
                stream.remaining -= chunk.len() as u64;
                Ok(chunk)
            }
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "VOD segment timed out",
            )),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid or truncated VOD MPEG-TS range",
            )),
        };
        stream.failed = result.is_err();
        Some((result, stream))
    });
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp2t")
        .header(header::CONTENT_LENGTH, expected_length)
        .body(Body::from_stream(body))
        .expect("valid vod segment response")
        .into_response()
}

#[cfg(test)]
mod qualification_tests {
    use super::*;
    use crate::provider::ProviderRegistry;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Source {
        chunks: std::collections::VecDeque<Bytes>,
        declared: u64,
        stall: bool,
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for Source {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[async_trait::async_trait]
    impl VodByteSource for Source {
        fn content_length(&self) -> u64 {
            self.declared
        }
        async fn next(&mut self) -> Option<Bytes> {
            if self.stall && self.chunks.is_empty() {
                std::future::pending::<()>().await;
            }
            self.chunks.pop_front()
        }
    }

    fn packets(count: usize) -> Vec<u8> {
        (0..count)
            .flat_map(|_| {
                let mut packet = [0x55; TS_PACKET_BYTES];
                packet[..4].copy_from_slice(&[0x47, 0x01, 0x00, 0x10]);
                packet
            })
            .collect()
    }

    fn source(chunks: Vec<Bytes>, declared: u64) -> (Box<dyn VodByteSource>, Arc<AtomicUsize>) {
        let dropped = Arc::new(AtomicUsize::new(0));
        (
            Box::new(Source {
                chunks: chunks.into(),
                declared,
                stall: false,
                dropped: dropped.clone(),
            }),
            dropped,
        )
    }

    async fn qualified_response(
        source: Box<dyn VodByteSource>,
        length: u64,
        timeout: Duration,
    ) -> Response {
        match probe_ts_source(source, length, timeout).await {
            Ok(stream) => vod_hls_segment_response(stream),
            Err(response) => *response,
        }
    }

    #[tokio::test]
    async fn vod_ts_segment_preserves_bytes_across_every_small_chunk_alignment() {
        let data = packets(8);
        for size in [1, 17, 187, 188, 189, 563, 564, 1504] {
            let chunks = data.chunks(size).map(Bytes::copy_from_slice).collect();
            let (source, dropped) = source(chunks, data.len() as u64);
            let response =
                qualified_response(source, data.len() as u64, Duration::from_secs(1)).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            assert_eq!(body.as_ref(), data, "chunk size {size}");
            assert_eq!(dropped.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test]
    async fn vod_ts_segment_fails_late_corruption_and_short_ranges() {
        let data = packets(8);
        let mut corrupt_tail = data[564..].to_vec();
        corrupt_tail[188] = 0;
        for tail in [corrupt_tail, data[564..1000].to_vec()] {
            let (source, dropped) = source(
                vec![Bytes::copy_from_slice(&data[..564]), Bytes::from(tail)],
                data.len() as u64,
            );
            let response =
                qualified_response(source, data.len() as u64, Duration::from_secs(1)).await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "prefix qualifies before the later source failure"
            );
            assert!(axum::body::to_bytes(response.into_body(), 4096)
                .await
                .is_err());
            assert_eq!(dropped.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test]
    async fn vod_ts_probe_rejects_short_oversized_and_misdeclared_ranges_and_drops_reader() {
        for (data, declared) in [(packets(2), 564), (packets(4), 564), (packets(3), 565)] {
            let (source, dropped) = source(vec![Bytes::from(data)], declared);
            let response = qualified_response(source, 564, Duration::from_secs(1)).await;
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            assert_eq!(dropped.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn vod_ts_probe_timeout_releases_the_reader() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let source = Box::new(Source {
            chunks: Default::default(),
            declared: 564,
            stall: true,
            dropped: dropped.clone(),
        });
        let response = qualified_response(source, 564, Duration::from_secs(1)).await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn vod_ts_probe_is_cancel_safe() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let source = Box::new(Source {
            chunks: Default::default(),
            declared: 564,
            stall: true,
            dropped: dropped.clone(),
        });
        let task = tokio::spawn(qualified_response(source, 564, Duration::from_secs(30)));
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }
    #[tokio::test(start_paused = true)]
    async fn vod_ts_body_timeout_releases_the_reader() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let source = Box::new(Source {
            chunks: [Bytes::from(packets(3))].into(),
            declared: 1504,
            stall: true,
            dropped: dropped.clone(),
        });
        let response = qualified_response(source, 1504, Duration::from_secs(1)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(axum::body::to_bytes(response.into_body(), 4096)
            .await
            .is_err());
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn vod_ts_range_open_and_probe_share_the_startup_deadline() {
        struct PendingContent(Arc<AtomicUsize>);
        struct OpenGuard(Arc<AtomicUsize>);
        impl Drop for OpenGuard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        #[async_trait::async_trait]
        impl VodContent for PendingContent {
            fn content_length(&self) -> u64 {
                564
            }
            async fn open_range(
                &self,
                _: u64,
                _: u64,
            ) -> Result<Box<dyn VodByteSource>, ProviderError> {
                let _guard = OpenGuard(self.0.clone());
                std::future::pending().await
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let state = AppState {
            manager: StreamManager::with_config(
                ProviderRegistry::new(),
                256,
                crate::config::HlsConfig {
                    startup_timeout_ms: 100,
                    ..crate::config::HlsConfig::default()
                },
            ),
            networks: vec![],
            resolve_content_ids_in_getstream: false,
            ace_sessions: Arc::new(AceSessionStore::default()),
            experimental_ace_compat: false,
            broadcasts: None,
        };
        let started = tokio::time::Instant::now();
        let result = open_ts_vod_range(&state, &PendingContent(dropped.clone()), 0, 563).await;
        let Err(response) = result else {
            panic!("pending range must time out")
        };
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(started.elapsed(), Duration::from_millis(100));
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }
}
