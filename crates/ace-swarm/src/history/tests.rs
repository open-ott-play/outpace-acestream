use super::*;
use ace_wire::live_auth::LiveSourceAuth;
use ace_wire::live_codec::build_piece;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
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
        _ = async {
            loop {
                if !peer.requests.lock().unwrap().is_empty() { break; }
                tokio::task::yield_now().await;
            }
        } => {}
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
fn receive_budget_counts_control_messages_and_payload_without_copying() {
    let mut budget = ReadBudget {
        bytes: 64,
        messages: 2,
    };
    budget.consume(&PeerMessage::KeepAlive).unwrap();
    budget.consume(&PeerMessage::KeepAlive).unwrap();
    assert_eq!(
        budget.consume(&PeerMessage::KeepAlive),
        Err(HistoryFailure::ReceiveLimit)
    );
    let mut budget = ReadBudget {
        bytes: 64,
        messages: 100,
    };
    assert_eq!(
        budget.consume(&PeerMessage::Bitfield(vec![0; 65])),
        Err(HistoryFailure::ReceiveLimit)
    );
}
