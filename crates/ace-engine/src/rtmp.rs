//! RTMP broadcast ingress. Dependency-owned sockets may outlive listener shutdown;
//! publication authority is fenced by publishing session and the broadcast generation.

use crate::broadcast::{cancelled, ProducerKind, ProducerSession};
use crate::broadcast_ingest::BroadcastIngest;
use crate::http::BroadcastState;
use crate::rtmp_ts::RtmpTsMuxer;
use rtmp_rs::media::{AacData, H264Data};
use rtmp_rs::protocol::message::{ConnectParams, PublishParams};
use rtmp_rs::server::handler::{AuthResult, MediaDeliveryMode, RtmpHandler};
use rtmp_rs::session::{SessionContext, StreamContext};
use rtmp_rs::{RtmpServer, ServerConfig};
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::{watch, Mutex as AsyncMutex};

pub async fn serve_rtmp(
    bind: SocketAddr,
    broadcasts: BroadcastState,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = ServerConfig::with_addr(bind).disable_gop_buffer();
    let server = RtmpServer::new(config, RtmpIngestHandler::new(broadcasts));
    server
        .run_until(async move { cancelled(&mut cancel).await })
        .await?;
    Ok(())
}

pub struct RtmpIngestHandler {
    broadcasts: BroadcastState,
    streams: Mutex<BTreeMap<String, Arc<RtmpStreamIngest>>>,
    // A canceled publishing connection cannot acquire a later generation for that name:
    // callbacks carry connection/stream IDs, not an independently echoable generation token.
    // Ordinary unpublish is reusable. Tombstones last only until connection disconnect.
    retired: Mutex<BTreeSet<(u64, String)>>,
    #[cfg(test)]
    operation_hook: Mutex<Option<OperationHook>>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RtmpOperation {
    Mux,
    Ingest,
    Finish,
}
#[cfg(test)]
type OperationHook = Arc<
    dyn Fn(RtmpOperation) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

struct RtmpStreamIngest {
    session_id: u64,
    stream_id: u32,
    generation: u64,
    cancel: watch::Receiver<bool>,
    generation_cancel: watch::Receiver<bool>,
    operation: Arc<AsyncMutex<()>>,
    data: AsyncMutex<Option<RtmpProducer>>,
}
struct RtmpProducer {
    session: ProducerSession,
    ingest: BroadcastIngest,
    muxer: RtmpTsMuxer,
}

impl RtmpIngestHandler {
    pub fn new(broadcasts: BroadcastState) -> Self {
        Self {
            broadcasts,
            streams: Mutex::new(BTreeMap::new()),
            retired: Mutex::new(BTreeSet::new()),
            #[cfg(test)]
            operation_hook: Mutex::new(None),
        }
    }
    #[cfg(test)]
    pub(crate) fn set_operation_hook(&self, hook: OperationHook) {
        *self.operation_hook.lock().unwrap() = Some(hook);
    }
    #[cfg(test)]
    async fn before_operation(&self, operation: RtmpOperation) {
        let hook = self.operation_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(operation).await;
        }
    }
    fn find(&self, ctx: &StreamContext) -> Option<Arc<RtmpStreamIngest>> {
        self.streams
            .lock()
            .unwrap()
            .get(&ctx.stream_key)
            .filter(|stream| {
                stream.session_id == ctx.session.session_id && stream.stream_id == ctx.stream_id
            })
            .cloned()
    }
    fn retire(&self, name: &str, stream: &RtmpStreamIngest) {
        self.retired
            .lock()
            .unwrap()
            .insert((stream.session_id, name.to_owned()));
    }
    fn remove_current(&self, name: &str, stream: &Arc<RtmpStreamIngest>) {
        let mut map = self.streams.lock().unwrap();
        if map
            .get(name)
            .is_some_and(|current| Arc::ptr_eq(current, stream))
        {
            map.remove(name);
        }
    }
    async fn push_media<F>(&self, ctx: &StreamContext, mux: F)
    where
        F: FnOnce(&mut RtmpTsMuxer) -> Vec<u8>,
    {
        let Some(stream) = self.find(ctx) else {
            return;
        };
        let _operation = stream.operation.lock().await;
        let mut data = stream.data.lock().await;
        let Some(producer) = data.as_mut() else {
            return;
        };
        if producer.session.generation() != stream.generation || producer.session.check().is_err() {
            self.retire(&ctx.stream_key, &stream);
            data.take();
            self.remove_current(&ctx.stream_key, &stream);
            return;
        }
        #[cfg(test)]
        self.before_operation(RtmpOperation::Mux).await;
        if producer.session.check().is_err() {
            return;
        }
        let bytes = mux(&mut producer.muxer);
        if bytes.is_empty() {
            return;
        }
        #[cfg(test)]
        self.before_operation(RtmpOperation::Ingest).await;
        if let Err(error) = producer.ingest.push_bytes(&bytes).await {
            crate::alog!("[rtmp] {}: {}", ctx.stream_key, error.code());
            self.retire(&ctx.stream_key, &stream);
            data.take();
            self.remove_current(&ctx.stream_key, &stream);
        }
    }
}

