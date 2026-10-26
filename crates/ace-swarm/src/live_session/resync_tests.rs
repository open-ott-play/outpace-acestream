//! Synthetic transport regressions use the real provider output path.
use super::*;
use ace_media::mpegts::{ts_pid, TS_PACKET_LEN};

const PMT: u16 = 0x100;
const VIDEO: u16 = 0x101;

fn continuity() -> Continuity {
    let info = StreamInfo {
        infohash: [0; 20],
        piece_length: 4,
        chunk_length: 2,
        trackers: vec![],
        metadata: Default::default(),
        sig_len: 0,
        source_pubkey: vec![],
    };
    Continuity::fresh(
        &info,
        100,
        149,
        PREFETCH_PIECES,
        LiveRecoveryConfig::default(),
    )
    .0
}

fn packet(pid: u16, marker: u8, rai: bool) -> Vec<u8> {
    let mut p = vec![marker; TS_PACKET_LEN];
    p[..4].copy_from_slice(&[0x47, ((pid >> 8) as u8 & 0x1f) | 0x40, pid as u8, 0x10]);
    if rai {
        p[3] = 0x30;
        p[4] = 1;
        p[5] = 0x40;
    }
    p
}

fn psi(pid: u16, section: &[u8]) -> Vec<u8> {
    let mut p = packet(pid, 0xff, false);
    p[4] = 0;
    p[5..5 + section.len()].copy_from_slice(section);
    p
}

fn pat(pmt: u16) -> Vec<u8> {
    psi(
        0,
        &[
            0,
            0xb0,
            13,
            0,
            1,
            0xc1,
            0,
            0,
            0,
            1,
            0xe0 | (pmt >> 8) as u8,
            pmt as u8,
            0,
            0,
            0,
            0,
        ],
    )
}

fn pmt(pmt: u16, video: u16, codec: u8) -> Vec<u8> {
    psi(
        pmt,
        &[
            2,
            0xb0,
            18,
            0,
            1,
            0xc1,
            0,
            0,
            0xe0 | (video >> 8) as u8,
            video as u8,
            0xf0,
            0,
            codec,
            0xe0 | (video >> 8) as u8,
            video as u8,
            0xf0,
            0,
            0,
            0,
            0,
            0,
        ],
    )
}

fn markers(bytes: &[u8]) -> Vec<u8> {
    bytes
        .as_chunks::<TS_PACKET_LEN>()
        .0
        .iter()
        .filter(|p| ts_pid(p.as_slice()) == VIDEO && p[3] & 0x10 != 0)
        .map(|p| p[20])
        .collect()
}

fn output_bytes(output: Vec<LiveOutput>) -> Vec<u8> {
    output
        .into_iter()
        .flat_map(|chunk| chunk.bytes.to_vec())
        .collect()
}

fn prime(c: &mut Continuity) {
    let opening = [
        pat(PMT),
        pmt(PMT, VIDEO, 0x1b),
        packet(VIDEO, b'a', true),
        packet(VIDEO, b'b', false),
    ]
    .concat();
    assert_eq!(markers(&output_bytes(c.resync_output(&opening))), b"a");
}

#[test]
fn resync_handles_multiple_losses_in_one_batch_in_order() {
    let mut c = continuity();
    prime(&mut c);
    let mixed = [
        packet(VIDEO, b'c', false),
        vec![0; 8],
        packet(VIDEO, b'x', false),
        packet(VIDEO, b'r', true),
        packet(VIDEO, b'e', false),
        vec![0; 12],
        packet(VIDEO, b'z', false),
        packet(VIDEO, b't', true),
        packet(VIDEO, b'u', false),
    ]
    .concat();
    let chunks = c.resync_output(&mixed);
    assert_eq!(chunks.len(), 3);
    assert_eq!(markers(&chunks[0].bytes), b"bc");
    assert!(!chunks[0].discontinuity);
    assert_eq!(markers(&chunks[1].bytes), b"re");
    assert!(chunks[1].discontinuity);
    assert_eq!(markers(&chunks[2].bytes), b"t");
    assert!(chunks[2].discontinuity);
}

