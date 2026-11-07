use super::*;

fn fixture() -> (StreamInfo, SeedConfig, Continuity) {
    let info = StreamInfo {
        protection: None,
        infohash: [0; 20],
        piece_length: 752,
        chunk_length: 752,
        trackers: vec![],
        metadata: Default::default(),
        sig_len: 0,
        source_pubkey: vec![],
    };
    let seed = SeedConfig {
        metrics: None,
        registry: SeedRegistry::new(),
        store_factory: test_store_factory(4096, None),
        enabled: true,
        prefetch_pieces: 0,
        live_recovery: LiveRecoveryConfig::default(),
        warm_peers: WarmPeerCache::memory(),
    };
    let c = Continuity::fresh(&info, 7, 7, 0, seed.live_recovery).0;
    (info, seed, c)
}
fn media(marker: u8) -> Vec<u8> {
    let mut packet = [marker; 188];
    packet[..4].copy_from_slice(&[0x47, 0x01, 0, 0x10]);
    packet.repeat(4)
}
fn runtime(id: u64) -> (PeerRuntime, mpsc::Receiver<PeerCommand>) {
    let (commands, rx) = mpsc::channel(8);
    (
        PeerRuntime {
            addr: SocketAddrV4::new([127, 0, 0, 1].into(), id as u16),
            min_piece: 7,
            max_piece: 8,
            unchoked_peer: false,
            produced_output: false,
            seen_ids: HashSet::new(),
            commands,
            worker: tokio::spawn(std::future::pending()),
        },
        rx,
    )
}

#[tokio::test]
async fn one_piece_output_retains_order_credit_and_bytes_across_cancelled_reservation() {
    let (info, seed, mut c) = fixture();
    let first = media(0x21);
    let second = media(0x22);
    c.reasm.add_block(7, 0, &first).unwrap();
    c.reasm.add_block(8, 0, &second).unwrap();
    let (peer, _rx) = runtime(1);
    let addr = peer.addr;
    let mut peers = BTreeMap::from([(1, peer)]);
    c.piece_producers.insert(7, Some(addr));
    let mut candidates = SessionCandidates::default();
    let downloaded = Arc::new(AtomicU64::new(0));
    let (tx, mut output) = mpsc::channel(1);
    tx.send(LiveOutput {
        bytes: Bytes::from_static(b"occupied"),
        discontinuity: false,
        startup_complete: false,
    })
    .await
    .unwrap();
    assert!(prepare_output(&mut c, &info, &seed));
    assert_eq!(c.reasm.next_needed(), 8);
    assert!(
        c.reasm.has_complete_piece(8),
        "only one accepted piece leaves reassembly"
    );
    assert_eq!(c.emitted, 0);
    assert_eq!(downloaded.load(Ordering::Relaxed), 0);
    assert!(!peers[&1].produced_output);
    let mut reservation = Box::pin(tx.clone().reserve_owned());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(
            reservation.as_mut().poll(cx).is_pending()
        ))
        .await
    );
    drop(reservation);
    assert!(c.pending_output.is_some());
    // Pool generation can disappear while its pending media remains session-owned.
    drop(peers.remove(&1));
    assert_eq!(output.recv().await.unwrap().bytes.as_ref(), b"occupied");
    let permit = tx.clone().reserve_owned().await.unwrap();
    finish_output(
        &mut c,
        &info,
        permit,
        &downloaded,
        &seed,
        &mut candidates,
        &mut peers,
    );
    // Existing TS resynchronization retains one packet as synchronization lookahead.
    assert_eq!(output.recv().await.unwrap().bytes.as_ref(), &first[..564]);
    assert_eq!(downloaded.load(Ordering::Relaxed), 564);
    assert!(prepare_output(&mut c, &info, &seed));
    finish_output(
        &mut c,
        &info,
        tx.clone().reserve_owned().await.unwrap(),
        &downloaded,
        &seed,
        &mut candidates,
        &mut peers,
    );
    assert_eq!(
        output.recv().await.unwrap().bytes.as_ref(),
        [&first[564..], &second[..564]].concat()
    );
    assert_eq!(downloaded.load(Ordering::Relaxed), 1316);
    assert!(c.pending_output.is_none());
    assert!(!prepare_output(&mut c, &info, &seed));
}

#[tokio::test]
async fn reciprocal_queue_is_bounded_fair_and_discards_old_peer_jobs() {
    let store = Arc::new(tokio::sync::Mutex::new(PieceStore::new(4, 2, 4096)));
    store
        .lock()
        .await
        .put_chunk_with_header(7, 0, [1; 8], b"ab");
    let (one, _one_rx) = runtime(1);
    let (two, _two_rx) = runtime(2);
    let mut peers = BTreeMap::from([(1, one), (2, two)]);
    let PeerMessage::Unknown { payload, .. } = chunk_request(7, 0) else {
        panic!("request");
    };
    let mut work = Reciprocal::default();
    for _ in 0..100 {
        work.queue(1, &payload);
    }
    work.queue(2, &payload);
    assert_eq!(work.queues[&1].len(), 64);
    let held = store.lock().await;
    work.start(&peers, &store);
    let mut next = Box::pin(work.next());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(next.as_mut().poll(cx).is_pending()))
            .await
    );
    drop(next);
    drop(held);
    let (id, reply) = work.next().await;
    assert_eq!(id, 1);
    assert_eq!(reply.unwrap().media_bytes, 2);
    work.start(&peers, &store);
    let (id, reply) = work.next().await;
    assert_eq!(id, 2, "round robin visits second peer before first backlog");
    assert!(reply.is_some());
    work.start(&peers, &store);
    assert!(work.has_job());
    drop(peers.remove(&1));
    work.start(&peers, &store);
    assert!(!work.has_job());
    assert!(work.queues.is_empty());
}