impl RtmpHandler for RtmpIngestHandler {
    async fn on_connect(&self, _ctx: &SessionContext, params: &ConnectParams) -> AuthResult {
        if params.app == "live" {
            AuthResult::Accept
        } else {
            AuthResult::Reject("unsupported app; use live".to_string())
        }
    }

    async fn on_publish(&self, ctx: &SessionContext, params: &PublishParams) -> AuthResult {
        let name = &params.stream_key;
        if !crate::broadcast::valid_broadcast_name(name) {
            return AuthResult::Reject("invalid_name".into());
        }
        if self.broadcasts.registry.is_service_stopping() {
            return AuthResult::Reject("broadcast_service_stopping".into());
        }
        if self
            .retired
            .lock()
            .unwrap()
            .contains(&(ctx.session_id, name.clone()))
        {
            return AuthResult::Reject("broadcast_stopping".into());
        }
        {
            let map = self.streams.lock().unwrap();
            if let Some(old) = map.get(name).filter(|old| *old.cancel.borrow()) {
                self.retire(name, old);
                if old.session_id == ctx.session_id {
                    return AuthResult::Reject("broadcast_stopping".into());
                }
            }
        }
        let (_, mut session) = match self
            .broadcasts
            .acquire_producer(name, ProducerKind::Rtmp)
            .await
        {
            Ok(pair) => pair,
            Err(error) => return AuthResult::Reject(error.code().into()),
        };
        let ingest = match session.open_ingest() {
            Ok(ingest) => ingest,
            Err(error) => return AuthResult::Reject(error.code().into()),
        };
        let stream = Arc::new(RtmpStreamIngest {
            session_id: ctx.session_id,
            stream_id: params.stream_id,
            generation: session.generation(),
            cancel: session.cancellation(),
            generation_cancel: session.generation_cancellation(),
            operation: session.operation_barrier(),
            data: AsyncMutex::new(Some(RtmpProducer {
                session,
                ingest,
                muxer: RtmpTsMuxer::new(),
            })),
        });
        let mut map = self.streams.lock().unwrap();
        if *stream.cancel.borrow() {
            return AuthResult::Reject("broadcast_stopping".into());
        }
        if let Some(old) = map.get(name) {
            if !*old.cancel.borrow() {
                return AuthResult::Reject("producer_conflict".into());
            }
            self.retire(name, old);
            if old.session_id == ctx.session_id {
                return AuthResult::Reject("broadcast_stopping".into());
            }
        }
        map.insert(name.clone(), stream);
        AuthResult::Accept
    }

