use super::*;

#[tokio::test]
async fn peer_worker_sends_chunk_requests_from_commands() {
    use tokio::io::AsyncReadExt;

    let (client, mut server) = tokio::io::duplex(4096);
    let session = PeerSession::new(client).with_timeout(Duration::from_millis(250));
    let addr: SocketAddrV4 = "1.2.3.4:8621".parse().unwrap();
    let (command_tx, command_rx) = mpsc::channel(4);
    let (event_tx, _event_rx) = mpsc::channel(4);
    let worker = tokio::spawn(peer_worker(
        1,
        addr,
        session,
        command_rx,
        event_tx,
        Arc::new(AtomicU64::new(0)),
    ));

    command_tx
        .send(PeerCommand::RequestPiece {
            piece: 42,
            chunks_per_piece: 2,
        })
        .await
        .unwrap();

    let expected = [chunk_request(42, 0).encode(), chunk_request(42, 1).encode()].concat();
    let mut got = vec![0u8; expected.len()];
    server.read_exact(&mut got).await.unwrap();
    assert_eq!(got, expected);

    drop(command_tx);
    worker.await.unwrap();
}

#[tokio::test]
async fn peer_worker_emits_messages_from_session() {
    use tokio::io::AsyncWriteExt;

    let (client, mut server) = tokio::io::duplex(4096);
    let session = PeerSession::new(client).with_timeout(Duration::from_millis(250));
    let addr: SocketAddrV4 = "1.2.3.4:8621".parse().unwrap();
    let (command_tx, command_rx) = mpsc::channel(4);
    let (event_tx, mut event_rx) = mpsc::channel(4);
    let worker = tokio::spawn(peer_worker(
        7,
        addr,
        session,
        command_rx,
        event_tx,
        Arc::new(AtomicU64::new(0)),
    ));

    server
        .write_all(&PeerMessage::Unchoke.encode())
        .await
        .unwrap();
    match event_rx.recv().await.unwrap() {
        PeerEvent::Message {
            peer_id,
            addr: event_addr,
            msg,
        } => {
            assert_eq!(peer_id, 7);
            assert_eq!(event_addr, addr);
            assert_eq!(msg, PeerMessage::Unchoke);
        }
        other => panic!("unexpected event: {other:?}"),
    }

    command_tx.send(PeerCommand::Stop).await.unwrap();
    worker.await.unwrap();
}

fn test_pool_peer(addr: SocketAddrV4) -> (PeerRuntime, mpsc::Receiver<PeerCommand>) {
    let (commands, receiver) = mpsc::channel(8);
    (
        PeerRuntime {
            addr,
            min_piece: 7,
            max_piece: 8,
            unchoked_peer: false,
            produced_output: false,
            seen_ids: HashSet::new(),
            commands,
            worker: tokio::spawn(std::future::pending()),
        },
        receiver,
    )
}

#[tokio::test]
async fn timed_out_piece_retries_when_all_peer_slots_are_full() {
    let policy = LiveRecoveryConfig {
        max_piece_advance: 1,
        ..default_live_recovery()
    };
    let (mut c, _) = Continuity::fresh(&info(), 7, 7, 0, policy);
    let addr = "127.0.0.1:1".parse().unwrap();
    let (peer, mut commands) = test_pool_peer(addr);
    let mut peers = BTreeMap::from([(1, peer)]);
    c.register_active_peer(1, addr, live_pos(7, 7));
    c.set_peer_unchoked(1, true);
    advance_pool_requests(&mut peers, &mut c, 32).await;
    assert!(matches!(
        commands.try_recv(),
        Ok(PeerCommand::RequestPiece { piece: 7, .. })
    ));
    let now = Instant::now() + policy.request_timeout();
    retransmit_stalled_requests(&mut peers, &mut c, 32, now).await;
    let retry = commands.try_recv();
    shutdown_peer_runtimes(&mut peers);
    assert!(
        matches!(retry, Ok(PeerCommand::RequestPiece { piece: 7, .. })),
        "timeout must queue a retry even when every peer was at capacity"
    );
}

