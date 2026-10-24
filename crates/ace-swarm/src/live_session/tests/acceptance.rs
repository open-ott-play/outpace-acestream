use super::*;

fn seed_test_chunk(piece: u32, chunk: u16, header: u8, data: &[u8]) -> LiveChunk {
    LiveChunk {
        piece,
        chunk,
        piece_header: [header; 8],
        data: data.to_vec(),
    }
}

#[tokio::test]
async fn live_seed_only_publishes_authenticated_pieces_after_a_valid_retry() {
    let auth = ace_wire::live_auth::LiveSourceAuth::generate();
    let mut stream = info();
    stream.piece_length = 128;
    stream.chunk_length = 64;
    stream.sig_len = auth.signature_len();
    stream.source_pubkey = auth.pubkey_der();
    let payload = vec![0x5a; 128 - auth.signature_len()];
    let mut valid = payload.clone();
    valid.extend_from_slice(&auth.sign(&payload));
    let mut corrupt = valid.clone();
    corrupt[0] ^= 0xff;

    // Exercise the exact read/advertisement APIs used by reciprocal and inbound
    // seeders with both supported storage backends.
    for disk in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let store = if disk {
            PieceStore::new_disk(128, 64, 256, temp.path().join("verified-seed")).unwrap()
        } else {
            PieceStore::new(128, 64, 256)
        };
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let (mut c, _) = Continuity::fresh(&stream, 0, 0, 0, default_live_recovery());
        c.receive_live_chunk(
            &seed_test_chunk(0, 0, 9, &corrupt[..64]),
            64,
            &store,
            Instant::now(),
        )
        .await
        .unwrap();
        assert!(PieceStore::shared_chunk(&store, 0, 0).await.is_none());
        assert!(store.lock().await.have_pieces().is_empty());
        assert!(c
            .receive_live_chunk(
                &seed_test_chunk(0, 1, 9, &corrupt[64..]),
                64,
                &store,
                Instant::now()
            )
            .await
            .is_err());
        assert!(c.reasm.take_ready().is_empty());
        assert!(c.piece_headers.is_empty());
        assert!(store.lock().await.window().is_none());
        assert!(PieceStore::shared_block(&store, 0, 0, 128).await.is_none());

        // The first retry block must not combine with any rejected data or header.
        c.receive_live_chunk(
            &seed_test_chunk(0, 1, 0, &valid[64..]),
            64,
            &store,
            Instant::now(),
        )
        .await
        .unwrap();
        assert!(PieceStore::shared_chunk(&store, 0, 1).await.is_none());
        assert!(store.lock().await.have_pieces().is_empty());
        c.receive_live_chunk(
            &seed_test_chunk(0, 0, 7, &valid[..64]),
            64,
            &store,
            Instant::now(),
        )
        .await
        .unwrap();
        assert_eq!(store.lock().await.have_pieces(), vec![0]);
        assert_eq!(store.lock().await.piece_header(0), Some([7; 8]));
        assert_eq!(
            PieceStore::shared_block(&store, 0, 0, 128).await,
            Some(valid.clone())
        );
        assert!(c.piece_headers.is_empty());

        // A malicious duplicate cannot rewrite either accepted bytes or headers,
        // whether the piece is waiting for media output or already consumed.
        for drained in [false, true] {
            if drained {
                assert_eq!(c.reasm.take_ready(), payload);
            }
            c.receive_live_chunk(
                &seed_test_chunk(0, 0, 9, &corrupt[..64]),
                64,
                &store,
                Instant::now(),
            )
            .await
            .unwrap();
            assert_eq!(
                PieceStore::shared_block(&store, 0, 0, 128).await,
                Some(valid.clone())
            );
            assert_eq!(store.lock().await.piece_header(0), Some([7; 8]));
        }
    }
}

