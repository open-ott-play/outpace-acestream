use super::SeederSession;
use ace_peer::session::PeerSession;
use ace_wire::extended::ExtendedHandshake;
use ace_wire::identity::Identity;
use ace_wire::message::PeerMessage;
use ace_wire::ut_metadata::{request_piece, MetadataMessage, METADATA_BLOCK_LEN};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::DuplexStream;
use tokio::task::JoinHandle;

async fn connect(
    metadata: Vec<u8>,
) -> (PeerSession<DuplexStream>, JoinHandle<ace_peer::Result<()>>) {
    let total = metadata.len();
    let (client, server) = tokio::io::duplex(128 * 1024);
    let task = tokio::spawn(async move {
        let mut session = PeerSession::new(server);
        SeederSession::serve(
            &mut session,
            None,
            Some(Arc::new(metadata)),
            [0; 8],
            &Identity::from_seed([7; 32]),
            [127, 0, 0, 1],
            None,
        )
        .await
    });
    let mut client = PeerSession::new(client).with_timeout(Duration::from_secs(2));
    let PeerMessage::Extended { ext_id: 0, payload } = client.read_message().await.unwrap() else {
        panic!("expected seeder extended handshake");
    };
    let handshake = ExtendedHandshake::parse(&payload).unwrap();
    assert_eq!(handshake.metadata_size(), Some(total as i64));
    assert_eq!(handshake.ut_metadata_id(), Some(2));
    (client, task)
}

async fn handshake(client: &mut PeerSession<DuplexStream>, payload: &[u8]) {
    client
        .send(&PeerMessage::Extended {
            ext_id: 0,
            payload: payload.to_vec(),
        })
        .await
        .unwrap();
}

async fn request(client: &mut PeerSession<DuplexStream>, piece: i64) {
    client
        .send(&PeerMessage::Extended {
            ext_id: 2,
            payload: request_piece(piece),
        })
        .await
        .unwrap();
}

async fn expect_data(
    client: &mut PeerSession<DuplexStream>,
    response_id: u8,
    piece: i64,
    total: usize,
    bytes: &[u8],
) {
    let PeerMessage::Extended { ext_id, payload } = client.read_message().await.unwrap() else {
        panic!("expected metadata data");
    };
    assert_eq!(ext_id, response_id);
    assert_eq!(
        MetadataMessage::parse(&payload),
        Some(MetadataMessage::Data {
            piece,
            total_size: Some(total as i64),
            data: bytes.to_vec(),
        })
    );
}

#[tokio::test]
async fn invalid_indices_do_not_alias_the_first_piece_or_crash_the_seeder() {
    let total = METADATA_BLOCK_LEN + 3;
    let mut metadata = vec![b'a'; METADATA_BLOCK_LEN];
    metadata.extend_from_slice(b"end");
    let (mut client, task) = connect(metadata).await;
    handshake(&mut client, b"d1:md11:ut_metadatai7eee").await;

    for piece in [-1, 1_i64 << 32, 1_i64 << 50, i64::MAX, 2] {
        request(&mut client, piece).await;
    }
    request(&mut client, 1).await;
    expect_data(&mut client, 7, 1, total, b"end").await;
    drop(client);
    assert!(matches!(
        task.await.unwrap(),
        Err(ace_peer::PeerError::Closed)
    ));
}

#[tokio::test]
async fn response_mapping_supports_partial_updates_disable_and_reenable() {
    let total = METADATA_BLOCK_LEN + 3;
    let first = vec![b'a'; METADATA_BLOCK_LEN];
    let mut metadata = first.clone();
    metadata.extend_from_slice(b"end");
    let (mut client, task) = connect(metadata).await;

    // The peer has not advertised an id, so a request must not get a guessed-id response.
    request(&mut client, 0).await;
    handshake(&mut client, b"d1:md11:ut_metadatai7eee").await;
    request(&mut client, 1).await;
    expect_data(&mut client, 7, 1, total, b"end").await;

    // BEP-10 updates are additive: absence from a later handshake keeps the old mapping.
    handshake(&mut client, b"de").await;
    handshake(&mut client, b"d1:mdee").await;
    request(&mut client, 0).await;
    expect_data(&mut client, 7, 0, total, &first).await;

    // Zero disables it; a partial update cannot re-enable it. Only the final request replies.
    handshake(&mut client, b"d1:md11:ut_metadatai0eee").await;
    request(&mut client, 0).await;
    handshake(&mut client, b"d1:mdee").await;
    request(&mut client, 0).await;
    handshake(&mut client, b"d1:md11:ut_metadatai11eee").await;
    request(&mut client, 1).await;
    expect_data(&mut client, 11, 1, total, b"end").await;

    drop(client);
    assert!(matches!(
        task.await.unwrap(),
        Err(ace_peer::PeerError::Closed)
    ));
}