#[test]
fn resync_pending_losses_keep_cache_and_accumulate_withholding() {
    let mut c = continuity();
    prime(&mut c);
    let first = [
        vec![0; 8],
        packet(VIDEO, b'x', false),
        packet(VIDEO, b'y', false),
        packet(VIDEO, b'w', false),
    ]
    .concat();
    let prefix = c.resync_output(&first);
    assert_eq!(markers(&prefix[0].bytes), b"b");
    assert!(!prefix[0].discontinuity);
    let pending = c.transport_recovery.as_ref().unwrap();
    assert_eq!((pending.discarded_bytes, pending.withheld_bytes), (8, 376));
    let second = [
        vec![0; 12],
        packet(VIDEO, b'z', false),
        packet(VIDEO, b't', true),
        packet(VIDEO, b'u', false),
    ]
    .concat();
    let resumed = c.resync_output(&second);
    assert_eq!(resumed.len(), 1);
    assert_eq!(markers(&resumed[0].bytes), b"t");
    assert!(resumed[0].discontinuity);
    assert!(c.transport_recovery.is_none());
    // The operational resume event must report 20 discarded and 752 withheld bytes,
    // loss_events=2. This focused test's --nocapture output also verifies that accounting.
}

#[test]
fn resync_split_pushes_preserve_prefix_and_first_access_point() {
    let mixed = [
        packet(VIDEO, b'c', false),
        packet(VIDEO, b'd', false),
        vec![0; 8],
        packet(VIDEO, b'x', false),
        packet(VIDEO, b'r', true),
        packet(VIDEO, b's', false),
        packet(VIDEO, b't', false),
    ]
    .concat();
    for split in 0..=mixed.len() {
        let mut c = continuity();
        prime(&mut c);
        let mut chunks = c.resync_output(&mixed[..split]);
        chunks.extend(c.resync_output(&mixed[split..]));
        assert_eq!(
            chunks.iter().filter(|o| o.discontinuity).count(),
            1,
            "split={split}"
        );
        for chunk in &chunks {
            if chunk.discontinuity {
                assert_eq!(markers(&chunk.bytes)[0], b'r', "split={split}");
            }
        }
        assert_eq!(markers(&output_bytes(chunks)), b"bcdrs", "split={split}");
    }
}

#[test]
fn resync_tail_loss_arms_only_after_its_valid_prefix() {
    let mut c = continuity();
    prime(&mut c);
    let mixed = [packet(VIDEO, b'c', false), vec![0; 1024]].concat();
    let prefix = c.resync_output(&mixed);
    assert_eq!(markers(&output_bytes(prefix)), b"bc");
    assert!(c.output_gate_armed());
    let resumed = c.resync_output(
        &[
            packet(VIDEO, b'x', false),
            packet(VIDEO, b'r', true),
            packet(VIDEO, b's', false),
        ]
        .concat(),
    );
    assert_eq!(resumed.len(), 1);
    assert!(resumed[0].discontinuity);
    assert_eq!(markers(&resumed[0].bytes), b"r");
}

#[test]
fn resync_preserves_every_pre_loss_packet_in_mixed_batch() {
    let mut c = continuity();
    prime(&mut c);
    let mixed = [
        packet(VIDEO, b'c', false),
        packet(VIDEO, b'd', false),
        vec![0; 8],
        pat(PMT),
        pmt(PMT, VIDEO, 0x1b),
        packet(VIDEO, b'x', false),
        packet(VIDEO, b'r', true),
        packet(VIDEO, b's', false),
    ]
    .concat();
    assert_eq!(
        markers(&output_bytes(c.resync_output(&mixed))),
        b"bcdr",
        "complete pre-loss packets must precede first resumed RAI"
    );
}

#[test]
fn resync_uses_cached_psi_at_first_post_loss_rai() {
    let mut c = continuity();
    prime(&mut c);
    let mixed = [
        vec![0; 8],
        packet(VIDEO, b'x', false),
        packet(VIDEO, b'r', true),
        packet(VIDEO, b's', false),
    ]
    .concat();
    assert_eq!(
        markers(&output_bytes(c.resync_output(&mixed))),
        b"br",
        "no new PAT/PMT is needed to resume"
    );
}

#[test]
fn resync_synthetic_vector_keeps_prefix_and_resumes_without_new_psi() {
    let vector = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/vectors/media/transport-resync.ts"
    ));
    let mut c = continuity();
    let output = c.resync_output(vector);
    assert_eq!(output.len(), 2);
    assert_eq!(markers(&output[0].bytes), b"abcd");
    assert!(!output[0].discontinuity);
    assert_eq!(markers(&output[1].bytes), b"rs");
    assert!(output[1].discontinuity);
}

struct BufferedOutputFixture {
    info: StreamInfo,
    continuity: Continuity,
    seed: SeedConfig,
    store: Arc<tokio::sync::Mutex<PieceStore>>,
    producer: SocketAddrV4,
}

