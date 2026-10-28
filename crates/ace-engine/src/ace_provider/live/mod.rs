use super::*;
use ace_swarm::live_session::{follow_live, LiveCounters, LiveSeedOptions, LiveSessionInputs};

const INITIAL_DISCOVERY_BUDGET: Duration = Duration::from_secs(10);

/// Retry a completed empty discovery once within the original deadline. An unfinished
/// (silent) run is never replaced by a competing generation.
async fn initial_candidates(
    discovery: &PeerDiscovery,
    mut run: DiscoveryRun,
    mut peers: Vec<SocketAddrV4>,
    deadline: tokio::time::Instant,
) -> (Vec<SocketAddrV4>, Option<DiscoveryRun>) {
    if peers.is_empty() {
        for attempt in 0..2 {
            match tokio::time::timeout_at(deadline, run.next_candidate()).await {
                Ok(Some(peer)) => {
                    peers.push(peer);
                    break;
                }
                Ok(None) if attempt == 0 && tokio::time::Instant::now() < deadline => {
                    // Drop the completed owner before starting its replacement.
                    drop(run);
                    run = DiscoveryRun::start(
                        discovery,
                        DiscoveryOptions {
                            dht_budget: deadline
                                .saturating_duration_since(tokio::time::Instant::now()),
                            ..DiscoveryOptions::default()
                        },
                    );
                }
                _ => break,
            }
        }
    }
    while let Ok(peer) = run.try_next_candidate() {
        if !peers.contains(&peer) {
            peers.push(peer);
        }
    }
    if peers.is_empty() {
        (peers, None)
    } else {
        (peers, Some(run))
    }
}

pub(super) struct AceSource {
    pub(super) metrics: Option<Arc<crate::live_stats::LiveMetrics>>,
    pub(super) _sampler: Option<crate::live_stats::MetricsSampler>,
    pub(super) rx: mpsc::Receiver<LiveOutput>,
    pub(super) worker: Option<tokio::task::JoinHandle<()>>,
    pub(super) discontinuity: bool,
    pub(super) startup_complete: bool,
    pub(super) peers: Arc<AtomicU32>,
    pub(super) downloaded: Arc<AtomicU64>,
    pub(super) uploaded: Arc<AtomicU64>,
    pub(super) peers_served: Arc<AtomicU32>,
    pub(super) metadata: StreamMetadata,
}

impl Drop for AceSource {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.abort();
        }
    }
}

#[async_trait]
impl TsSource for AceSource {
    async fn next(&mut self) -> Option<Bytes> {
        let output = self.rx.recv().await?;
        self.discontinuity = output.discontinuity;
        self.startup_complete = output.startup_complete;
        Some(output.bytes)
    }
    fn take_discontinuity(&mut self) -> bool {
        std::mem::take(&mut self.discontinuity)
    }
    fn startup_complete(&self) -> bool {
        self.startup_complete
    }
    fn stats(&self) -> SourceStats {
        if let Some(metrics) = &self.metrics {
            return crate::provider::StatsReader::snapshot(metrics.as_ref());
        }
        SourceStats {
            peers: self.peers.load(Ordering::Relaxed),
            bitrate: 0,
            buffer_ms: 0,
            downloaded: self.downloaded.load(Ordering::Relaxed),
            uploaded: self.uploaded.load(Ordering::Relaxed),
            peers_served: self.peers_served.load(Ordering::Relaxed),
            ..SourceStats::default()
        }
    }
    fn stats_handle(&self) -> Option<crate::provider::StatsHandle> {
        self.metrics
            .as_ref()
            .map(|metrics| metrics.clone() as crate::provider::StatsHandle)
    }
    fn metadata(&self) -> StreamMetadata {
        self.metadata.clone()
    }
}
impl AceProvider {
    // Shared resolved-live entry point; injected discovery keeps protocol regressions offline.
    pub(super) async fn open_resolved(
        &self,
        id: &str,
        info: StreamInfo,
        discovery: PeerDiscovery,
    ) -> Result<Box<dyn TsSource>, ProviderError> {
        let resolved_at = Instant::now();
        // Bootstrap peers are the proven/direct path and must be tried without waiting for
        // tracker/DHT discovery. Background refill can still discover more peers after start.
        let mut initial_run = None;
        let peers = if self.bootstrap_peers.is_empty() {
            let deadline = tokio::time::Instant::now() + INITIAL_DISCOVERY_BUDGET;
            let run = DiscoveryRun::start(
                &discovery,
                DiscoveryOptions {
                    dht_budget: INITIAL_DISCOVERY_BUDGET,
                    ..DiscoveryOptions::default()
                },
            );
            let _ = tokio::time::timeout_at(deadline, self.warm_peers.initialized()).await;
            let hints = self.warm_peers.candidate_addresses(&info.infohash);
            let (peers, run) = initial_candidates(&discovery, run, hints, deadline).await;
            initial_run = run;
            peers
        } else {
            self.bootstrap_peers.clone()
        };
        crate::alog!(
            "[ace] discovery stage=first-candidates after_resolution_ms={} count={}",
            resolved_at.elapsed().as_millis(),
            peers.len()
        );
        crate::alog!("[ace] open {id}: discovered {} peer(s)", peers.len());
        if peers.is_empty() {
            return Err(ProviderError::Backend(
                "no peers (no trackers/bootstrap)".into(),
            ));
        }

        let (tx, rx) = mpsc::channel::<LiveOutput>(256);
        let peer_count = Arc::new(AtomicU32::new(0));
        let downloaded = Arc::new(AtomicU64::new(0));
        let uploaded = Arc::new(AtomicU64::new(0));
        let peers_served = Arc::new(AtomicU32::new(0));
        let identity = self.identity.clone();
        let stats_peers = peer_count.clone();
        let stats_downloaded = downloaded.clone();
        let stats_uploaded = uploaded.clone();
        let stats_peers_served = peers_served.clone();
        let metrics = crate::live_stats::LiveMetrics::new(
            stats_downloaded.clone(),
            Arc::new(AtomicU64::new(0)),
            stats_uploaded.clone(),
            stats_peers.clone(),
            stats_peers_served.clone(),
        );
        let sampler = crate::live_stats::spawn_sampler(metrics.clone());
        let prefetch_pieces = self.prefetch_policy_for(&info);
        let startup_buffer = self.startup_buffer;
        let bitrate = info.metadata.bitrate;
        let seed = LiveSeedOptions {
            metrics: Some(metrics.clone()),
            registry: self.seed_registry.clone(),
            store_factory: self.store_factory(&info),
            enabled: self.enable_seeding,
            prefetch_pieces,
            live_recovery: self.live_recovery.into(),
            warm_peers: self.warm_peers.clone(),
        };
        let announce_info = info.clone();
        let metadata = info.metadata.clone();
        let announce_port = self.announce_peer_port.clone();
        let reachability = self.reachability.clone();
        let worker = tokio::spawn(async move {
            // Run the download loop and the periodic seeder self-announce concurrently;
            // whichever ends first (normally `follow_live`, when the consumer drops) tears
            // down the other — no separate lifecycle to manage.
            tokio::select! {
                _ = follow_live(LiveSessionInputs { info, peers, identity, output: tx, counters: LiveCounters { peers: stats_peers, downloaded, uploaded, peers_served }, seed, discovery, reachability, initial_run }) => {},
                _ = announce_seeder_periodically(announce_info, announce_port) => {},
            }
        });
        let source = AceSource {
            metrics: Some(metrics),
            _sampler: Some(sampler),
            rx,
            worker: Some(worker),
            discontinuity: false,
            startup_complete: false,
            peers: peer_count,
            downloaded: stats_downloaded,
            uploaded: stats_uploaded,
            peers_served: stats_peers_served,
            metadata,
        };
        Ok(StartupBufferedSource::new(
            Box::new(source),
            startup_buffer,
            bitrate,
        ))
    }
}