#[tokio::test]
async fn full_output_keeps_real_pool_peer_control_and_cancellation_live() {
    let (info, seed, mut c) = fixture();
    c.reasm.add_block(7, 0, &media(0x31)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    let client = TcpStream::connect(target).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let std::net::SocketAddr::V4(addr) = target else {
        unreachable!()
    };
    let (tx, mut output) = mpsc::channel(1);
    tx.send(LiveOutput {
        bytes: Bytes::from_static(b"occupied"),
        discontinuity: false,
        startup_complete: false,
    })
    .await
    .unwrap();
    let task = tokio::spawn(async move {
        let mut c = Some(c);
        let result = follow_peer_pool(
            vec![ConnectedUpstream {
                session: PeerSession::new(client),
                addr,
                window: WireLivePosition {
                    min_piece: 7,
                    max_piece: 7,
                    position: -1,
                    distance_from_source: 1,
                },
                unchoked: false,
                yourip: None,
            }],
            &info,
            &Identity::generate(),
            1,
            &tx,
            &Arc::new(AtomicU64::new(0)),
            &Arc::new(AtomicU64::new(0)),
            &Arc::new(AtomicU32::new(0)),
            &seed,
            &Arc::new(tokio::sync::Mutex::new(PieceStore::new(752, 752, 4096))),
            &mut c,
            vec![],
            vec![],
            completed_discovery(|_| Box::pin(async { vec![] })),
            &mut SessionCandidates::default(),
            &Arc::new(AtomicU32::new(0)),
            None,
        )
        .await;
        (result, c)
    });
    let mut peer = PeerSession::new(server).with_timeout(Duration::from_secs(2));
    assert!(matches!(
        peer.read_message().await.unwrap(),
        PeerMessage::Extended { .. }
    ));
    assert_eq!(peer.read_message().await.unwrap(), PeerMessage::Interested);
    peer.send(&PeerMessage::Interested).await.unwrap();
    assert_eq!(
        peer.read_message().await.unwrap(),
        PeerMessage::Unchoke,
        "control must run while output has no capacity"
    );
    assert_eq!(output.recv().await.unwrap().bytes.as_ref(), b"occupied");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), output.recv())
            .await
            .unwrap()
            .unwrap()
            .bytes
            .as_ref(),
        &media(0x31)[..564]
    );
    drop(output);
    let (end, c) = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(end, FollowEnd::ConsumerGone));
    assert_eq!(c.unwrap().emitted, 564);
}

#[tokio::test]
async fn pending_first_output_still_requires_gap_marker_after_head_jump() {
    let (info, seed, mut c) = fixture();
    c.reasm.add_block(7, 0, &media(0x41)).unwrap();
    assert!(prepare_output(&mut c, &info, &seed));
    assert_eq!(c.emitted, 0);
    c.head = 20;
    assert_eq!(c.skip_far_behind_live(Instant::now()), Some(20));
    assert!(
        c.output_gate_armed(),
        "retained pre-gap output is already a playback commitment"
    );
    assert!(c.discontinuity_pending);
    let (tx, mut output) = mpsc::channel(1);
    let downloaded = Arc::new(AtomicU64::new(0));
    let mut candidates = SessionCandidates::default();
    let mut peers = BTreeMap::new();
    finish_output(
        &mut c,
        &info,
        tx.clone().reserve_owned().await.unwrap(),
        &downloaded,
        &seed,
        &mut candidates,
        &mut peers,
    );
    let prefix = output.recv().await.unwrap();
    assert!(!prefix.discontinuity);
    assert_eq!(prefix.bytes.as_ref(), &media(0x41)[..564]);
    // Force the existing bounded fallback to one packet; no arbitrary keyframe fixture.
    c.output_gate = ace_media::mpegts::KeyframeGate::with_max_scan_packets(1);
    c.reasm.add_block(20, 0, &media(0x42)).unwrap();
    assert!(prepare_output(&mut c, &info, &seed));
    finish_output(
        &mut c,
        &info,
        tx.clone().reserve_owned().await.unwrap(),
        &downloaded,
        &seed,
        &mut candidates,
        &mut peers,
    );
    assert!(
        output.recv().await.unwrap().discontinuity,
        "first post-gap batch must carry the boundary"
    );
}

#[tokio::test]
async fn reciprocal_failed_enqueue_and_old_generation_never_receive_credit() {
    let (_, _, mut c) = fixture();
    let (peer, receiver) = runtime(1);
    let addr = peer.addr;
    let mut peers = BTreeMap::from([(1, peer)]);
    drop(receiver);
    let uploaded = Arc::new(AtomicU64::new(0));
    let served = Arc::new(AtomicU32::new(0));
    let reply = || {
        Some(crate::seed::protocol::PreparedReply {
            message: PeerMessage::KeepAlive,
            media_bytes: 7,
            _read_permit: None,
        })
    };
    assert_eq!(
        super::super::finish_reciprocal(1, reply(), &mut peers, &mut c, &uploaded, &served).await,
        Some(addr)
    );
    assert_eq!(uploaded.load(Ordering::Relaxed), 0);
    let (mut replacement, mut receiver) = runtime(2);
    replacement.addr = addr;
    peers.insert(2, replacement);
    assert_eq!(
        super::super::finish_reciprocal(1, reply(), &mut peers, &mut c, &uploaded, &served).await,
        None
    );
    assert!(receiver.try_recv().is_err());
    assert_eq!(uploaded.load(Ordering::Relaxed), 0);
    assert_eq!(served.load(Ordering::Relaxed), 0);
}
