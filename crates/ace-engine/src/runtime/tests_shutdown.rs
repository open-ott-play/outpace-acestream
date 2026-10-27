//! Graceful HTTP drain must close provider streams before waiting for their bodies.

use super::*;
use crate::provider::{ProviderError, SourceStats, StreamProvider, TsSource};
use async_trait::async_trait;
use bytes::Bytes;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Notify};
use tokio::time::{timeout, Duration};

struct PendingProvider {
    reading: Arc<Notify>,
    dropped: Arc<AtomicBool>,
    pending_open: bool,
}

struct PendingSource {
    reading: Arc<Notify>,
    dropped: Arc<AtomicBool>,
}

#[async_trait]
impl StreamProvider for PendingProvider {
    fn network(&self) -> &'static str {
        "shutdown-test"
    }

    async fn open(&self, _id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        if self.pending_open {
            let _guard = PendingSource {
                reading: self.reading.clone(),
                dropped: self.dropped.clone(),
            };
            self.reading.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(Box::new(PendingSource {
            reading: self.reading.clone(),
            dropped: self.dropped.clone(),
        }))
    }
}

#[async_trait]
impl TsSource for PendingSource {
    async fn next(&mut self) -> Option<Bytes> {
        self.reading.notify_one();
        std::future::pending().await
    }

    fn stats(&self) -> SourceStats {
        SourceStats::default()
    }
}

impl Drop for PendingSource {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

struct TestDirectory(PathBuf);

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn shutdown_with_open_stream(path: &str, pending_open: bool) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let dir = TestDirectory(
                std::env::temp_dir()
                    .join(format!("outpace-http-shutdown-{}", rand::random::<u64>())),
            );
            let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = probe.local_addr().unwrap();
            let mut runtime = build_runtime(
                Config {
                    bind: address,
                    rtmp_bind: "127.0.0.1:0".parse().unwrap(),
                    data_dir: dir.0.clone(),
                    networks: vec![],
                    enable_inbound: false,
                    seed_ttl_secs: 0,
                    experimental_ace_compat: true,
                    ..Config::default()
                },
                vec![],
            )
            .await
            .unwrap();
            // Replace the empty runtime provider registry with a source that never reaches EOF.
            runtime.manager.shutdown().await;
            let reading = Arc::new(Notify::new());
            let dropped = Arc::new(AtomicBool::new(false));
            let mut providers = ProviderRegistry::new();
            providers.register(Arc::new(PendingProvider {
                reading: reading.clone(),
                dropped: dropped.clone(),
                pending_open,
            }));
            let manager = StreamManager::new(providers);
            runtime.manager = manager.clone();
            runtime.networks = vec!["shutdown-test".into()];
            let broadcasts = runtime.broadcasts.clone();
            let (stop, stopped) = oneshot::channel();
            drop(probe);
            let serving = tokio::task::spawn_local(async move {
                serve_http_until(runtime, async move {
                    let _ = stopped.await;
                })
                .await
                .map_err(|error| error.to_string())
            });
            let mut client = timeout(Duration::from_secs(5), async {
                loop {
                    match TcpStream::connect(address).await {
                        Ok(client) => break client,
                        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        Err(error) => panic!("local HTTP connection failed: {error}"),
                    }
                }
            })
            .await
            .expect("runtime must start its local HTTP listener");
            client
                .write_all(
                    format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            timeout(Duration::from_secs(5), reading.notified())
                .await
                .expect("the open response must subscribe and start the pending source");
            let active = manager.list().await;
            if pending_open {
                assert!(active.is_empty(), "pending opens must not publish a source");
            } else {
                assert_eq!(active.len(), 1);
                assert_eq!(active[0].2, 1, "one HTTP body must own the source");
            }
            assert!(!dropped.load(Ordering::SeqCst));

            // Keep the socket and its unfinished response alive. Client disconnection or source
            // EOF must not be required to let the real serving wrapper finish graceful drain.
            stop.send(()).unwrap();
            timeout(Duration::from_secs(5), serving)
                .await
                .expect("shutdown must close the provider before waiting for HTTP drain")
                .unwrap()
                .unwrap();
            let mut response = Vec::new();
            timeout(Duration::from_secs(2), client.read_to_end(&mut response))
                .await
                .expect("shutdown must finish the response without client disconnection")
                .unwrap();
            if pending_open {
                assert!(response.starts_with(b"HTTP/1.1 404 Not Found\r\n"));
            } else {
                assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
            }
            assert!(dropped.load(Ordering::SeqCst));
            assert!(manager.list().await.is_empty());
            assert!(broadcasts.mint("after-stop").await.is_err());
            assert!(TcpListener::bind(address).await.is_ok());
        })
        .await;
}

#[tokio::test]
async fn shutdown_finishes_with_open_native_provider_response() {
    shutdown_with_open_stream("/streams/shutdown-test/pending.ts", false).await;
}

#[tokio::test]
async fn shutdown_finishes_with_open_compatibility_provider_response() {
    shutdown_with_open_stream(
        "/ace/getstream?infohash=0123456789abcdef0123456789abcdef01234567",
        false,
    )
    .await;
}

#[tokio::test]
async fn shutdown_finishes_with_pending_provider_open() {
    shutdown_with_open_stream("/streams/shutdown-test/pending.ts", true).await;
}

