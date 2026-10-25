//! One shared download per stream, fanned out to many subscribers via a broadcast channel.
//! Exactly one [`TsSource`] is pulled regardless of how many clients are attached.

use crate::provider::{SourceStats, StatsHandle, TsSource};
use ace_swarm::types::StreamMetadata;
use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, Notify};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    Data(Bytes),
    /// Media that precedes the source's initial live-head catch-up boundary.
    StartupData(Bytes),
    Discontinuity,
}

/// A live session: pulls from one `TsSource` and broadcasts TS chunks to subscribers.
pub struct StreamSession {
    tx: broadcast::WeakSender<StreamEvent>,
    receiver_ready: Arc<Notify>,
    subscribers: Arc<AtomicU64>,
    stats: Arc<Mutex<SourceStats>>,
    live_stats: Option<StatsHandle>,
    metadata: StreamMetadata,
    /// The background pull task; aborted when the session is dropped (last `Arc` gone, e.g.
    /// the manager's idle reaper or an explicit force-stop) so a live download doesn't keep
    /// running headless.
    pump: tokio::task::JoinHandle<()>,
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

/// Counts one source owner without retaining unread broadcast data.
pub struct SessionPin {
    count: Arc<AtomicU64>,
}

impl Drop for SessionPin {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A subscription that decrements the live subscriber count on drop.
pub struct Subscription {
    pub rx: broadcast::Receiver<StreamEvent>,
    pin: SessionPin,
}

impl Subscription {
    /// Transfer ownership unchanged while dropping this unread receiver. HLS leases need
    /// a source pin, but their packager already owns the receiver that consumes the media.
    pub fn into_pin(self) -> SessionPin {
        self.pin
    }
}

impl StreamSession {
    /// Own `source` in a background task and start pulling after the first receiver attaches.
    /// Initial media must not race ahead of that receiver. The pump then runs until the source
    /// ends, including between receivers. `buffer` is the broadcast backlog (how far a
    /// slow/late receiver may lag before it drops old chunks).
    pub fn start(mut source: Box<dyn TsSource>, buffer: usize) -> Arc<StreamSession> {
        let (tx, _rx) = broadcast::channel(buffer);
        let stats = Arc::new(Mutex::new(SourceStats::default()));
        let metadata = source.metadata();
        let live_stats = source.stats_handle();
        let weak_tx = tx.downgrade();
        let pump_tx = tx;
        let pump_stats = stats.clone();
        let receiver_ready = Arc::new(Notify::new());
        let pump_ready = receiver_ready.clone();
        let pump = tokio::spawn(async move {
            pump_ready.notified().await;
            while let Some(chunk) = source.next().await {
                *pump_stats.lock().await = source.stats();
                if source.take_discontinuity() {
                    let _ = pump_tx.send(StreamEvent::Discontinuity);
                }
                // Err just means no live receivers right now; keep pulling (live).
                let event = if source.startup_complete() {
                    StreamEvent::Data(chunk)
                } else {
                    StreamEvent::StartupData(chunk)
                };
                let _ = pump_tx.send(event);
            }
        });
        Arc::new(StreamSession {
            tx: weak_tx,
            receiver_ready,
            subscribers: Arc::new(AtomicU64::new(0)),
            stats,
            live_stats,
            metadata,
            pump,
        })
    }

    /// Own this session without attaching a receiver or starting source consumption.
    pub(crate) fn pin(&self) -> SessionPin {
        self.subscribers.fetch_add(1, Ordering::SeqCst);
        SessionPin {
            count: self.subscribers.clone(),
        }
    }

    pub fn subscribe(&self) -> Subscription {
        let pin = self.pin();
        Subscription {
            rx: self.raw_receiver(),
            pin,
        }
    }

    pub fn subscriber_count(&self) -> u64 {
        self.subscribers.load(Ordering::SeqCst)
    }

    /// The source pump has exited (including EOF or failure). Old subscribers may still
    /// drain their queued data, but a fresh playback acquisition may start a new session.
    /// Before the first receiver attaches, EOF and pull-driven statistics remain unobserved.
    pub fn is_finished(&self) -> bool {
        self.pump.is_finished()
    }

    /// A receiver that does NOT count as a client (used by internal consumers like the HLS
    /// packager, which must not pin the session open against idle teardown).
    pub fn raw_receiver(&self) -> broadcast::Receiver<StreamEvent> {
        if let Some(tx) = self.tx.upgrade() {
            let rx = tx.subscribe();
            // Keep a permit even if the pump has not been polled yet. The receiver must
            // already exist when the source is allowed to yield its initial media.
            self.receiver_ready.notify_one();
            rx
        } else {
            // A late subscriber must observe EOF too, without creating a new live sender.
            let (_tx, rx) = broadcast::channel(1);
            rx
        }
    }

    pub async fn stats(&self) -> SourceStats {
        if let Some(reader) = &self.live_stats {
            return reader.snapshot();
        }
        self.stats.lock().await.clone()
    }