#[tokio::test]
async fn live_seed_provisional_state_obeys_the_reassembly_window() {
    let policy = LiveRecoveryConfig {
        max_reasm_pieces_ahead: 4,
        ..default_live_recovery()
    };
    let (mut c, _) = Continuity::fresh(&info(), 10, 10, 0, policy);
    let store = Arc::new(tokio::sync::Mutex::new(PieceStore::new(4, 2, 16)));
    for piece in 10..14 {
        c.receive_live_chunk(
            &seed_test_chunk(piece, 0, 7, &[1]),
            2,
            &store,
            Instant::now(),
        )
        .await
        .unwrap();
    }
    assert_eq!(c.piece_headers.len(), 4);
    for piece in [14, 100, u32::MAX] {
        assert!(c
            .receive_live_chunk(
                &seed_test_chunk(piece, 0, 7, &[1]),
                2,
                &store,
                Instant::now()
            )
            .await
            .is_err());
    }
    for chunk in [
        seed_test_chunk(14, 0, 7, &[]),
        seed_test_chunk(14, 0, 7, &[1, 2, 3]),
    ] {
        assert!(c
            .receive_live_chunk(&chunk, 2, &store, Instant::now())
            .await
            .is_err());
    }
    assert_eq!(c.piece_headers.len(), 4);
    assert!(store.lock().await.window().is_none());
    c.resume(test_addr(), 20, 21);
    assert!(c.piece_headers.is_empty());
    c.receive_live_chunk(&seed_test_chunk(10, 1, 9, &[2]), 2, &store, Instant::now())
        .await
        .unwrap();
    c.receive_live_chunk(&seed_test_chunk(20, 0, 9, &[]), 2, &store, Instant::now())
        .await
        .unwrap();
    assert!(c.piece_headers.is_empty());
    assert!(store.lock().await.window().is_none());
}

#[tokio::test]
async fn live_seed_unsigned_streams_require_full_coverage_and_keep_cache_bounded() {
    let (mut c, _) = Continuity::fresh(&info(), 0, 0, 0, default_live_recovery());
    let store = Arc::new(tokio::sync::Mutex::new(PieceStore::new(4, 2, 4)));
    // All chunk indices with incomplete payloads still do not constitute a piece.
    for chunk in 0..2 {
        c.receive_live_chunk(
            &seed_test_chunk(0, chunk, 7, &[1]),
            2,
            &store,
            Instant::now(),
        )
        .await
        .unwrap();
    }
    assert!(store.lock().await.window().is_none());
    assert!(c
        .receive_live_chunk(
            &seed_test_chunk(0, u16::MAX, 9, &[1]),
            2,
            &store,
            Instant::now()
        )
        .await
        .is_err());
    for chunk in 0..2 {
        c.receive_live_chunk(
            &seed_test_chunk(0, chunk, 0, &[1, 2]),
            2,
            &store,
            Instant::now(),
        )
        .await
        .unwrap();
    }
    assert_eq!(store.lock().await.have_pieces(), vec![0]);
    assert_eq!(store.lock().await.piece_header(0), Some([7; 8]));
    assert_eq!(c.reasm.take_ready(), vec![1, 2, 1, 2]);
    for chunk in 0..2 {
        c.receive_live_chunk(
            &seed_test_chunk(1, chunk, 8, &[3, 4]),
            2,
            &store,
            Instant::now(),
        )
        .await
        .unwrap();
    }
    assert_eq!(store.lock().await.have_pieces(), vec![1]);
    assert!(PieceStore::shared_chunk(&store, 0, 0).await.is_none());
    assert_eq!(
        PieceStore::shared_block(&store, 1, 0, 4).await,
        Some(vec![3, 4, 3, 4])
    );
}

#[tokio::test]
async fn retries_refresh_only_for_new_bytes_and_completion_requires_full_coverage() {
    let (mut c, _) = Continuity::fresh(&info(), 0, 0, 0, default_live_recovery());
    let store = Arc::new(tokio::sync::Mutex::new(PieceStore::new(4, 2, 16)));
    let start = Instant::now();
    c.requested_at.insert(0, start);
    let first = start + Duration::from_secs(1);
    let accepted = c
        .receive_live_chunk(&seed_test_chunk(0, 0, 1, &[1]), 2, &store, first)
        .await
        .unwrap();
    assert!(accepted.contributed && accepted.progressed);
    assert_eq!(c.requested_at[&0], first);
    let duplicate = c
        .receive_live_chunk(
            &seed_test_chunk(0, 0, 2, &[2]),
            2,
            &store,
            first + Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(duplicate.contributed && !duplicate.progressed);
    assert_eq!(c.requested_at[&0], first);
    c.receive_live_chunk(&seed_test_chunk(0, 1, 1, &[3]), 2, &store, first)
        .await
        .unwrap();
    assert!(!c.note_chunk(0, 0, 2));
    assert!(!c.note_chunk(0, 1, 2));
    assert!(c.requested_at.contains_key(&0));
    assert!(store.lock().await.window().is_none());
    c.receive_live_chunk(&seed_test_chunk(0, 0, 1, &[1, 2]), 2, &store, first)
        .await
        .unwrap();
    c.receive_live_chunk(&seed_test_chunk(0, 1, 1, &[3, 4]), 2, &store, first)
        .await
        .unwrap();
    assert!(c.note_chunk(0, 1, 2));
    assert!(!c.requested_at.contains_key(&0));
    assert_eq!(c.reasm.take_ready(), vec![1, 2, 3, 4]);
}