#[tokio::test]
async fn occupied_http_bind_releases_inbound_listener() {
    let dir = TestDirectory(
        std::env::temp_dir().join(format!("outpace-inbound-bind-{}", rand::random::<u64>())),
    );
    let occupied_http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_address = peer_probe.local_addr().unwrap();
    let runtime = build_runtime(
        Config {
            bind: occupied_http.local_addr().unwrap(),
            peer_listen: peer_address,
            rtmp_bind: "127.0.0.1:0".parse().unwrap(),
            data_dir: dir.0.clone(),
            networks: vec![],
            enable_inbound: true,
            seed_ttl_secs: 0,
            ..Config::default()
        },
        vec![],
    )
    .await
    .unwrap();
    drop(peer_probe);
    let result = timeout(
        Duration::from_secs(5),
        serve_http_until(runtime, std::future::pending()),
    )
    .await
    .expect("occupied HTTP bind must return promptly");
    assert!(result.is_err());
    assert!(
        TcpListener::bind(peer_address).await.is_ok(),
        "HTTP startup failure must release the already-bound inbound listener"
    );
}

#[tokio::test]
async fn dropping_prebuilt_runtime_cancels_maintenance_tasks() {
    let dir = TestDirectory(
        std::env::temp_dir().join(format!("outpace-runtime-drop-{}", rand::random::<u64>())),
    );
    let runtime = build_runtime(
        Config {
            data_dir: dir.0.clone(),
            networks: vec![],
            enable_inbound: true,
            ..Config::default()
        },
        vec![],
    )
    .await
    .unwrap();
    let maintenance: Vec<_> = runtime
        .background
        .tasks
        .iter()
        .map(|task| task.abort_handle())
        .collect();
    assert!(!maintenance.is_empty());
    tokio::task::yield_now().await;
    assert!(maintenance.iter().all(|task| !task.is_finished()));
    drop(runtime);
    timeout(Duration::from_secs(2), async {
        while maintenance.iter().any(|task| !task.is_finished()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping an unserved runtime must cancel its maintenance tasks");
}

#[tokio::test]
async fn cancelled_background_join_keeps_abort_ownership() {
    let mut background = RuntimeTasks::default();
    let entered = Arc::new(Notify::new());
    let stopped = entered.clone();
    background.spawn_until(move |mut cancel| async move {
        let _ = cancel.changed().await;
        stopped.notify_one();
        std::future::pending::<()>().await;
    });
    let task = background.tasks[0].abort_handle();
    let mut shutdown = Box::pin(background.shutdown());
    assert!(futures::poll!(shutdown.as_mut()).is_pending());
    timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    drop(shutdown);
    assert!(!task.is_finished());
    drop(background);
    timeout(Duration::from_secs(2), async {
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a cancelled join must not detach its unfinished task");
}

async fn finish_with_idle_inbound_connection(cancel_serving: bool) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let dir = TestDirectory(
                std::env::temp_dir()
                    .join(format!("outpace-inbound-stop-{}", rand::random::<u64>())),
            );
            let http_probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let peer_probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let http_address = http_probe.local_addr().unwrap();
            let peer_address = peer_probe.local_addr().unwrap();
            let runtime = build_runtime(
                Config {
                    bind: http_address,
                    peer_listen: peer_address,
                    rtmp_bind: "127.0.0.1:0".parse().unwrap(),
                    data_dir: dir.0.clone(),
                    networks: vec![],
                    enable_inbound: true,
                    ..Config::default()
                },
                vec![],
            )
            .await
            .unwrap();
            let monitor = runtime.reachability.clone().unwrap();
            let weak_manager = Arc::downgrade(&runtime.manager);
            let maintenance: Vec<_> = runtime
                .background
                .tasks
                .iter()
                .map(|task| task.abort_handle())
                .collect();
            let (stop, stopped) = oneshot::channel();
            drop((http_probe, peer_probe));
            let serving = tokio::task::spawn_local(async move {
                serve_http_until(runtime, async move {
                    let _ = stopped.await;
                })
                .await
                .map_err(|error| error.to_string())
            });
            let mut peer = timeout(Duration::from_secs(5), async {
                loop {
                    match TcpStream::connect(peer_address).await {
                        Ok(peer) => break peer,
                        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        Err(error) => panic!("local peer connection failed: {error}"),
                    }
                }
            })
            .await
            .expect("inbound listener must start");
            // No protocol payload: the accepted socket stays idle in its handshake read.
            timeout(Duration::from_secs(2), async {
                while monitor.inbound_peers() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if cancel_serving {
                serving.abort();
                assert!(serving.await.unwrap_err().is_cancelled());
            } else {
                stop.send(()).unwrap();
                timeout(Duration::from_secs(5), serving)
                    .await
                    .expect("graceful shutdown must join inbound tasks")
                    .unwrap()
                    .unwrap();
            }
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_secs(2), peer.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0,
                "shutdown must close the accepted idle socket"
            );
            assert!(TcpListener::bind(peer_address).await.is_ok());
            assert!(TcpListener::bind(http_address).await.is_ok());
            timeout(Duration::from_secs(2), async {
                while maintenance.iter().any(|task| !task.is_finished()) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("all runtime maintenance tasks must finish");
            timeout(Duration::from_secs(2), async {
                while weak_manager.upgrade().is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the HTTP shutdown task must release the manager after cancellation");
        })
        .await;
}

#[tokio::test]
async fn graceful_shutdown_closes_inbound_listener_and_idle_connection() {
    finish_with_idle_inbound_connection(false).await;
}

#[tokio::test]
async fn cancelled_serving_closes_inbound_listener_and_idle_connection() {
    finish_with_idle_inbound_connection(true).await;
}
