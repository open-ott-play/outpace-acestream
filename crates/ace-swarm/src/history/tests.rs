use super::*;
use ace_wire::live_auth::LiveSourceAuth;
use ace_wire::live_codec::build_piece;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

fn source() -> &'static LiveSourceAuth {
    static SOURCE: OnceLock<LiveSourceAuth> = OnceLock::new();
    SOURCE.get_or_init(LiveSourceAuth::generate)
}

fn info() -> StreamInfo {
    StreamInfo {
        infohash: [0; 20],
        piece_length: 1024,
        chunk_length: 256,
        trackers: vec![],
        metadata: Default::default(),
        sig_len: source().signature_len(),
        source_pubkey: source().pubkey_der(),
    }
}

fn limits() -> HistoryLimits {
    HistoryLimits {
        total_timeout: Duration::from_secs(3),
        connect_timeout: Duration::from_secs(1),
        piece_timeout: Duration::from_millis(250),
        max_peers: 4,
        max_bytes: 1024 * 1024,
    }
}

fn request() -> HistoryRequest {
    HistoryRequest {
        start_piece: 42,
        piece_count: 2,
    }
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    WrongStream,
    WrongPiece,
    ShortChunk,
    ChunkOutsidePiece,
    Corrupt(u32),
    HeaderConflict,
    InvalidTimestamp,
    DuplicateChunk,
    MissingPiece(u32),
    EarlyUnchoke,
    EarlyUnchokeThenChoke,
}

struct FakePeer {
    addr: SocketAddrV4,
    requests: Arc<Mutex<Vec<(u32, u16)>>>,
    task: JoinHandle<()>,
}

impl Drop for FakePeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn peer(min_piece: u32, max_piece: u32, fault: Fault) -> FakePeer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
        unreachable!()
    };
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    // Generate signing material outside timeout-sensitive network tasks.
    let descriptor = info();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut session = PeerSession::new(stream);
        session
            .accept_handshake(random_peer_id(), |hash| *hash == [0; 20])
            .await
            .unwrap();
        let window = HistoryPeerWindow {
            peer: addr,
            min_piece,
            max_piece,
        };
        let hello = outgoing(min_piece, window, addr);
        if matches!(fault, Fault::EarlyUnchoke | Fault::EarlyUnchokeThenChoke) {
            session.send(&PeerMessage::Unchoke).await.unwrap();
            if matches!(fault, Fault::EarlyUnchokeThenChoke) {
                session.send(&PeerMessage::Choke).await.unwrap();
            }
        }
        if session
            .send(&PeerMessage::Extended {
                ext_id: 0,
                payload: hello.encode_payload(),
            })
            .await
            .is_err()
        {
            return;
        }
        let mut signed = HashMap::new();
        while let Ok(message) = session.read_message().await {
            match message {
                PeerMessage::Interested => {
                    if matches!(fault, Fault::EarlyUnchoke | Fault::EarlyUnchokeThenChoke) {
                        // The peer has already announced its state before its
                        // window and does not repeat it in response to interest.
                        continue;
                    }
                    if session.send(&PeerMessage::Unchoke).await.is_err() {
                        return;
                    }
                }
                PeerMessage::Unknown { id: 6, payload } if payload.len() == 10 => {
                    assert_eq!(&payload[..4], &[0; 4]);
                    let piece = u32::from_be_bytes(payload[4..8].try_into().unwrap());
                    let mut chunk = u16::from_be_bytes(payload[8..10].try_into().unwrap());
                    recorded.lock().unwrap().push((piece, chunk));
                    if matches!(fault, Fault::MissingPiece(missing) if piece == missing) {
                        continue;
                    }
                    let data = signed.entry(piece).or_insert_with(|| {
                        let mut bytes = vec![
                            piece as u8;
                            descriptor.piece_length as usize - descriptor.sig_len
                        ];
                        let signature = source().sign(&bytes);
                        bytes.extend(signature);
                        if matches!(fault, Fault::Corrupt(bad) if bad == piece) {
                            bytes[0] ^= 1;
                        }
                        bytes
                    });
                    if matches!(fault, Fault::DuplicateChunk) && chunk == 2 {
                        chunk = 0;
                    }
                    let offset = usize::from(chunk) * descriptor.chunk_length as usize;
                    let mut data = data[offset..offset + descriptor.chunk_length as usize].to_vec();
                    let mut header = (1_700_000_000.0 + f64::from(piece)).to_be_bytes();
                    if matches!(fault, Fault::HeaderConflict) && chunk == 1 {
                        header = 1.0_f64.to_be_bytes();
                    }
                    if matches!(fault, Fault::InvalidTimestamp) {
                        header = f64::NAN.to_be_bytes();
                    }
                    if matches!(fault, Fault::ShortChunk) {
                        data.pop();
                    }
                    if matches!(fault, Fault::ChunkOutsidePiece) {
                        chunk = 4;
                    }
                    let stream = u32::from(matches!(fault, Fault::WrongStream));
                    let coordinate = piece + u32::from(matches!(fault, Fault::WrongPiece));
                    if session
                        .send(&build_piece(stream, coordinate, chunk, header, &data))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                _ => {}
            }
        }
    });
    FakePeer {
        addr,
        requests,
        task,
    }
}