    async fn on_video_frame(&self, ctx: &StreamContext, frame: &H264Data, timestamp: u32) {
        self.push_media(ctx, |muxer| muxer.push_video(frame, timestamp))
            .await;
    }
    async fn on_audio_frame(&self, ctx: &StreamContext, frame: &AacData, timestamp: u32) {
        self.push_media(ctx, |muxer| muxer.push_audio(frame, timestamp))
            .await;
    }
    async fn on_unpublish(&self, ctx: &StreamContext) {
        let Some(stream) = self.find(ctx) else {
            return;
        };
        let _operation = stream.operation.lock().await;
        let mut data = stream.data.lock().await;
        if let Some(mut producer) = data.take() {
            if producer.session.check().is_err() {
                self.retire(&ctx.stream_key, &stream);
            }
            #[cfg(test)]
            self.before_operation(RtmpOperation::Finish).await;
            if producer.session.generation() == stream.generation
                && producer.session.check().is_ok()
            {
                if let Err(error) = producer.ingest.finish().await {
                    crate::alog!("[rtmp] {}: {}", ctx.stream_key, error.code());
                    self.retire(&ctx.stream_key, &stream);
                }
            } else {
                self.retire(&ctx.stream_key, &stream);
            }
        }
        // Unlike producer cancellation on ordinary Drop, this signal identifies deletion
        // racing the final successful unpublish/lease release.
        if *stream.generation_cancel.borrow() {
            self.retire(&ctx.stream_key, &stream);
        }
        self.remove_current(&ctx.stream_key, &stream);
    }
    async fn on_disconnect(&self, ctx: &SessionContext) {
        let streams: Vec<_> = self
            .streams
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, stream)| stream.session_id == ctx.session_id)
            .map(|(name, stream)| (name.clone(), stream.clone()))
            .collect();
        for (name, stream) in streams {
            let _operation = stream.operation.lock().await;
            // A broken connection discards its partial attempt; only unpublish seals a
            // finite tail. The session drop cancels every outstanding authorized operation.
            stream.data.lock().await.take();
            self.remove_current(&name, &stream);
        }
        self.retired
            .lock()
            .unwrap()
            .retain(|(session_id, _)| *session_id != ctx.session_id);
    }
    fn media_delivery_mode(&self) -> MediaDeliveryMode {
        MediaDeliveryMode::ParsedFrames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::BroadcastState;
    use rtmp_rs::protocol::message::{ConnectParams, PublishParams};
    use rtmp_rs::server::handler::{AuthResult, RtmpHandler};
    use rtmp_rs::session::SessionContext;
    use std::process::Stdio;
    use tokio::process::Command;

    fn state() -> BroadcastState {
        BroadcastState {
            registry: crate::broadcast::BroadcastRegistry::new(),
            seed_registry: ace_swarm::listen::SeedRegistry::new(),
            trackers: vec![],
            store_bytes: 4 << 20,
            inbound_peer_port: tokio::sync::watch::channel(None).1,
        }
    }

    fn session() -> SessionContext {
        SessionContext::new(1, "127.0.0.1:40000".parse().unwrap())
    }

    #[tokio::test]
    async fn connect_accepts_only_live_app() {
        let handler = RtmpIngestHandler::new(state());
        assert!(matches!(
            handler
                .on_connect(
                    &session(),
                    &ConnectParams {
                        app: "live".to_string(),
                        ..Default::default()
                    },
                )
                .await,
            AuthResult::Accept
        ));
        assert!(matches!(
            handler
                .on_connect(
                    &session(),
                    &ConnectParams {
                        app: "vod".to_string(),
                        ..Default::default()
                    },
                )
                .await,
            AuthResult::Reject(_)
        ));
    }

    #[tokio::test]
    async fn publish_mints_stream_key_broadcast() {
        let bs = state();
        let registry = bs.registry.clone();
        let handler = RtmpIngestHandler::new(bs);
        let result = handler
            .on_publish(
                &session(),
                &PublishParams {
                    stream_key: "mychan".to_string(),
                    publish_type: "live".to_string(),
                    stream_id: 1,
                },
            )
            .await;

        assert!(matches!(result, AuthResult::Accept));
        assert!(
            registry.get("mychan").await.is_some(),
            "RTMP publish must mint/resume the broadcast named by stream key"
        );
    }

    async fn wait_for_tcp(addr: std::net::SocketAddr) {
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("RTMP server did not start on {addr}");
    }

    #[tokio::test]
    async fn rtmp_publish_reaches_broadcast_piece_store() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use tokio::io::AsyncReadExt;
        use tokio::time::timeout;

        let ffmpeg_available = timeout(
            Duration::from_secs(5),
            Command::new("ffmpeg")
                .arg("-version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await
        .expect("ffmpeg version probe timed out")
        .is_ok_and(|status| status.success());
        if !ffmpeg_available {
            eprintln!("skipping RTMP loopback smoke: target-executable ffmpeg is unavailable");
            return;
        }

        let bs = state();
        let registry = bs.registry.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        // A normal RTMP disconnect intentionally drops an incomplete signed piece. Fence
        // the graceful-unpublish flush too: this test must observe a full piece produced
        // by ordinary media ingestion, independent of FFmpeg's shutdown command ordering.
        let handler = RtmpIngestHandler::new(bs);
        let (finish_tx, finish_rx) = watch::channel(false);
        let operations = Arc::new(std::array::from_fn::<_, 3, _>(|_| AtomicUsize::new(0)));
        let hook_operations = operations.clone();
        handler.set_operation_hook(Arc::new(move |operation| {
            let mut finish_rx = finish_rx.clone();
            let index = match operation {
                RtmpOperation::Mux => 0,
                RtmpOperation::Ingest => 1,
                RtmpOperation::Finish => 2,
            };
            hook_operations[index].fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                if operation == RtmpOperation::Finish {
                    while !*finish_rx.borrow_and_update() {
                        if finish_rx.changed().await.is_err() {
                            break;
                        }
                    }
                }
            })
        }));
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let mut server = tokio::spawn(async move {
            RtmpServer::new(ServerConfig::with_addr(addr).disable_gop_buffer(), handler)
                .run_until(async move { cancelled(&mut cancel_rx).await })
                .await
        });
        wait_for_tcp(addr).await;

        let output = format!("rtmp://{addr}/live/loop");
        // HRD filler keeps even a tiny synthetic picture above the default 64 KiB
        // piece boundary. Bound encoder/filter threads for concurrent CI test loads.
        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-filter_threads",
                "1",
                "-filter_complex_threads",
                "1",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=128x96:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=1000:sample_rate=44100",
                "-t",
                "2",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-threads:v",
                "1",
                "-b:v",
                "1M",
                "-minrate",
                "1M",
                "-maxrate",
                "1M",
                "-bufsize",
                "1M",
                "-x264-params",
                "nal-hrd=cbr:force-cfr=1",
                "-g",
                "10",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-threads:a",
                "1",
                "-f",
                "flv",
            ])
            .arg(output)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("ffmpeg preflight succeeded for RTMP loopback test");
        // Drain stderr concurrently, retaining only a bounded prefix for failure reports.
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let captured = stderr.clone();
        let mut pipe = child.stderr.take().unwrap();
        let mut stderr_task = tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            loop {
                let count = pipe.read(&mut buffer).await?;
                if count == 0 {
                    return Ok::<_, std::io::Error>(());
                }
                let mut captured = captured.lock().unwrap();
                let keep = count.min((64 * 1024usize).saturating_sub(captured.len()));
                captured.extend_from_slice(&buffer[..keep]);
            }
        });

        let observed = timeout(Duration::from_secs(15), async {
            loop {
                if let Some(bc) = registry.get("loop").await {
                    let store = bc.store.lock().await;
                    if store.has_piece(0) {
                        let piece: Vec<u8> = (0..store.chunks_per_piece())
                            .flat_map(|chunk| store.chunk(0, chunk).unwrap().into_owned())
                            .collect();
                        return (bc.clone(), piece);
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;

        // Always release the test barrier and reap processes/tasks before assertions.
        // Keeping the child alive until this point also covers servers which wait for
        // unpublish acknowledgement before FFmpeg exits.
        finish_tx.send_replace(true);
        let status = timeout(Duration::from_secs(5), child.wait()).await;
        if status.is_err() {
            let _ = child.start_kill();
            let _ = timeout(Duration::from_secs(5), child.wait()).await;
        }
        let stderr_result = timeout(Duration::from_secs(5), &mut stderr_task).await;
        if stderr_result.is_err() {
            stderr_task.abort();
        }
        cancel_tx.send_replace(true);
        let server_result = timeout(Duration::from_secs(5), &mut server).await;
        if server_result.is_err() {
            server.abort();
        }
        let shutdown = timeout(Duration::from_secs(5), registry.shutdown()).await;
        let counts = operations
            .each_ref()
            .map(|value| value.load(Ordering::Relaxed));
        let stderr = String::from_utf8_lossy(&stderr.lock().unwrap()).into_owned();
        let diagnostics =
            format!("ffmpeg={status:?}; mux/ingest/finish={counts:?}; stderr={stderr}");
        assert!(
            matches!(status, Ok(Ok(status)) if status.success()),
            "FFmpeg RTMP publish failed or timed out: {diagnostics}"
        );
        assert!(
            matches!(stderr_result, Ok(Ok(Ok(())))),
            "stderr capture failed: {stderr_result:?}; {diagnostics}"
        );
        assert!(
            matches!(server_result, Ok(Ok(Ok(())))),
            "RTMP server did not shut down: {server_result:?}; {diagnostics}"
        );
        assert!(
            matches!(shutdown, Ok(Ok(()))),
            "registry shutdown: {shutdown:?}"
        );
        let (bc, piece) = observed.unwrap_or_else(|_| {
            panic!("RTMP publish did not produce a complete piece before tail flush: {diagnostics}")
        });
        assert_eq!(bc.geometry.piece_length, 65_536);
        assert_eq!(piece.len() as u64, bc.geometry.piece_length);
        let (payload, signature) =
            ace_wire::live_auth::split_piece(&piece, bc.auth.signature_len()).unwrap();
        assert!(ace_wire::live_auth::verify_piece(
            &bc.auth.pubkey_der(),
            payload,
            signature,
        ));
        assert!(payload
            .as_chunks::<188>()
            .0
            .iter()
            .all(|packet| packet[0] == 0x47));
    }
}

#[cfg(test)]
mod tests_pull206;
