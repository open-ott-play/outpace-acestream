//! Synthetic production admission, reassembly, and completion-attribution regressions.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceCase {
    Clean,
    RetainedPartial,
    Mixed,
}

async fn next_owner_after_measured_completions(case: SourceCase) -> u64 {
    let info = StreamInfo {
        infohash: [0; 20],
        piece_length: 752,
        chunk_length: 376,
        trackers: vec![],
        metadata: Default::default(),
        sig_len: 0,
        source_pubkey: vec![],
    };
    let recovery = LiveRecoveryConfig {
        max_piece_advance: 1,
        ..LiveRecoveryConfig::default()
    };
    let (mut c, _) = Continuity::fresh(&info, 0, 1, 1, recovery);
    let store = Arc::new(tokio::sync::Mutex::new(PieceStore::new(752, 376, 4096)));
    let addr1: SocketAddrV4 = "127.0.0.1:1".parse().unwrap();
    let addr2: SocketAddrV4 = "127.0.0.1:2".parse().unwrap();
    let payload = [0x55; 376];

    if case == SourceCase::RetainedPartial {
        // A previous runtime at the SAME address contributed half the piece before
        // this generation admitted its request. Finishing it must not teach a speed.
        let chunk = LiveChunk::from_message(&build_piece(0, 1, 0, [0; 8], &payload)).unwrap();
        let at = Instant::now();
        let accepted = c.receive_live_chunk(&chunk, 376, &store, at).await.unwrap();
        record_accepted_contribution(&mut c, 99, addr1, 1, accepted, at);
    }

    let mut peers = BTreeMap::new();
    let mut receivers = BTreeMap::new();
    for (id, addr) in [(1, addr1), (2, addr2)] {
        c.register_active_peer(
            id,
            addr,
            WireLivePosition {
                min_piece: 0,
                max_piece: 1,
                position: -1,
                distance_from_source: 1,
            },
        );
        c.set_peer_unchoked(id, true);
        let (commands, receiver) = mpsc::channel(4);
        receivers.insert(id, receiver);
        peers.insert(
            id,
            PeerRuntime {
                addr,
                min_piece: 0,
                max_piece: 1,
                unchoked_peer: false,
                produced_output: false,
                seen_ids: HashSet::new(),
                commands,
                worker: tokio::spawn(async {}),
            },
        );
    }
    // Actual bounded command admission must establish timing and exclude retained data.
    assert!(advance_pool_requests(&mut peers, &mut c, 2)
        .await
        .is_empty());
    for (owner, expected_piece) in [(1, 1), (2, 0)] {
        assert!(matches!(
            receivers.get_mut(&owner).unwrap().try_recv().unwrap(),
            PeerCommand::RequestPiece { piece, .. } if piece == expected_piece
        ));
    }
    let base = Instant::now();
    for (owner, addr, piece, delay) in [(1, addr1, 1, 10), (2, addr2, 0, 100)] {
        for index in 0..2 {
            if case == SourceCase::RetainedPartial && piece == 1 && index == 0 {
                continue;
            }
            let chunk =
                LiveChunk::from_message(&build_piece(0, piece, index, [0; 8], &payload)).unwrap();
            let at = base + Duration::from_millis(delay);
            let accepted = c.receive_live_chunk(&chunk, 376, &store, at).await.unwrap();
            let source = if case == SourceCase::Mixed && piece == 1 && index == 0 {
                addr2
            } else {
                addr
            };
            record_accepted_contribution(&mut c, owner, source, u64::from(piece), accepted, at);
            c.note_chunk(u64::from(piece), index, 2);
        }
    }
    // An already-complete duplicate cannot refresh or replace a legitimate sample.
    let chunk = LiveChunk::from_message(&build_piece(0, 1, 1, [0; 8], &payload)).unwrap();
    let at = base + Duration::from_millis(200);
    let duplicate = c.receive_live_chunk(&chunk, 376, &store, at).await.unwrap();
    assert!(!duplicate.contributed);
    record_accepted_contribution(&mut c, 1, addr1, 1, duplicate, at);
    for id in [1, 2] {
        c.active_peers.update_window(id, 0, 2);
    }
    let next = c.active_peers.assign_if_at(
        &mut c.scheduler,
        2,
        2,
        at,
        recovery.request_timeout(),
        |_| true,
    );
    assert_eq!(next.len(), 1);
    for peer in peers.values_mut() {
        (&mut peer.worker).await.unwrap();
    }
    next[0].peer_id
}

#[tokio::test]
async fn accepted_exclusive_completion_refines_the_next_request() {
    assert_eq!(
        next_owner_after_measured_completions(SourceCase::Clean).await,
        1
    );
}

#[tokio::test]
async fn retained_partial_at_same_address_preserves_unknown_fallback() {
    assert_eq!(
        next_owner_after_measured_completions(SourceCase::RetainedPartial).await,
        2
    );
}

#[tokio::test]
async fn mixed_contributors_preserve_unknown_fallback() {
    assert_eq!(
        next_owner_after_measured_completions(SourceCase::Mixed).await,
        2
    );
}