#[tokio::test]
async fn exact_older_range_is_verified_and_signature_trailer_removed() {
    let peer = peer(0, 500, Fault::None).await;
    let result = fetch_history(
        &info(),
        &Identity::generate(),
        &[peer.addr],
        request(),
        limits(),
    )
    .await
    .unwrap();
    assert_eq!(result.attempted_peers, 1);
    assert_eq!(result.pieces.len(), 2);
    for (piece, index) in result.pieces.iter().zip(42..44) {
        assert_eq!(piece.index, index);
        assert_eq!(
            &piece.payload[..],
            vec![index as u8; 1024 - source().signature_len()]
        );
        assert_eq!(
            piece.reported_unix_seconds,
            Some(1_700_000_000.0 + f64::from(index))
        );
    }
    assert_eq!(
        *peer.requests.lock().unwrap(),
        (42..44)
            .flat_map(|piece| (0..4).map(move |chunk| (piece, chunk)))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn unchoke_before_window_is_retained_without_waiting_for_a_repeat() {
    let peer = peer(0, 500, Fault::EarlyUnchoke).await;
    let range = fetch_history(
        &info(),
        &Identity::generate(),
        &[peer.addr],
        request(),
        limits(),
    )
    .await
    .unwrap();
    assert_eq!(range.attempted_peers, 1);
    assert_eq!(
        range
            .pieces
            .iter()
            .map(|piece| piece.index)
            .collect::<Vec<_>>(),
        vec![42, 43]
    );
    assert_eq!(peer.requests.lock().unwrap().len(), 8);
}

#[tokio::test]
async fn choke_before_window_revokes_an_earlier_unchoke() {
    let peer = peer(0, 500, Fault::EarlyUnchokeThenChoke).await;
    let result = fetch_history(
        &info(),
        &Identity::generate(),
        &[peer.addr],
        request(),
        HistoryLimits {
            connect_timeout: Duration::from_millis(250),
            ..limits()
        },
    )
    .await;
    assert_eq!(
        result,
        Err(HistoryError::Unavailable {
            piece: 42,
            attempted_peers: 1,
            last_failure: HistoryFailure::PeerTimeout,
        })
    );
    assert!(peer.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn skips_newer_only_peer_without_jumping_and_deduplicates_candidates() {
    let newer = peer(100, 500, Fault::None).await;
    let older = peer(0, 300, Fault::None).await;
    let result = fetch_history(
        &info(),
        &Identity::generate(),
        &[newer.addr, newer.addr, older.addr],
        request(),
        limits(),
    )
    .await
    .unwrap();
    assert_eq!(result.attempted_peers, 2);
    assert_eq!(result.pieces[0].index, 42);
    assert!(newer.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn verified_prefix_survives_fallback_but_bad_piece_does_not() {
    let broken = peer(0, 500, Fault::Corrupt(43)).await;
    let valid = peer(0, 500, Fault::None).await;
    let result = fetch_history(
        &info(),
        &Identity::generate(),
        &[broken.addr, valid.addr],
        request(),
        limits(),
    )
    .await
    .unwrap();
    assert_eq!(result.attempted_peers, 2);
    assert_eq!(result.pieces.len(), 2);
    assert_eq!(
        &result.pieces[1].payload[..],
        vec![43; 1024 - source().signature_len()]
    );
    assert!(valid
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|(piece, _)| *piece == 43));
}

#[tokio::test]
async fn invalid_peer_data_has_explicit_failure_and_no_partial_range() {
    for (fault, expected) in [
        (Fault::WrongStream, HistoryFailure::WrongStream),
        (Fault::WrongPiece, HistoryFailure::WrongPiece),
        (Fault::ShortChunk, HistoryFailure::MalformedChunk),
        (Fault::ChunkOutsidePiece, HistoryFailure::MalformedChunk),
        (Fault::Corrupt(42), HistoryFailure::SignatureRejected),
        (Fault::HeaderConflict, HistoryFailure::ConflictingTimestamp),
    ] {
        let peer = peer(0, 500, fault).await;
        let result = fetch_history(
            &info(),
            &Identity::generate(),
            &[peer.addr],
            request(),
            limits(),
        )
        .await;
        assert_eq!(
            result,
            Err(HistoryError::Unavailable {
                piece: 42,
                attempted_peers: 1,
                last_failure: expected
            })
        );
    }
}

#[tokio::test]
async fn missing_piece_does_not_turn_verified_prefix_into_success() {
    let peer = peer(0, 500, Fault::MissingPiece(43)).await;
    let result = fetch_history(
        &info(),
        &Identity::generate(),
        &[peer.addr],
        request(),
        limits(),
    )
    .await;
    assert_eq!(
        result,
        Err(HistoryError::Unavailable {
            piece: 43,
            attempted_peers: 1,
            last_failure: HistoryFailure::PeerTimeout
        })
    );
}

#[tokio::test]
async fn duplicate_chunks_cannot_fake_coverage() {
    let peer = peer(0, 500, Fault::DuplicateChunk).await;
    let result = fetch_history(
        &info(),
        &Identity::generate(),
        &[peer.addr],
        request(),
        limits(),
    )
    .await;
    assert_eq!(
        result,
        Err(HistoryError::Unavailable {
            piece: 42,
            attempted_peers: 1,
            last_failure: HistoryFailure::PeerTimeout
        })
    );
}

#[tokio::test]
async fn invalid_advisory_timestamp_does_not_become_media_time() {
    let peer = peer(0, 500, Fault::InvalidTimestamp).await;
    let result = fetch_history(
        &info(),
        &Identity::generate(),
        &[peer.addr],
        request(),
        limits(),
    )
    .await
    .unwrap();
    assert!(result
        .pieces
        .iter()
        .all(|piece| piece.reported_unix_seconds.is_none()));
}

#[tokio::test]
async fn global_deadline_bounds_all_peers() {
    let peer = peer(0, 500, Fault::MissingPiece(42)).await;
    let limits = HistoryLimits {
        total_timeout: Duration::from_millis(100),
        piece_timeout: Duration::from_secs(1),
        ..limits()
    };
    let result = fetch_history(
        &info(),
        &Identity::generate(),
        &[peer.addr],
        request(),
        limits,
    )
    .await;
    assert_eq!(result, Err(HistoryError::Deadline));
}

#[tokio::test]
async fn dropping_future_closes_its_peer_connection() {
    let peer = peer(0, 500, Fault::MissingPiece(42)).await;
    let descriptor = info();
    let identity = Identity::generate();
    let peers = [peer.addr];
    let mut operation = Box::pin(fetch_history(
        &descriptor,
        &identity,
        &peers,
        request(),
        limits(),
    ));
    tokio::select! {
        result = &mut operation => panic!("unexpected completion: {result:?}"),
        observed = timeout(Duration::from_secs(1), async {
            loop {
                if !peer.requests.lock().unwrap().is_empty() { break; }
                tokio::task::yield_now().await;
            }
        }) => { observed.expect("peer never observed a request"); }
    }
    drop(operation);
    timeout(Duration::from_secs(1), async {
        while !peer.task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("peer socket remained open after cancellation");
}

#[tokio::test]
async fn max_peer_count_bounds_fallback() {
    let newer = peer(100, 500, Fault::None).await;
    let valid = peer(0, 500, Fault::None).await;
    let result = fetch_history(
        &info(),
        &Identity::generate(),
        &[newer.addr, valid.addr],
        request(),
        HistoryLimits {
            max_peers: 1,
            ..limits()
        },
    )
    .await;
    assert_eq!(
        result,
        Err(HistoryError::Unavailable {
            piece: 42,
            attempted_peers: 1,
            last_failure: HistoryFailure::NotAdvertised
        })
    );
    assert!(valid.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn window_inspection_is_bounded_and_requests_no_media() {
    let peer = peer(10, 500, Fault::None).await;
    let windows = inspect_history_windows(&info(), &[peer.addr], limits())
        .await
        .unwrap();
    assert_eq!(
        windows,
        vec![HistoryPeerWindow {
            peer: peer.addr,
            min_piece: 10,
            max_piece: 500
        }]
    );
    assert!(peer.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn validation_rejects_unsigned_bad_geometry_overflow_and_budgets_before_network() {
    let identity = Identity::generate();
    let mut descriptor = info();
    descriptor.source_pubkey.clear();
    assert_eq!(
        fetch_history(&descriptor, &identity, &[], request(), limits()).await,
        Err(HistoryError::VerificationRequired)
    );
    descriptor = info();
    descriptor.sig_len += 1;
    assert_eq!(
        fetch_history(&descriptor, &identity, &[], request(), limits()).await,
        Err(HistoryError::VerificationRequired)
    );
    descriptor = info();
    descriptor.chunk_length = 0;
    assert!(matches!(
        fetch_history(&descriptor, &identity, &[], request(), limits()).await,
        Err(HistoryError::InvalidRequest(_))
    ));
    for request in [
        HistoryRequest {
            start_piece: u32::MAX,
            piece_count: 2,
        },
        HistoryRequest {
            start_piece: 0,
            piece_count: 0,
        },
        HistoryRequest {
            start_piece: 0,
            piece_count: 129,
        },
    ] {
        assert!(matches!(
            fetch_history(&info(), &identity, &[], request, limits()).await,
            Err(HistoryError::InvalidRequest(_))
        ));
    }
    assert!(matches!(
        fetch_history(
            &info(),
            &identity,
            &[],
            request(),
            HistoryLimits {
                max_bytes: 1024,
                ..limits()
            }
        )
        .await,
        Err(HistoryError::InvalidRequest(_))
    ));
}

#[test]
fn missing_or_inverted_window_is_never_assumed_to_start_at_zero() {
    let addr = "127.0.0.1:1".parse().unwrap();
    for payload in [
        b"d2:mid9:max_piecei99eee".as_slice(),
        b"d2:mid9:max_piecei1e9:min_piecei2eee",
        b"d2:mid9:max_piecei4294967296e9:min_piecei0eee",
    ] {
        assert_eq!(
            parse_window(payload, addr),
            Err(HistoryFailure::InvalidWindow)
        );
    }
}

#[test]
fn receive_message_budget_is_independent_of_wire_byte_budget() {
    let mut budget = ReadBudget {
        bytes: Arc::new(AtomicU64::new(64)),
        messages: 2,
    };
    budget.consume_message().unwrap();
    budget.consume_message().unwrap();
    assert_eq!(budget.consume_message(), Err(HistoryFailure::ReceiveLimit));
    assert_eq!(budget.bytes.load(Ordering::Relaxed), 64);
}

#[tokio::test]
async fn transport_budget_caps_reads_and_survives_connection_replacement() {
    let budget = ReadBudget {
        bytes: Arc::new(AtomicU64::new(5)),
        messages: 100,
    };
    let mut first = BudgetedStream {
        inner: b"abc".as_slice(),
        remaining: Arc::clone(&budget.bytes),
    };
    let mut data = [0; 16];
    assert_eq!(first.read(&mut data).await.unwrap(), 3);
    // Ordinary EOF retains unused budget; it must not be reported as exhaustion.
    assert_eq!(first.read(&mut data).await.unwrap(), 0);
    assert_eq!(
        budget.failure(HistoryFailure::PeerIo),
        HistoryFailure::PeerIo
    );
    drop(first);
    let mut next = BudgetedStream {
        inner: b"defg".as_slice(),
        remaining: Arc::clone(&budget.bytes),
    };
    assert_eq!(next.read(&mut data).await.unwrap(), 2);
    assert_eq!(&data[..2], b"de");
    assert!(budget.exhausted());
    assert_eq!(next.inner, b"fg");
    assert!(next.read(&mut data).await.is_err());
    assert_eq!(next.inner, b"fg");
    assert_eq!(
        budget.failure(HistoryFailure::PeerIo),
        HistoryFailure::ReceiveLimit
    );
    // A zero-capacity read does not need more bytes, even at the budget boundary.
    assert_eq!(next.read(&mut []).await.unwrap(), 0);
}

#[tokio::test(start_paused = true)]
async fn partial_read_cancellation_retains_bytes_without_charging_pending_polls() {
    let remaining = Arc::new(AtomicU64::new(8));
    let (mut sender, receiver) = tokio::io::duplex(16);
    sender.write_all(b"abc").await.unwrap();
    let mut stream = BudgetedStream {
        inner: receiver,
        remaining: Arc::clone(&remaining),
    };
    let mut data = [0; 6];
    assert!(
        timeout(Duration::from_millis(10), stream.read_exact(&mut data))
            .await
            .is_err()
    );
    assert_eq!(remaining.load(Ordering::Relaxed), 5);
    assert_eq!(&data[..3], b"abc");
    drop(stream);
    assert!(sender.write_all(b"d").await.is_err());
    let mut next = BudgetedStream {
        inner: b"next read".as_slice(),
        remaining: Arc::clone(&remaining),
    };
    assert_eq!(next.read(&mut data).await.unwrap(), 5);
    assert_eq!(&data[..5], b"next ");
    assert!(next.read(&mut data).await.is_err());
}

#[tokio::test]
async fn transport_budget_includes_successful_handshake_bytes() {
    let remaining = Arc::new(AtomicU64::new(1024));
    let (client, server) = tokio::io::duplex(1024);
    let mut client = PeerSession::new(BudgetedStream {
        inner: client,
        remaining: Arc::clone(&remaining),
    });
    let mut server = PeerSession::new(server);
    let (sent, accepted) = tokio::join!(
        client.perform_handshake([0; 20], random_peer_id()),
        server.accept_handshake(random_peer_id(), |hash| *hash == [0; 20]),
    );
    sent.unwrap();
    accepted.unwrap();
    assert_eq!(
        remaining.load(Ordering::Relaxed),
        1024 - ace_wire::handshake::HANDSHAKE_LEN as u64
    );
}

#[tokio::test]
async fn final_u32_piece_is_a_valid_single_piece_range() {
    let peer = peer(u32::MAX, u32::MAX, Fault::None).await;
    let result = fetch_history(
        &info(),
        &Identity::generate(),
        &[peer.addr],
        HistoryRequest {
            start_piece: u32::MAX,
            piece_count: 1,
        },
        limits(),
    )
    .await
    .unwrap();
    assert_eq!(result.pieces.len(), 1);
    assert_eq!(result.pieces[0].index, u32::MAX);
    assert_eq!(
        &result.pieces[0].payload[..],
        vec![255; 1024 - source().signature_len()]
    );
    assert!(peer
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|(piece, _)| *piece == u32::MAX));
}