async fn buffered_output_fixture() -> BufferedOutputFixture {
    let auth = ace_wire::live_auth::LiveSourceAuth::generate();
    let payload = [
        pat(PMT),
        pmt(PMT, VIDEO, 0x1b),
        packet(VIDEO, b'a', true),
        packet(VIDEO, b'b', false),
        packet(VIDEO, b'c', false),
    ]
    .concat();
    let mut signed = payload.clone();
    signed.extend(auth.sign(&payload));
    let info = StreamInfo {
        infohash: [0; 20],
        piece_length: signed.len() as u64,
        chunk_length: signed.len() as u64,
        trackers: vec![],
        metadata: Default::default(),
        sig_len: auth.signature_len(),
        source_pubkey: auth.pubkey_der(),
    };
    let policy = LiveRecoveryConfig {
        max_active_upstreams: 1,
        request_timeout_ms: 20,
        request_check_interval_ms: 5,
        ..LiveRecoveryConfig::default()
    };
    let seed = SeedConfig {
        metrics: None,
        registry: SeedRegistry::new(),
        store_factory: test_store_factory(4096, None),
        enabled: false,
        prefetch_pieces: 1,
        live_recovery: policy,
        warm_peers: WarmPeerCache::new(std::path::Path::new(""), true),
    };
    let store = Arc::new(tokio::sync::Mutex::new(PieceStore::new(
        info.piece_length,
        info.chunk_length,
        4096,
    )));
    let (mut continuity, _) = Continuity::fresh(&info, 0, 1, 1, policy);
    let chunk = LiveChunk::from_message(&build_piece(0, 1, 0, [0; 8], &signed)).unwrap();
    let accepted = continuity
        .receive_live_chunk(&chunk, info.chunk_length, &store, Instant::now())
        .await
        .unwrap();
    assert!(accepted.contributed && accepted.progressed);
    assert!(continuity.note_chunk(1, 0, 1));
    assert_eq!(continuity.reasm.next_needed(), 0);
    assert!(continuity.reasm.has_complete_piece(1));
    // The retained data came from a different transport, not the announcing peer below.
    let producer = SocketAddrV4::new([127, 0, 0, 2].into(), 8621);
    continuity.piece_producers.insert(1, Some(producer));
    BufferedOutputFixture {
        info,
        continuity,
        seed,
        store,
        producer,
    }
}