    pub fn metadata(&self) -> &StreamMetadata {
        &self.metadata
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::StreamProvider;
    use crate::testprovider::TestProvider;
    use ace_swarm::types::StreamMetadata;

    #[tokio::test]
    async fn session_captures_source_metadata_before_pumping() {
        struct MetadataSource;
        #[async_trait::async_trait]
        impl TsSource for MetadataSource {
            async fn next(&mut self) -> Option<Bytes> {
                std::future::pending().await
            }

            fn stats(&self) -> SourceStats {
                SourceStats::default()
            }

            fn metadata(&self) -> StreamMetadata {
                StreamMetadata {
                    title: Some("Synthetic Demo Channel".to_string()),
                    bitrate: Some(100_000),
                    categories: vec!["sports".to_string()],
                }
            }
        }

        let session = StreamSession::start(Box::new(MetadataSource), 8);

        assert_eq!(
            session.metadata(),
            &StreamMetadata {
                title: Some("Synthetic Demo Channel".to_string()),
                bitrate: Some(100_000),
                categories: vec!["sports".to_string()],
            }
        );
    }

    #[tokio::test]
    async fn two_subscribers_share_one_source() {
        let source = TestProvider { chunks: 5 }.open("x").await.unwrap();
        let session = StreamSession::start(source, 16);
        // The first subscription leaves a start permit before the pump is polled.
        let mut a = session.subscribe();
        let mut b = session.subscribe();
        assert_eq!(session.subscriber_count(), 2);
        let ca = a.rx.recv().await.unwrap();
        let cb = b.rx.recv().await.unwrap();
        assert_eq!(ca, cb);
        let StreamEvent::Data(ca) = ca else {
            panic!("expected data")
        };
        assert_eq!(ca[0], 0x47);
        drop(a);
        assert_eq!(session.subscriber_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn ownership_pin_does_not_consume_initial_media() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        struct ImmediateSource {
            pulls: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl TsSource for ImmediateSource {
            async fn next(&mut self) -> Option<Bytes> {
                (self.pulls.fetch_add(1, Ordering::SeqCst) == 0)
                    .then(|| Bytes::from_static(b"initial media"))
            }
            fn stats(&self) -> SourceStats {
                SourceStats::default()
            }
        }

        let pulls = Arc::new(AtomicUsize::new(0));
        let session = StreamSession::start(
            Box::new(ImmediateSource {
                pulls: pulls.clone(),
            }),
            8,
        );
        let pin = session.pin();
        // Let every ready task run, including the pump, without attaching a consumer.
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(pulls.load(Ordering::SeqCst), 0);
        assert_eq!(session.subscriber_count(), 1);
        assert!(!session.is_finished());

        let mut rx = session.raw_receiver();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            StreamEvent::Data(Bytes::from_static(b"initial media"))
        );
        assert_eq!(rx.recv().await, Err(broadcast::error::RecvError::Closed));
        assert_eq!(pulls.load(Ordering::SeqCst), 2);
        drop(pin);
        assert_eq!(session.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn dropping_session_aborts_pump_and_drops_source() {
        use async_trait::async_trait;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        // A source that never yields (like a live swarm) but flags when it is dropped.
        struct BlockingSource(Arc<AtomicBool>);
        impl Drop for BlockingSource {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        #[async_trait]
        impl TsSource for BlockingSource {
            async fn next(&mut self) -> Option<Bytes> {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                None
            }
            fn stats(&self) -> SourceStats {
                SourceStats::default()
            }
        }

        for attach_receiver in [false, true] {
            let dropped = Arc::new(AtomicBool::new(false));
            let session = StreamSession::start(Box::new(BlockingSource(dropped.clone())), 16);
            let _pin = session.pin();
            let _receiver = attach_receiver.then(|| session.raw_receiver());
            tokio::task::yield_now().await;
            assert!(!dropped.load(Ordering::SeqCst));
            drop(session); // both waiting and running pumps must drop their source

            tokio::time::timeout(Duration::from_secs(1), async {
                while !dropped.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("pump must be aborted and source dropped");
        }
    }

    #[tokio::test]
    async fn provider_discontinuity_precedes_first_post_gap_chunk() {
        use async_trait::async_trait;

        struct GapSource {
            step: u8,
            gap: bool,
        }
        #[async_trait]
        impl TsSource for GapSource {
            async fn next(&mut self) -> Option<Bytes> {
                self.step += 1;
                match self.step {
                    1 => Some(Bytes::from_static(b"before")),
                    2 => {
                        self.gap = true;
                        Some(Bytes::from_static(b"after"))
                    }
                    _ => None,
                }
            }
            fn take_discontinuity(&mut self) -> bool {
                std::mem::take(&mut self.gap)
            }
            fn stats(&self) -> SourceStats {
                SourceStats::default()
            }
        }

        let session = StreamSession::start(
            Box::new(GapSource {
                step: 0,
                gap: false,
            }),
            8,
        );
        let mut sub = session.subscribe();
        assert_eq!(
            sub.rx.recv().await.unwrap(),
            StreamEvent::Data(Bytes::from_static(b"before"))
        );
        assert_eq!(sub.rx.recv().await.unwrap(), StreamEvent::Discontinuity);
        assert_eq!(
            sub.rx.recv().await.unwrap(),
            StreamEvent::Data(Bytes::from_static(b"after"))
        );
    }
}