#[cfg(test)]
mod initial_discovery_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn completed_empty_run_retries_and_keeps_replacement_for_late_candidates() {
        let calls = Arc::new(AtomicU32::new(0));
        let called = calls.clone();
        let first = SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 10001);
        let later = SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 10002);
        let discovery: PeerDiscovery = Arc::new(move |_, sender| {
            let attempt = called.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                if attempt == 1 {
                    sender.send(first).await.unwrap();
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let _ = sender.send(later).await;
                }
            })
        });
        let run = DiscoveryRun::start(&discovery, DiscoveryOptions::default());
        let (peers, run) = initial_candidates(
            &discovery,
            run,
            Vec::new(),
            tokio::time::Instant::now() + INITIAL_DISCOVERY_BUDGET,
        )
        .await;
        assert_eq!(peers, vec![first]);
        assert_eq!(run.unwrap().next_candidate().await, Some(later));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn empty_retry_shares_deadline_and_remaining_dht_budget() {
        let calls = Arc::new(AtomicU32::new(0));
        let called = calls.clone();
        let budgets = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = budgets.clone();
        let discovery: PeerDiscovery = Arc::new(move |options, sender| {
            let attempt = called.fetch_add(1, Ordering::Relaxed);
            observed.lock().unwrap().push(options.dht_budget);
            Box::pin(async move {
                if attempt == 0 {
                    tokio::time::sleep(Duration::from_secs(6)).await;
                } else {
                    sender.closed().await;
                }
            })
        });
        let started = tokio::time::Instant::now();
        let run = DiscoveryRun::start(
            &discovery,
            DiscoveryOptions {
                dht_budget: INITIAL_DISCOVERY_BUDGET,
                ..DiscoveryOptions::default()
            },
        );
        let (peers, run) = initial_candidates(
            &discovery,
            run,
            Vec::new(),
            started + INITIAL_DISCOVERY_BUDGET,
        )
        .await;
        assert!(peers.is_empty() && run.is_none());
        assert_eq!(started.elapsed(), INITIAL_DISCOVERY_BUDGET);
        assert_eq!(
            *budgets.lock().unwrap(),
            vec![Duration::from_secs(10), Duration::from_secs(4)]
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn silent_discovery_is_bounded_without_starting_another_generation() {
        let calls = Arc::new(AtomicU32::new(0));
        let called = calls.clone();
        let discovery: PeerDiscovery = Arc::new(move |_, sender| {
            called.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move { sender.closed().await })
        });
        let started = tokio::time::Instant::now();
        let run = DiscoveryRun::start(&discovery, DiscoveryOptions::default());
        let (peers, run) = initial_candidates(
            &discovery,
            run,
            Vec::new(),
            started + INITIAL_DISCOVERY_BUDGET,
        )
        .await;
        assert!(peers.is_empty() && run.is_none());
        assert_eq!(started.elapsed(), INITIAL_DISCOVERY_BUDGET);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn two_completed_empty_runs_stop_without_a_retry_loop() {
        let calls = Arc::new(AtomicU32::new(0));
        let called = calls.clone();
        let discovery: PeerDiscovery = Arc::new(move |_, _| {
            called.fetch_add(1, Ordering::Relaxed);
            Box::pin(async {})
        });
        let run = DiscoveryRun::start(&discovery, DiscoveryOptions::default());
        let (peers, run) = initial_candidates(
            &discovery,
            run,
            Vec::new(),
            tokio::time::Instant::now() + INITIAL_DISCOVERY_BUDGET,
        )
        .await;
        assert!(peers.is_empty() && run.is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