#[tokio::test]
async fn timed_out_piece_still_prefers_a_peer_with_spare_capacity() {
    let policy = LiveRecoveryConfig {
        max_piece_advance: 1,
        ..default_live_recovery()
    };
    let (mut c, _) = Continuity::fresh(&info(), 7, 7, 0, policy);
    let addr = "127.0.0.1:1".parse().unwrap();
    let (peer, mut original) = test_pool_peer(addr);
    let mut peers = BTreeMap::from([(1, peer)]);
    c.register_active_peer(1, addr, live_pos(7, 7));
    c.set_peer_unchoked(1, true);
    advance_pool_requests(&mut peers, &mut c, 32).await;
    original.try_recv().unwrap();
    let (peer, mut faster) = test_pool_peer(addr);
    peers.insert(2, peer);
    c.register_active_peer(2, addr, live_pos(7, 7));
    c.set_peer_unchoked(2, true);
    retransmit_stalled_requests(
        &mut peers,
        &mut c,
        32,
        Instant::now() + policy.request_timeout(),
    )
    .await;
    let retry = faster.try_recv();
    let unexpected = original.try_recv();
    shutdown_peer_runtimes(&mut peers);
    assert!(matches!(
        retry,
        Ok(PeerCommand::RequestPiece { piece: 7, .. })
    ));
    assert!(
        unexpected.is_err(),
        "normal retry should still prefer spare peer capacity"
    );
    assert_eq!(
        c.active_peers.in_flight_count(1),
        1,
        "original late response stays tracked"
    );
}

#[tokio::test]
async fn rejected_piece_releases_duplicate_assignments_without_dropping_other_requests() {
    for rejecting_peer_choked in [true, false] {
        let policy = LiveRecoveryConfig {
            max_piece_advance: 2,
            ..default_live_recovery()
        };
        let (mut c, _) = Continuity::fresh(&info(), 7, 8, 1, policy);
        let addr = "127.0.0.1:1".parse().unwrap();
        let (peer, mut original) = test_pool_peer(addr);
        let mut peers = BTreeMap::from([(1, peer)]);
        c.register_active_peer(1, addr, live_pos(7, 8));
        c.set_peer_unchoked(1, true);
        advance_pool_requests(&mut peers, &mut c, 32).await;
        original.try_recv().unwrap(); // piece 7
        original.try_recv().unwrap(); // unrelated piece 8
        let (peer, mut rejecting) = test_pool_peer(addr);
        peers.insert(2, peer);
        c.register_active_peer(2, addr, live_pos(7, 7));
        c.set_peer_unchoked(2, true);
        let now = Instant::now() + policy.request_timeout();
        c.requested_at.insert(8, now);
        retransmit_stalled_requests(&mut peers, &mut c, 32, now).await;
        assert!(matches!(
            rejecting.try_recv(),
            Ok(PeerCommand::RequestPiece { piece: 7, .. })
        ));
        assert_eq!(c.active_peers.in_flight_count(1), 2);
        if rejecting_peer_choked {
            c.set_peer_unchoked(2, false);
        } else {
            c.update_peer_window(2, 8, 9);
        }
        c.received_chunks.insert(7, HashSet::from([0]));
        c.received_chunks.insert(8, HashSet::from([0]));
        c.release_rejected_piece(7);
        assert!(!c.received_chunks.contains_key(&7));
        assert!(!c.requested_at.contains_key(&7));
        assert_eq!(c.received_chunks.get(&8).unwrap().len(), 1);
        assert_eq!(c.requested_at.get(&8), Some(&now));
        advance_pool_requests(&mut peers, &mut c, 32).await;
        let retry = original.try_recv();
        let unexpected = rejecting.try_recv();
        shutdown_peer_runtimes(&mut peers);
        assert!(matches!(
            retry,
            Ok(PeerCommand::RequestPiece { piece: 7, .. })
        ));
        assert!(unexpected.is_err());
        assert_eq!(
            c.active_peers.in_flight_count(1),
            2,
            "piece 8 remains outstanding"
        );
        assert_eq!(
            c.active_peers.in_flight_count(2),
            0,
            "all rejected copies must release"
        );
    }
}