async fn buffered_output_after_window_move(eviction: bool) {
    let fixture = buffered_output_fixture().await;
    let warm = fixture.seed.warm_peers.clone();
    let producer = fixture.producer;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = match listener.local_addr().unwrap() {
        std::net::SocketAddr::V4(addr) => addr,
        _ => unreachable!(),
    };
    let client = TcpStream::connect(addr).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (advance_tx, advance_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        let mut session = PeerSession::new(server);
        loop {
            if matches!(
                session.read_message().await.unwrap(),
                PeerMessage::Interested
            ) {
                break;
            }
        }
        ready_tx.send(()).unwrap();
        advance_rx.await.unwrap();
        let announcement = if eviction {
            let mut payload = vec![0; 4];
            payload.extend_from_slice(&1u32.to_be_bytes());
            PeerMessage::Unknown { id: 10, payload }
        } else {
            PeerMessage::Have(2)
        };
        session.send(&announcement).await.unwrap();
        // Deliberately no Piece (or any other message) after the cursor-moving event.
        // The complete, verified piece is already in the retained reassembler.
        finish_rx.await.unwrap();
    });
    let (tx, mut rx) = mpsc::channel(1);
    tasks.spawn(async move {
        let mut continuity = Some(fixture.continuity);
        let mut candidates = SessionCandidates::default();
        candidates.learn(addr, CandidateKind::Source);
        candidates.failed(addr, Instant::now() + Duration::from_secs(60));
        let end = follow_peer_pool(
            vec![ConnectedUpstream {
                session: PeerSession::new(client),
                addr,
                window: WireLivePosition {
                    min_piece: 0,
                    max_piece: 1,
                    position: -1,
                    distance_from_source: 1,
                },
                unchoked: true,
                yourip: None,
            }],
            &fixture.info,
            &Identity::generate(),
            1,
            &tx,
            &Arc::new(AtomicU64::new(0)),
            &Arc::new(AtomicU64::new(0)),
            &Arc::new(AtomicU32::new(0)),
            &fixture.seed,
            &fixture.store,
            &mut continuity,
            vec![],
            vec![],
            completed_discovery(|_| Box::pin(async { vec![] })),
            &mut candidates,
            &Arc::new(AtomicU32::new(0)),
            None,
        )
        .await;
        assert!(matches!(end, FollowEnd::ConsumerGone));
        assert_eq!(continuity.unwrap().reasm.next_needed(), 2);
        assert!(
            !candidates.eligible(Instant::now()).contains(&addr),
            "announcing another producer's retained bytes must not reset the sender's cooldown"
        );
    });
    tokio::time::timeout(Duration::from_secs(2), ready_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(
        rx.try_recv().is_err(),
        "the missing prefix must not be emitted"
    );
    advance_tx.send(()).unwrap();
    let output = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("window movement must release buffered output without another Piece")
        .unwrap();
    assert_eq!(markers(&output.bytes), b"ab");
    assert_eq!(output.discontinuity, eviction);
    assert!(output.startup_complete);
    assert!(
        tokio::time::timeout(Duration::from_millis(60), rx.recv())
            .await
            .is_err(),
        "repeated pool ticks must not emit the retained piece twice"
    );
    assert_eq!(warm.candidate_addresses(&[0; 20]), vec![producer]);
    drop(rx);
    tokio::time::timeout(Duration::from_secs(2), tasks.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    finish_tx.send(()).unwrap();
    tasks.join_next().await.unwrap().unwrap();
}

#[tokio::test]
async fn have_releases_buffered_authenticated_piece_without_new_piece_message() {
    buffered_output_after_window_move(false).await;
}

#[tokio::test]
async fn eviction_releases_buffered_authenticated_piece_without_new_piece_message() {
    buffered_output_after_window_move(true).await;
}

#[tokio::test]
async fn buffered_output_drain_preserves_backpressure_and_emits_once() {
    let mut fixture = buffered_output_fixture().await;
    fixture.continuity.head = 2;
    let downloaded = Arc::new(AtomicU64::new(0));
    let mut peers = BTreeMap::new();
    let mut candidates = SessionCandidates::default();
    let (tx, mut rx) = mpsc::channel(1);
    tx.send(LiveOutput {
        bytes: Bytes::from_static(b"occupied"),
        discontinuity: false,
        startup_complete: false,
    })
    .await
    .unwrap();
    {
        let drain = pool::drain_ready_output(
            &mut fixture.continuity,
            &fixture.info,
            &tx,
            &downloaded,
            &fixture.seed,
            &mut candidates,
            &mut peers,
        );
        tokio::pin!(drain);
        tokio::select! {
            biased;
            _ = &mut drain => panic!("full output channel must apply backpressure"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(downloaded.load(Ordering::Relaxed), 0);
        assert_eq!(rx.recv().await.unwrap().bytes.as_ref(), b"occupied");
        assert!(drain.await.unwrap());
    }
    let output = rx.recv().await.unwrap();
    assert_eq!(markers(&output.bytes), b"ab");
    assert_eq!(
        downloaded.load(Ordering::Relaxed),
        output.bytes.len() as u64
    );
    assert!(!pool::drain_ready_output(
        &mut fixture.continuity,
        &fixture.info,
        &tx,
        &downloaded,
        &fixture.seed,
        &mut candidates,
        &mut peers,
    )
    .await
    .unwrap());
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn buffered_output_drain_stops_workers_when_consumer_closes() {
    let mut fixture = buffered_output_fixture().await;
    fixture.continuity.head = 2;
    let downloaded = Arc::new(AtomicU64::new(0));
    let mut candidates = SessionCandidates::default();
    let (commands, _command_rx) = mpsc::channel(1);
    let (worker_owned, worker_dropped) = tokio::sync::oneshot::channel::<()>();
    let worker = tokio::spawn(async move {
        let _owned = worker_owned;
        std::future::pending::<()>().await;
    });
    let mut peers = BTreeMap::from([(
        0,
        PeerRuntime {
            addr: fixture.producer,
            min_piece: 0,
            max_piece: 2,
            unchoked_peer: false,
            produced_output: false,
            seen_ids: HashSet::new(),
            commands,
            worker,
        },
    )]);
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    assert!(pool::drain_ready_output(
        &mut fixture.continuity,
        &fixture.info,
        &tx,
        &downloaded,
        &fixture.seed,
        &mut candidates,
        &mut peers,
    )
    .await
    .is_err());
    assert!(peers.is_empty());
    assert_eq!(downloaded.load(Ordering::Relaxed), 0);
    assert!(fixture
        .seed
        .warm_peers
        .candidate_addresses(&[0; 20])
        .is_empty());
    assert!(tokio::time::timeout(Duration::from_secs(1), worker_dropped)
        .await
        .unwrap()
        .is_err());
}
