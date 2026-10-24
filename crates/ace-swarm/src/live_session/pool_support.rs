use super::*;

pub(super) fn record_pool_losses(
    candidates: &mut SessionCandidates,
    loss_count: &mut usize,
    addrs: impl IntoIterator<Item = SocketAddrV4>,
) {
    for addr in addrs {
        candidates.failed(addr, Instant::now());
        *loss_count = loss_count.saturating_add(1);
    }
}

pub(super) fn update_runtime_window(
    peers: &mut BTreeMap<u64, PeerRuntime>,
    continuity: &mut Continuity,
    peer_id: u64,
    max_piece: u64,
) {
    if let Some(peer) = peers.get_mut(&peer_id) {
        peer.max_piece = peer.max_piece.max(max_piece);
        continuity.update_peer_window(peer_id, peer.min_piece, peer.max_piece);
    }
}

/// One peer's window can advance while a different peer already advertised a newer
/// global head. Keep its request coverage and activity independent of the global head.
pub(super) fn observe_runtime_have(
    peers: &mut BTreeMap<u64, PeerRuntime>,
    continuity: &mut Continuity,
    peer_id: u64,
    piece: u64,
) -> bool {
    let advanced = piece > continuity.head
        || peers
            .get(&peer_id)
            .is_some_and(|peer| piece > peer.max_piece);
    continuity.head = continuity.head.max(piece);
    update_runtime_window(peers, continuity, peer_id, piece);
    advanced
}

/// The trailing live pointer evicts old request coverage. A delayed update cannot
/// move the minimum backwards and make the scheduler retry already-evicted pieces.
pub(super) fn observe_runtime_eviction(
    peers: &mut BTreeMap<u64, PeerRuntime>,
    continuity: &mut Continuity,
    peer_id: u64,
    first_piece: u64,
) {
    if let Some(peer) = peers.get_mut(&peer_id) {
        peer.min_piece = peer.min_piece.max(first_piece);
        peer.max_piece = peer.max_piece.max(peer.min_piece);
        continuity.update_peer_window(peer_id, peer.min_piece, peer.max_piece);
    }
}

pub(super) fn observe_runtime_window_payload(
    peers: &mut BTreeMap<u64, PeerRuntime>,
    continuity: &mut Continuity,
    peer_id: u64,
    payload: &[u8],
) -> Option<bool> {
    let window = LiveWindow::from_myinfo_payload(payload)?;
    if window.max_piece > u64::from(u32::MAX) {
        return None;
    }
    let advanced = observe_runtime_have(peers, continuity, peer_id, window.max_piece);
    observe_runtime_eviction(peers, continuity, peer_id, window.min_piece);
    Some(advanced)
}

pub(super) async fn send_peer_command(
    peer_id: u64,
    msg: PeerMessage,
    peers: &mut BTreeMap<u64, PeerRuntime>,
    continuity: &mut Continuity,
) -> Option<SocketAddrV4> {
    let sender = peers.get(&peer_id).map(|peer| peer.commands.clone())?;
    if sender.try_send(PeerCommand::Send(msg)).is_err() {
        drop_peer_runtime(peer_id, peers, continuity)
    } else {
        None
    }
}

#[cfg(test)]
pub(super) fn spawn_learned_connects(
    candidates: &mut SessionCandidates,
    peers: &BTreeMap<u64, PeerRuntime>,
    infohash: [u8; 20],
    max_parallel: usize,
    tx: &mpsc::Sender<PoolRefill>,
    tasks: &mut tokio::task::JoinSet<(SocketAddrV4, Option<PeerConnectFailure>, bool)>,
    pending: &mut HashSet<SocketAddrV4>,
) {
    spawn_candidate_connects(
        candidates,
        peers,
        infohash,
        max_parallel,
        tx,
        tasks,
        pending,
        false,
    );
}

