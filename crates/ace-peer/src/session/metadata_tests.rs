use super::*;
use ace_wire::ut_metadata::{data_piece, MetadataMessage, METADATA_BLOCK_LEN};

const PEER_ID: u8 = 7;
const OUR_ID: u8 = 2;

fn data(piece: i64, total: usize, bytes: &[u8]) -> PeerMessage {
    PeerMessage::Extended {
        ext_id: OUR_ID,
        payload: data_piece(piece, total as i64, bytes),
    }
}

async fn exchange(total: usize, replies: Vec<PeerMessage>) -> Result<Vec<u8>> {
    let (client, server) = tokio::io::duplex(128 * 1024);
    let peer = tokio::spawn(async move {
        let mut session = PeerSession::new(server);
        for index in 0..total.div_ceil(METADATA_BLOCK_LEN) {
            let PeerMessage::Extended { ext_id, payload } = session.read_message().await.unwrap()
            else {
                panic!("expected metadata request");
            };
            assert_eq!(ext_id, PEER_ID, "requests use the peer's advertised id");
            assert_eq!(
                MetadataMessage::parse(&payload),
                Some(MetadataMessage::Request {
                    piece: index as i64
                })
            );
        }
        for reply in replies {
            if session.send(&reply).await.is_err() {
                break;
            }
        }
    });
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        PeerSession::new(client).fetch_metadata(PEER_ID, OUR_ID, total),
    )
    .await
    .expect("metadata exchange must finish");
    peer.await.unwrap();
    result
}

#[tokio::test]
async fn replies_use_our_id_and_unrelated_data_and_rejects_are_ignored() {
    let unrelated_data = PeerMessage::Extended {
        ext_id: PEER_ID,
        payload: data_piece(0, 4, b"evil"),
    };
    let unrelated_reject = PeerMessage::Extended {
        ext_id: 11,
        payload: b"d8:msg_typei2e5:piecei0ee".to_vec(),
    };
    assert_eq!(
        exchange(
            4,
            vec![unrelated_data, unrelated_reject, data(0, 4, b"good")]
        )
        .await
        .unwrap(),
        b"good"
    );
}

#[tokio::test]
async fn rejects_empty_short_and_oversized_metadata_blocks() {
    for (label, total, piece, bytes) in [
        ("empty", 100, 0, vec![]),
        ("short single", 100, 0, vec![1; 99]),
        ("oversized single", 100, 0, vec![1; 101]),
        ("short interior", METADATA_BLOCK_LEN + 100, 0, vec![1; 100]),
        (
            "oversized interior",
            METADATA_BLOCK_LEN + 100,
            0,
            vec![1; METADATA_BLOCK_LEN + 1],
        ),
        ("short tail", METADATA_BLOCK_LEN + 100, 1, vec![1; 99]),
        ("oversized tail", METADATA_BLOCK_LEN + 100, 1, vec![1; 101]),
    ] {
        assert!(
            matches!(
                exchange(total, vec![data(piece, total, &bytes)]).await,
                Err(PeerError::Protocol("invalid metadata block geometry"))
            ),
            "{label} must not complete a zero-filled or overlapping block"
        );
    }
}

#[tokio::test]
async fn rejects_missing_or_inconsistent_total_size() {
    for payload in [
        b"d8:msg_typei1e5:piecei0eeabcd".to_vec(),
        b"d8:msg_typei1e5:piecei0e10:total_size1:4eabcd".to_vec(),
        data_piece(0, -1, b"abcd"),
        data_piece(0, 0, b"abcd"),
        data_piece(0, 3, b"abcd"),
        data_piece(0, 5, b"abcd"),
    ] {
        assert!(matches!(
            exchange(
                4,
                vec![PeerMessage::Extended {
                    ext_id: OUR_ID,
                    payload
                }]
            )
            .await,
            Err(PeerError::Protocol("invalid metadata block geometry"))
        ));
    }
}

#[tokio::test]
async fn assembles_out_of_order_blocks_and_identical_duplicates() {
    let total = METADATA_BLOCK_LEN + 3;
    let first = vec![1; METADATA_BLOCK_LEN];
    let tail = b"end";
    let mut expected = first.clone();
    expected.extend_from_slice(tail);
    assert_eq!(
        exchange(
            total,
            vec![
                data(1, total, tail),
                data(1, total, tail),
                data(0, total, &first)
            ]
        )
        .await
        .unwrap(),
        expected
    );
}

#[tokio::test]
async fn conflicting_duplicate_is_rejected_before_overwriting_accepted_bytes() {
    let total = METADATA_BLOCK_LEN + 3;
    assert!(matches!(
        exchange(total, vec![data(1, total, b"end"), data(1, total, b"bad")]).await,
        Err(PeerError::Protocol("conflicting metadata block"))
    ));
}

#[tokio::test]
async fn invalid_piece_indices_cannot_wrap_into_a_valid_slot() {
    assert_eq!(
        exchange(
            4,
            vec![
                data(-1, 4, b"evil"),
                data(1_i64 << 32, 4, b"evil"),
                data(i64::MAX, 4, b"evil"),
                data(0, 4, b"good"),
            ],
        )
        .await
        .unwrap(),
        b"good"
    );
}

#[tokio::test]
async fn disabled_extension_ids_are_rejected_before_sending() {
    for (peer_id, our_id) in [(0, OUR_ID), (PEER_ID, 0)] {
        let (client, _server) = tokio::io::duplex(128);
        assert!(matches!(
            PeerSession::new(client)
                .fetch_metadata(peer_id, our_id, 4)
                .await,
            Err(PeerError::Protocol("metadata extension is disabled"))
        ));
    }
}