#[test]
fn rejected_malformed_block_resets_partial_bytes_with_chunk_counts() {
    let mut stream = info();
    stream.piece_length = 8;
    stream.chunk_length = 4;
    stream.sig_len = 0;
    let (mut c, _) = Continuity::fresh(&stream, 7, 7, 0, default_live_recovery());
    c.reasm.add_block(7, 0, &[1, 2, 3, 4]).unwrap();
    assert!(!c.note_chunk(7, 0, 2));
    assert!(c.reasm.add_block(7, 8, &[9]).is_err());
    c.release_rejected_piece(7);
    assert_eq!(c.reasm.next_needed(), 7);
    c.reasm.add_block(7, 4, &[5, 6, 7, 8]).unwrap();
    assert!(!c.note_chunk(7, 1, 2));
    assert!(
        c.reasm.take_ready().is_empty(),
        "retry must not use discarded partial bytes"
    );
    c.reasm.add_block(7, 0, &[1, 2, 3, 4]).unwrap();
    assert!(c.note_chunk(7, 0, 2));
    assert_eq!(c.reasm.take_ready(), vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

#[tokio::test]
async fn timed_out_piece_without_eligible_peer_retries_on_unchoke_and_keeps_partial_data() {
    let mut stream = info();
    stream.piece_length = 8;
    stream.chunk_length = 4;
    stream.sig_len = 0;
    let policy = LiveRecoveryConfig {
        max_piece_advance: 1,
        ..default_live_recovery()
    };
    let (mut c, _) = Continuity::fresh(&stream, 7, 7, 0, policy);
    let addr = "127.0.0.1:1".parse().unwrap();
    let (peer, mut commands) = test_pool_peer(addr);
    let mut peers = BTreeMap::from([(1, peer)]);
    c.register_active_peer(1, addr, live_pos(7, 7));
    c.set_peer_unchoked(1, true);
    advance_pool_requests(&mut peers, &mut c, 2).await;
    commands.try_recv().unwrap();
    c.reasm.add_block(7, 0, &[1, 2, 3, 4]).unwrap();
    assert!(!c.note_chunk(7, 0, 2));
    c.set_peer_unchoked(1, false);
    retransmit_stalled_requests(
        &mut peers,
        &mut c,
        2,
        Instant::now() + policy.request_timeout(),
    )
    .await;
    assert!(
        commands.try_recv().is_err(),
        "choked peer cannot receive retry requests"
    );
    assert_eq!(c.active_peers.in_flight_count(1), 0);
    assert!(!c.requested_at.contains_key(&7));
    assert_eq!(c.received_chunks.get(&7).unwrap().len(), 1);
    c.set_peer_unchoked(1, true);
    advance_pool_requests(&mut peers, &mut c, 2).await;
    let retry = commands.try_recv();
    shutdown_peer_runtimes(&mut peers);
    assert!(matches!(
        retry,
        Ok(PeerCommand::RequestPiece {
            piece: 7,
            chunks_per_piece: 2
        })
    ));
    c.reasm.add_block(7, 4, &[5, 6, 7, 8]).unwrap();
    assert!(c.note_chunk(7, 1, 2));
    assert_eq!(c.reasm.take_ready(), vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn active_peers_assign_missing_pieces_to_unchoked_peers_with_coverage() {
    let mut active = ActivePeers::new();
    let addr1: SocketAddrV4 = "1.2.3.4:8621".parse().unwrap();
    let addr2: SocketAddrV4 = "1.2.3.5:8621".parse().unwrap();
    active.insert(1, addr1, live_pos(100, 102));
    active.insert(2, addr2, live_pos(103, 105));
    active.set_unchoked(1, true);
    active.set_unchoked(2, true);
    let mut scheduler = Scheduler::new(2);

    let assigned = active.assign(&mut scheduler, 101, 105);

    assert_eq!(
        assigned,
        vec![
            PeerAssignment {
                peer_id: 1,
                piece: 101
            },
            PeerAssignment {
                peer_id: 1,
                piece: 102
            },
            PeerAssignment {
                peer_id: 2,
                piece: 103
            },
            PeerAssignment {
                peer_id: 2,
                piece: 104
            },
        ]
    );
    assert_eq!(active.in_flight_count(1), 2);
    assert_eq!(active.in_flight_count(2), 2);
}

#[test]
fn active_peer_drop_returns_its_in_flight_pieces_for_requeue() {
    let mut active = ActivePeers::new();
    let addr1: SocketAddrV4 = "1.2.3.4:8621".parse().unwrap();
    let addr2: SocketAddrV4 = "1.2.3.5:8621".parse().unwrap();
    active.insert(1, addr1, live_pos(100, 105));
    active.insert(2, addr2, live_pos(100, 105));
    active.set_unchoked(1, true);
    active.set_unchoked(2, true);
    let mut scheduler = Scheduler::new(4);
    let assigned = active.assign(&mut scheduler, 100, 105);
    assert!(assigned.iter().any(|a| a.peer_id == 1));

    let dropped = active.remove(1);
    for piece in &dropped {
        scheduler.on_drop(*piece);
    }

    assert!(!dropped.is_empty());
    assert_eq!(active.in_flight_count(1), 0);
    let reassigned = active.assign(&mut scheduler, 100, 105);
    assert!(
        reassigned.iter().any(|a| dropped.contains(&a.piece)),
        "dropped peer pieces should be eligible for reassignment"
    );
    assert!(reassigned.iter().all(|a| a.peer_id == 2));
}

#[test]
fn pool_refill_candidates_skip_active_and_cooling_peers() {
    let peers = addrs(&[1, 2, 3, 4]);
    let active: HashSet<SocketAddrV4> = addrs(&[2]).into_iter().collect();
    let mut candidates = SessionCandidates::default();
    for addr in peers {
        candidates.learn(addr, CandidateKind::Discovered);
    }
    candidates.failed(addrs(&[3])[0], Instant::now());
    assert_eq!(
        pool_refill_candidates(&candidates.eligible_discovered(Instant::now()), &active),
        addrs(&[1, 4])
    );
}

#[test]
fn peer_connect_stats_reports_failure_classes() {
    let mut stats = PeerConnectStats::default();
    stats.record_failure(PeerConnectFailure {
        addr: addrs(&[1])[0],
        stage: PeerConnectStage::Tcp,
        timing: PeerConnectTiming::default(),
    });
    stats.record_failure(PeerConnectFailure {
        addr: addrs(&[2])[0],
        stage: PeerConnectStage::Handshake,
        timing: PeerConnectTiming::default(),
    });
    stats.record_failure(PeerConnectFailure {
        addr: addrs(&[3])[0],
        stage: PeerConnectStage::Window,
        timing: PeerConnectTiming::default(),
    });
    stats.record_failure(PeerConnectFailure {
        addr: addrs(&[4])[0],
        stage: PeerConnectStage::Window,
        timing: PeerConnectTiming::default(),
    });

    assert_eq!(stats.summary(), "attempted=4 tcp=1 handshake=1 window=2");
}

#[test]
fn newly_discovered_refill_candidates_are_deduped_against_known_peers() {
    let mut candidates = SessionCandidates::default();
    for addr in addrs(&[1, 2]) {
        candidates.learn(addr, CandidateKind::Discovered);
    }
    let before = candidates.all();
    for addr in addrs(&[2, 3, 1, 4, 3]) {
        candidates.learn(addr, CandidateKind::Discovered);
    }
    let after = candidates.all();
    assert_eq!(
        after
            .iter()
            .copied()
            .filter(|addr| !before.contains(addr))
            .collect::<Vec<_>>(),
        addrs(&[3, 4])
    );
    assert_eq!(after.len(), 4);
}

#[test]
fn background_discovery_options_leave_stale_timer_margin() {
    let opts = background_discovery_options();
    assert!(opts.peer_target > 8);
    assert!(opts.dht_budget < default_live_recovery().stale_upstream_timeout());
}

#[test]
fn upstream_selection_prefers_the_freshest_live_head() {
    let stale = WireLivePosition {
        min_piece: 100,
        max_piece: 120,
        position: 120,
        distance_from_source: 1,
    };
    let fresh = WireLivePosition {
        min_piece: 105,
        max_piece: 124,
        position: 124,
        distance_from_source: 3,
    };

    assert!(
        prefer_window(&fresh, &stale),
        "a newer advertised head should beat a lower distance value"
    );
    assert!(!prefer_window(&stale, &fresh));
}

#[test]
fn upstream_selection_uses_distance_as_a_tiebreaker() {
    let near = WireLivePosition {
        min_piece: 100,
        max_piece: 120,
        position: 120,
        distance_from_source: 1,
    };
    let far = WireLivePosition {
        min_piece: 90,
        max_piece: 120,
        position: 120,
        distance_from_source: 4,
    };

    assert!(prefer_window(&near, &far));
    assert!(!prefer_window(&far, &near));
}

#[test]
fn recovery_channel_capacities_are_checked() {
    let policy = LiveRecoveryConfig {
        max_active_upstreams: 2,
        ..LiveRecoveryConfig::default()
    };
    assert_eq!(recovery_channel_capacities(policy).unwrap(), (64, 2));

    let invalid = LiveRecoveryConfig {
        max_active_upstreams: usize::MAX,
        ..LiveRecoveryConfig::default()
    };
    assert!(recovery_channel_capacities(invalid).is_err());
}

#[tokio::test]
async fn full_command_queue_defers_requests_and_sweep_retries_without_new_events() {
    let (mut c, _) = Continuity::fresh(&info(), 0, 100, 100, default_live_recovery());
    let addr = "127.0.0.1:1".parse().unwrap();
    let (peer, mut commands) = test_pool_peer(addr);
    for _ in 0..8 {
        peer.commands
            .try_send(PeerCommand::Send(PeerMessage::KeepAlive))
            .unwrap();
    }
    let mut peers = BTreeMap::from([(1, peer)]);
    c.register_active_peer(1, addr, live_pos(0, 100));
    c.set_peer_unchoked(1, true);
    assert!(advance_pool_requests(&mut peers, &mut c, 2)
        .await
        .is_empty());
    assert!(c.requested_at.is_empty());
    assert_eq!(c.active_peers.in_flight_count(1), 0);
    for _ in 0..8 {
        commands.try_recv().unwrap();
    }
    retransmit_stalled_requests(&mut peers, &mut c, 2, Instant::now()).await;
    assert!(matches!(
        commands.try_recv(),
        Ok(PeerCommand::RequestPiece { piece: 0, .. })
    ));
    shutdown_peer_runtimes(&mut peers);
}

#[tokio::test]
async fn saturated_control_queue_drops_peer_without_blocking_pool() {
    let (mut c, _) = Continuity::fresh(&info(), 0, 0, 0, default_live_recovery());
    let addr = "127.0.0.1:1".parse().unwrap();
    let (peer, _commands) = test_pool_peer(addr);
    for _ in 0..8 {
        peer.commands
            .try_send(PeerCommand::Send(PeerMessage::KeepAlive))
            .unwrap();
    }
    let mut peers = BTreeMap::from([(1, peer)]);
    assert_eq!(
        send_peer_command(1, PeerMessage::Unchoke, &mut peers, &mut c).await,
        Some(addr)
    );
    assert!(peers.is_empty());
}

#[test]
fn request_head_keeps_the_saturating_upper_bound_exclusive() {
    let (mut c, _) = Continuity::fresh(&info(), 0, 100, 100, default_live_recovery());
    c.head = u64::MAX;
    c.reasm.skip_to(u64::MAX - 1);
    assert_eq!(c.request_head(), u64::MAX - 1);
    assert!(c.reasm.add_block(u64::MAX, 0, b"data").is_err());
}

#[test]
fn peer_distance_ordering_handles_minimum_signed_distance() {
    let mut preferred = live_pos(0, 1);
    preferred.distance_from_source = i64::MIN;
    let other = live_pos(0, 1);
    assert!(prefer_window(&preferred, &other));
    assert!(!prefer_window(&other, &preferred));
}

fn runtime_peer(min_piece: u64, max_piece: u64) -> PeerRuntime {
    let (commands, _receiver) = mpsc::channel(1);
    PeerRuntime {
        addr: test_addr(),
        min_piece,
        max_piece,
        unchoked_peer: false,
        produced_output: false,
        seen_ids: HashSet::new(),
        commands,
        worker: tokio::spawn(std::future::pending()),
    }
}

#[tokio::test]
async fn catching_up_peer_have_can_serve_without_advancing_global_head() {
    let (mut c, _) = Continuity::fresh(&info(), 100, 200, 8, default_live_recovery());
    c.register_active_peer(1, test_addr(), win(100, 150));
    c.set_peer_unchoked(1, true);
    let mut peers = BTreeMap::from([(1, runtime_peer(100, 150))]);
    assert!(!c.active_peers.any_unchoked_covers(c.reasm.next_needed()));

    assert!(observe_runtime_have(&mut peers, &mut c, 1, 195));
    assert!(!observe_runtime_have(&mut peers, &mut c, 1, 190));
    assert_eq!(c.head, 200);
    let requests = schedule_piece_assignments(
        &mut c.scheduler,
        &mut c.active_peers,
        c.reasm.next_needed(),
        c.head,
    );
    assert_eq!(
        requests.first(),
        Some(&PeerAssignment {
            peer_id: 1,
            piece: 192
        })
    );
    shutdown_peer_runtimes(&mut peers);
}

#[tokio::test]
async fn live_eviction_pointer_releases_a_stranded_cursor() {
    let (mut c, _) = Continuity::fresh(&info(), 100, 110, 8, default_live_recovery());
    c.register_active_peer(1, test_addr(), win(100, 110));
    c.set_peer_unchoked(1, true);
    let mut peers = BTreeMap::from([(1, runtime_peer(100, 110))]);
    let now = c.next_needed_since + default_live_recovery().request_timeout();
    assert_eq!(c.skip_evicted_gap(now), None);

    observe_runtime_eviction(&mut peers, &mut c, 1, 105);
    observe_runtime_eviction(&mut peers, &mut c, 1, 101);
    assert_eq!(c.skip_evicted_gap(now), Some(105));
    assert_eq!(c.reasm.next_needed(), 105);
    shutdown_peer_runtimes(&mut peers);
}

#[tokio::test]
async fn full_peer_window_updates_availability_below_global_head() {
    let (mut c, _) = Continuity::fresh(&info(), 100, 200, 8, default_live_recovery());
    c.register_active_peer(1, test_addr(), win(100, 150));
    c.set_peer_unchoked(1, true);
    let mut peers = BTreeMap::from([(1, runtime_peer(100, 150))]);
    assert_eq!(
        observe_runtime_window_payload(
            &mut peers,
            &mut c,
            1,
            b"d9:max_piecei195e9:min_piecei180ee",
        ),
        Some(true),
    );
    assert!(c.active_peers.any_unchoked_covers(192));
    assert!(!c.active_peers.any_unchoked_covers(170));
    assert_eq!(c.head, 200);
    assert_eq!(
        observe_runtime_window_payload(
            &mut peers,
            &mut c,
            1,
            b"d9:max_piecei170e9:min_piecei100ee"
        ),
        Some(false),
    );
    assert_eq!((peers[&1].min_piece, peers[&1].max_piece), (180, 195));
    assert_eq!(
        observe_runtime_window_payload(
            &mut peers,
            &mut c,
            1,
            b"d2:mid9:max_piecei210e9:min_piecei185eee"
        ),
        Some(true),
    );
    assert_eq!(c.head, 210);
    for invalid in [b"not-a-window".as_slice(), b"d9:max_piecei4294967296ee"] {
        assert_eq!(
            observe_runtime_window_payload(&mut peers, &mut c, 1, invalid),
            None
        );
    }
    assert_eq!(c.head, 210);
    assert_eq!((peers[&1].min_piece, peers[&1].max_piece), (185, 210));
    shutdown_peer_runtimes(&mut peers);
}

#[tokio::test]
async fn closed_old_owner_preserves_admitted_retry_and_does_not_duplicate_next_tick() {
    let policy = LiveRecoveryConfig {
        max_piece_advance: 2,
        ..default_live_recovery()
    };
    let (mut c, _) = Continuity::fresh(&info(), 7, 7, 1, policy);
    let old_addr = "127.0.0.1:1".parse().unwrap();
    let (old, mut old_commands) = test_pool_peer(old_addr);
    let mut peers = BTreeMap::from([(2, old)]);
    c.register_active_peer(2, old_addr, live_pos(7, 7));
    c.set_peer_unchoked(2, true);
    assert!(advance_pool_requests(&mut peers, &mut c, 1)
        .await
        .is_empty());
    assert!(matches!(
        old_commands.try_recv(),
        Ok(PeerCommand::RequestPiece { piece: 7, .. })
    ));
    drop(old_commands); // Closed command queue, but old piece 7 ownership remains.

    let new_addr = "127.0.0.1:2".parse().unwrap();
    let (replacement, mut new_commands) = test_pool_peer(new_addr);
    peers.insert(1, replacement);
    c.register_active_peer(1, new_addr, live_pos(7, 8));
    c.set_peer_unchoked(1, true);
    c.update_peer_window(2, 7, 8);
    c.head = 8;
    c.scheduler.on_drop(7);
    c.requested_at.remove(&7);
    // Spare capacity selects replacement for 7, then discovers old's Closed queue
    // while requesting 8 and falls back to replacement. Dropping old must preserve 7.
    assert_eq!(
        advance_pool_requests(&mut peers, &mut c, 1).await,
        vec![old_addr]
    );
    for wanted in [7, 8] {
        assert!(matches!(
            new_commands.try_recv(),
            Ok(PeerCommand::RequestPiece { piece, .. }) if piece == wanted
        ));
    }
    assert_eq!(c.active_peers.in_flight_count(1), 2);
    assert_eq!(c.active_peers.in_flight_count(2), 0);
    assert_eq!(c.scheduler.in_flight_count(), 2);
    let retry_at = c.requested_at[&7];
    assert!(advance_pool_requests(&mut peers, &mut c, 1)
        .await
        .is_empty());
    assert!(
        new_commands.try_recv().is_err(),
        "live retry must not be dispatched twice"
    );
    assert_eq!(c.requested_at[&7], retry_at);

    // Removing the final owner still frees both pieces for a later replacement.
    assert_eq!(drop_peer_runtime(1, &mut peers, &mut c), Some(new_addr));
    assert_eq!(c.scheduler.in_flight_count(), 0);
    assert!(!c.active_peers.any_in_flight(7));
    assert!(!c.active_peers.any_in_flight(8));
}