pub(super) fn finish_candidate_connect(
    (addr, failure, queued): (SocketAddrV4, Option<PeerConnectFailure>, bool),
    pending: &mut HashSet<SocketAddrV4>,
    candidates: &mut SessionCandidates,
) -> bool {
    if !queued {
        pending.remove(&addr);
    }
    if let Some(failure) = failure {
        candidates.explored(failure.addr);
        candidates.failed(failure.addr, Instant::now());
        return true;
    }
    false
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_candidate_connects(
    candidates: &mut SessionCandidates,
    peers: &BTreeMap<u64, PeerRuntime>,
    infohash: [u8; 20],
    max_parallel: usize,
    tx: &mpsc::Sender<PoolRefill>,
    tasks: &mut tokio::task::JoinSet<(SocketAddrV4, Option<PeerConnectFailure>, bool)>,
    pending: &mut HashSet<SocketAddrV4>,
    include_discovered: bool,
) {
    let (sources, pex) = candidates.learned_counts();
    if !include_discovered && sources + pex == 0 {
        return;
    }
    let active = peers.values().map(|peer| peer.addr).collect();
    let cohort = candidates.exploration_cohort(Instant::now(), &active, pending);
    let eligible = if include_discovered {
        candidates.eligible(Instant::now())
    } else {
        candidates.eligible_learned(Instant::now())
    };
    for addr in eligible {
        if Some(candidates.explorations(addr)) != cohort {
            continue;
        }
        if tasks.len() >= max_parallel || pending.len() >= max_parallel {
            break;
        }
        if peers.values().any(|p| p.addr == addr) || !pending.insert(addr) {
            continue;
        }
        // Reserve a retry slot before spawning: gossip floods cannot duplicate pending work.
        candidates.attempting(addr, Instant::now() + CONNECT_TIMEOUT);
        let discovered = candidates.kind(addr) == CandidateKind::Discovered;
        let tx = tx.clone();
        tasks.spawn(async move {
            let (failure, queued) = match connect_upstream(addr, infohash).await {
                PeerConnectAttempt::Connected(upstream, _) => {
                    let receipt = if discovered {
                        PoolRefill::ReservedDiscovered(upstream)
                    } else {
                        PoolRefill::Learned(upstream)
                    };
                    (None, tx.try_send(receipt).is_ok())
                }
                PeerConnectAttempt::Failed(failure) => (Some(failure), false),
            };
            // Successful queued peers remain pending until activation/rejection; neither
            // cooldown expiry nor gossip can race an older cohort into their capacity.
            (addr, failure, queued)
        });
    }
}

// Stale peers are gossip transports only. One deadline covers the whole batch,
// including our handshake writes; no Interested, media requests or continuity updates.
pub(super) async fn harvest_stale_gossip(
    upstreams: Vec<ConnectedUpstream>,
    identity: &Identity,
    candidates: &mut SessionCandidates,
) {
    harvest_stale_gossip_with_budget(upstreams, identity, candidates, STALE_GOSSIP_BUDGET).await;
}

pub(super) async fn harvest_stale_gossip_with_budget(
    upstreams: Vec<ConnectedUpstream>,
    identity: &Identity,
    candidates: &mut SessionCandidates,
    budget: Duration,
) {
    let deadline = tokio::time::Instant::now() + budget;
    let mut tasks = tokio::task::JoinSet::new();
    for mut upstream in upstreams {
        let hs = OutgoingExtendedHandshake {
            ace_metadata_version: 1,
            ut_metadata_id: 2,
            mi: None,
            node: NodeFields::default(),
            peer_ip: Some(upstream.addr.ip().octets()),
            metadata_size: None,
        };
        let handshake = PeerMessage::Extended {
            ext_id: 0,
            payload: hs.sign_and_encode(identity),
        };
        tasks.spawn(async move {
            let mut learned = SessionCandidates::default();
            let _ = tokio::time::timeout_at(deadline, async {
                if upstream.session.send(&handshake).await.is_err() {
                    return;
                }
                for _ in 0..32 {
                    match upstream.session.read_message().await {
                        Ok(PeerMessage::Unknown { id: 12, payload }) => {
                            for addr in ace_wire::peer_exchange::parse_peer_exchange(&payload) {
                                learned.learn(addr, CandidateKind::Pex);
                            }
                        }
                        Ok(PeerMessage::Unknown { id: 36, payload }) => {
                            if let Some(addr) =
                                ace_wire::peer_exchange::parse_peer_announce(&payload)
                            {
                                learned.learn(addr, CandidateKind::Source);
                            }
                        }
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
            })
            .await;
            learned.into_learned()
        });
    }
    while let Some(joined) = tasks.join_next().await {
        if let Ok(learned) = joined {
            for (addr, kind) in learned {
                candidates.learn(addr, kind);
            }
        }
    }
}

pub(super) async fn advance_pool_requests(
    peers: &mut BTreeMap<u64, PeerRuntime>,
    continuity: &mut Continuity,
    chunks_per_piece: u16,
) -> Vec<SocketAddrV4> {
    continuity.skip_far_behind_live(Instant::now());
    if continuity.startup_deadline.is_some() {
        return Vec::new();
    }
    let mut failed = Vec::new();
    let request_head = continuity.request_head();
    let assignments = continuity.active_peers.assign_if(
        &mut continuity.scheduler,
        continuity.reasm.next_needed(),
        request_head,
        |assignment| {
            let Some(peer) = peers.get(&assignment.peer_id) else {
                return false;
            };
            match peer.commands.try_send(PeerCommand::RequestPiece {
                piece: assignment.piece,
                chunks_per_piece,
            }) {
                Ok(()) => true,
                // A busy worker is not a dead peer. The periodic sweep retries admission.
                Err(mpsc::error::TrySendError::Full(_)) => false,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    failed.push(assignment.peer_id);
                    false
                }
            }
        },
    );
    let now = Instant::now();
    for assignment in assignments {
        continuity
            .requested_at
            .entry(assignment.piece)
            .or_insert(now);
    }
    failed.sort_unstable();
    failed.dedup();
    failed
        .into_iter()
        .filter_map(|peer_id| drop_peer_runtime(peer_id, peers, continuity))
        .collect()
}

pub(super) fn observe_live_window(c: &mut Continuity, peer: u64, payload: &[u8]) {
    let Ok((root, _)) = ace_wire::bencode::Bencode::parse_prefix(payload) else {
        return;
    };
    let dict = if root.get(b"max_piece").is_some() {
        &root
    } else if let Some(dict) = root.get(b"mi") {
        dict
    } else {
        return;
    };
    let value = |key: &[u8]| {
        dict.get(key)
            .and_then(ace_wire::bencode::Bencode::as_int)
            .and_then(|value| u64::try_from(value).ok())
    };
    c.active_peers
        .observe_window(peer, value(b"min_piece").zip(value(b"max_piece")));
}

pub(super) fn ready_buffer_snapshot(continuity: &Continuity, bitrate: Option<u64>) -> (u64, u64) {
    let mut clock = ace_media::mpegts::SelectedPcrClock::new();
    let mut bytes = 0_u64;
    let mut pieces = 0_u64;
    for chunk in continuity.reasm.ready_chunks() {
        pieces += 1;
        bytes = bytes.saturating_add(chunk.len() as u64);
        for packet in chunk.as_chunks::<{ ace_media::mpegts::TS_PACKET_LEN }>().0 {
            clock.observe(ace_media::mpegts::ts_timing(packet));
        }
    }
    let duration = clock
        .elapsed()
        .filter(|duration| !duration.is_zero())
        .or_else(|| {
            bitrate
                .filter(|rate| *rate > 0)
                .map(|rate| Duration::from_secs_f64(bytes as f64 / rate as f64))
        })
        .unwrap_or_default();
    (duration.as_millis().min(u64::MAX as u128) as u64, pieces)
}

pub(super) fn publish_live_stats(
    c: &Continuity,
    metrics: Option<&Arc<dyn LiveObserver>>,
    info: &StreamInfo,
) {
    let Some(metrics) = metrics else {
        return;
    };
    let (buffer_ms, buffer_pieces) = ready_buffer_snapshot(c, info.metadata.bitrate);
    let bounds = c.active_peers.coverage_bounds();
    metrics.publish_position(
        Some(LivePosition {
            pos: Some(c.reasm.next_needed()),
            last: Some(c.head),
            live_first: bounds.map(|(first, _)| first),
            live_last: bounds.map(|(_, last)| last),
            buffer_pieces,
        }),
        buffer_ms,
    );
}

pub(super) fn take_ready_with_stats(
    c: &mut Continuity,
    metrics: Option<&Arc<dyn LiveObserver>>,
    info: &StreamInfo,
) -> Vec<u8> {
    publish_live_stats(c, metrics, info);
    let ready = c.reasm.take_ready();
    if let Some(metrics) = metrics {
        metrics.received_bytes(ready.len() as u64);
    }
    publish_live_stats(c, metrics, info);
    ready
}

/// Periodic self-heal for the request pipeline (runs on each pool loop tick): re-requeue any
/// piece outstanding past the configured request timeout and skip a piece evicted from every
/// upstream window, then re-issue requests. A timed-out piece is only requeued in the *scheduler* (its
/// original peer keeps the in-flight slot), so [`ActivePeers::assign`] steers the retry to a
/// peer with more spare capacity — i.e. a different, faster one when available. If no retry
/// was assigned, release the old slots for that piece so full capacity cannot prevent retrying.
/// Returns any peers dropped while re-issuing requests.
pub(super) async fn retransmit_stalled_requests(
    peers: &mut BTreeMap<u64, PeerRuntime>,
    continuity: &mut Continuity,
    chunks_per_piece: u16,
    now: Instant,
) -> Vec<SocketAddrV4> {
    let mut changed = false;
    let head = continuity.head;
    if let Some(target) = continuity.skip_far_behind_live(now) {
        crate::alog!(
            "[ace] cursor fell too far behind live edge {head}; re-syncing forward to piece {target}"
        );
        changed = true;
    }
    if let Some(floor) = continuity.skip_evicted_gap(now) {
        crate::alog!(
            "[ace] next needed piece evicted from all upstream windows; skipping ahead to {floor}"
        );
        changed = true;
    }
    let timed_out = continuity.timed_out_requests(now);
    if !timed_out.is_empty() {
        let request_timeout = continuity.live_recovery.request_timeout();
        crate::alog!(
            "[ace] re-requesting {} piece(s) outstanding > {:?} (from {})",
            timed_out.len(),
            request_timeout,
            continuity.reasm.next_needed()
        );
        for &piece in &timed_out {
            continuity.scheduler.on_drop(piece);
            continuity.requested_at.remove(&piece);
        }
        changed = true;
    }
    if changed {
        let mut lost = advance_pool_requests(peers, continuity, chunks_per_piece).await;
        let mut released = false;
        for piece in timed_out {
            if !continuity.requested_at.contains_key(&piece) {
                continuity.active_peers.complete_everywhere(piece);
                released = true;
            }
        }
        if released {
            lost.extend(advance_pool_requests(peers, continuity, chunks_per_piece).await);
        }
        lost
    } else {
        // A worker may have drained a previously full command queue without any new event.
        advance_pool_requests(peers, continuity, chunks_per_piece).await
    }
}

pub(super) fn drop_peer_runtime(
    peer_id: u64,
    peers: &mut BTreeMap<u64, PeerRuntime>,
    continuity: &mut Continuity,
) -> Option<SocketAddrV4> {
    let peer = peers.remove(&peer_id)?;
    continuity.admitted_upstreams = peers.len() as u32;
    continuity.publish_upstreams();
    for piece in continuity.active_peers.remove(peer_id) {
        continuity.scheduler.on_drop(piece);
    }
    let _ = peer.commands.try_send(PeerCommand::Stop);
    peer.worker.abort();
    Some(peer.addr)
}

pub(super) fn shutdown_peer_runtimes(peers: &mut BTreeMap<u64, PeerRuntime>) {
    for (_, peer) in std::mem::take(peers) {
        let _ = peer.commands.try_send(PeerCommand::Stop);
        peer.worker.abort();
    }
}
