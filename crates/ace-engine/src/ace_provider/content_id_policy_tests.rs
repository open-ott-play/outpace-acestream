use super::*;
use ace_swarm::listen::PeerListener;
use ace_wire::infohash::transport_file_hash;
use std::future::{ready, Future};

const CID: &str = "0123456789abcdef0123456789abcdef01234567";
const RAW_METAINFO: &[u8] =
    include_bytes!("../../../../tests/vectors/metainfo/single-file-unsorted.torrent");

fn catalog_failure<T>() -> impl Future<Output = Result<T, ResolveError>> {
    ready(Err(ResolveError::Catalog("synthetic catalog unavailable")))
}

fn provider() -> AceProvider {
    AceProvider::new(Arc::new(Identity::generate()), 0)
}

// A real metadata server exercises the complete TCP + BEP-9 path. Abort on drop keeps
// the server bounded even when an assertion fails.
struct MetadataPeer(tokio::task::JoinHandle<()>);
impl Drop for MetadataPeer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn metadata_peer(key: [u8; 20], bytes: Vec<u8>) -> (SocketAddrV4, MetadataPeer) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let std::net::SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
        unreachable!()
    };
    let registry = SeedRegistry::new();
    registry.register_metadata(key, bytes);
    let server = tokio::spawn(PeerListener::serve(
        listener,
        registry,
        random_peer_id(),
        [0; 8],
        4,
        Arc::new(Identity::generate()),
        4,
        None,
    ));
    (addr, MetadataPeer(server))
}

fn vod_transport() -> Vec<u8> {
    use ace_wire::bencode::Bencode;
    let descriptor = Bencode::Dict(std::collections::BTreeMap::from([
        (b"name".to_vec(), Bencode::Bytes(b"Synthetic VOD".to_vec())),
        (b"authmethod".to_vec(), Bencode::Bytes(b"None".to_vec())),
        (b"pubkey".to_vec(), Bencode::Bytes(vec![])),
        (b"bitrate".to_vec(), Bencode::Int(100_000)),
        (b"piece_length".to_vec(), Bencode::Int(16_384)),
        (b"chunk_length".to_vec(), Bencode::Int(16_384)),
        (b"length".to_vec(), Bencode::Int(16_384)),
        (b"pieces".to_vec(), Bencode::Bytes(vec![0; 20])),
    ]));
    ace_wire::transport::encode_transport(&descriptor)
}

#[tokio::test]
async fn disabled_live_and_vod_fail_before_peer_connections_or_discovery() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let std::net::SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
        unreachable!()
    };
    // Empty bootstrap peers would trigger discovery if the policy gate were bypassed.
    for peers in [vec![addr], vec![]] {
        let p = provider().with_bootstrap_peers(peers);
        let live = tokio::time::timeout(
            Duration::from_millis(100),
            p.resolve_content_id_with_catalog(CID, catalog_failure()),
        )
        .await
        .unwrap()
        .unwrap_err();
        let vod = tokio::time::timeout(
            Duration::from_millis(100),
            p.resolve_vod_info_with_catalog(&format!("cid:{CID}"), catalog_failure()),
        )
        .await
        .unwrap()
        .unwrap_err();
        for error in [live, vod] {
            let message = error.to_string();
            assert!(message.contains("synthetic catalog unavailable"));
            assert!(message.contains("fallback is disabled"));
            assert!(!message.contains(CID));
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn catalog_raw_metainfo_is_vod_only_at_the_provider_boundary() {
    let p = provider();
    let vod = p
        .resolve_vod_info_with_catalog(&format!("cid:{CID}"), ready(Ok(RAW_METAINFO.to_vec())))
        .await
        .unwrap();
    assert_eq!(vod.total_length, 16_385);
    assert_eq!(vod.piece_hashes, vec![[0xA5; 20], [0x5A; 20]]);

    let live = p
        .resolve_content_id_with_catalog(&infohash_hex(&hex20(CID).unwrap()), async {
            stream_info_from_transport(RAW_METAINFO)
                .map_err(|_| ResolveError::Transport("raw metainfo is not live transport"))
        })
        .await;
    assert!(
        live.is_err(),
        "raw metainfo must not become a live StreamInfo"
    );
}

#[tokio::test]
async fn enabled_live_peer_resolution_stays_out_of_catalog_trust() {
    let (bytes, _) = test_support::synthetic_live_transport(524_288);
    let key = transport_file_hash(&bytes);
    let expected = stream_info_from_transport(&bytes).unwrap();
    let (addr, _server) = metadata_peer(key, bytes).await;
    let p = provider()
        .with_bep9_content_id_fallback(true)
        .with_bootstrap_peers(vec![addr]);
    let cid = infohash_hex(&key);
    let actual = p
        .resolve_content_id_with_catalog(&cid, catalog_failure())
        .await
        .unwrap();
    assert_eq!(actual, expected);
    assert!(p.resolve_cache.get(&cid).is_none());
    assert!(p.peer_resolve_cache.get(&cid).is_some());
    assert!(p
        .verified_info_for_infohash(&infohash_hex(&expected.infohash))
        .is_err());
}

#[tokio::test]
async fn enabled_live_and_vod_reject_mismatched_raw_transport_hash() {
    let (live, _) = test_support::synthetic_live_transport(524_288);
    let key = hex20(CID).unwrap();
    for bytes in [live, vod_transport()] {
        assert_ne!(transport_file_hash(&bytes), key);
        let (addr, _server) = metadata_peer(key, bytes).await;
        let p = provider()
            .with_bep9_content_id_fallback(true)
            .with_bootstrap_peers(vec![addr]);
        assert!(p
            .resolve_content_id_with_catalog(CID, catalog_failure())
            .await
            .is_err());
        assert!(p
            .resolve_vod_info_with_catalog(&format!("cid:{CID}"), catalog_failure())
            .await
            .is_err());
        assert!(p.peer_resolve_cache.get(CID).is_none());
    }
}

#[tokio::test]
async fn enabled_vod_resolves_raw_hash_metadata() {
    let bytes = vod_transport();
    let key = transport_file_hash(&bytes);
    let (addr, _server) = metadata_peer(key, bytes).await;
    let p = provider()
        .with_bep9_content_id_fallback(true)
        .with_bootstrap_peers(vec![addr]);
    let result = p
        .resolve_vod_info_with_catalog(&format!("cid:{}", infohash_hex(&key)), catalog_failure())
        .await
        .unwrap();
    assert_eq!(result.total_length, 16_384);
    assert!(p.resolve_cache.get(&infohash_hex(&key)).is_none());
    assert!(p.infohash_index.get(&result.infohash).is_none());
}

#[tokio::test]
async fn local_broadcast_content_id_resolves_without_catalog_or_opt_in() {
    let seed = SeedRegistry::new();
    let broadcasts = crate::broadcast::BroadcastRegistry::new();
    let broadcast = broadcasts
        .mint("policy-test", &[], &seed, 1 << 20)
        .await
        .unwrap();
    let p = provider().with_seed_registry(seed);
    let cid = infohash_hex(&broadcast.content_id);
    let info = p
        .resolve_content_id_with_catalog(&cid, async {
            panic!("local broadcast must not fetch catalog")
        })
        .await
        .unwrap();
    assert_eq!(info.infohash, broadcast.infohash);
    assert!(p.resolve_cache.get(&cid).is_none());
    assert!(p.peer_resolve_cache.get(&cid).is_none());
    assert!(p.infohash_index.get(&info.infohash).is_none());
}

#[tokio::test]
async fn generic_seed_metadata_does_not_gain_local_broadcast_trust() {
    let (bytes, _) = test_support::synthetic_live_transport(524_288);
    let key = transport_file_hash(&bytes);
    let registry = SeedRegistry::new();
    registry.register_metadata(key, bytes);
    let p = provider().with_seed_registry(registry);
    let cid = infohash_hex(&key);
    let error = p
        .resolve_content_id_with_catalog(&cid, catalog_failure())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("fallback is disabled"));
}

#[tokio::test]
async fn disabled_fallback_preserves_catalog_resolution_and_cached_trust() {
    let (bytes, _) = test_support::synthetic_live_transport(524_288);
    let expected = stream_info_from_transport(&bytes).unwrap();
    let p = provider();
    assert_eq!(
        p.resolve_content_id_with_catalog(CID, ready(Ok(expected.clone())))
            .await
            .unwrap(),
        expected
    );
    assert_eq!(
        p.resolve_content_id_with_catalog(CID, async {
            panic!("cached catalog result must not refetch")
        })
        .await
        .unwrap(),
        expected
    );
    assert!(p
        .verified_info_for_infohash(&infohash_hex(&expected.infohash))
        .is_ok());
}

#[tokio::test]
async fn origin_hls_classification_is_fresh_and_never_indexes_swarm_geometry() {
    let p = provider().with_bep9_content_id_fallback(true);
    for token in ["first%2F", "second%2B"] {
        let url = format!("https://cdn.example.test/master.m3u8?token={token}");
        let resolved = p
            .resolve_content_playback_with_catalog(
                CID,
                ready(Ok(ResolvedTransport::Hls {
                    manifest_url: url.clone(),
                    metadata: StreamMetadata::default(),
                })),
            )
            .await
            .unwrap();
        let error = AceProvider::require_live(resolved).unwrap_err();
        assert!(
            matches!(&error, ProviderError::DirectHls { manifest_url } if manifest_url == &url)
        );
        assert!(!format!("{error:?}").contains(token));
        assert!(!error.to_string().contains(token));
        assert!(p.resolve_cache.get(CID).is_none());
        assert!(p.peer_resolve_cache.get(CID).is_none());
        assert!(p.verified_info_for_infohash(CID).is_err());
    }
}

#[tokio::test]
async fn classified_vod_does_not_enter_live_caches_or_peer_fallback() {
    let p = provider().with_bep9_content_id_fallback(true);
    let source = p
        .resolve_content_playback_with_catalog(CID, async { playback_from_transport(RAW_METAINFO) })
        .await
        .unwrap();
    assert!(matches!(&source, ResolvedTransport::Vod(info) if info.total_length == 16_385));
    let error = AceProvider::require_live(source).unwrap_err();
    assert!(matches!(&error, ProviderError::Unresolvable(message) if message.contains("/vod/")));
    assert!(p.resolve_cache.get(CID).is_none());
    assert!(p.peer_resolve_cache.get(CID).is_none());
}

#[tokio::test]
async fn recognized_invalid_source_never_falls_back_to_peer_resolution() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let std::net::SocketAddr::V4(address) = listener.local_addr().unwrap() else {
        unreachable!()
    };
    let p = provider()
        .with_bep9_content_id_fallback(true)
        .with_bootstrap_peers(vec![address]);
    for failure in [
        ResolveError::Hls("missing manifest"),
        ResolveError::Transport("multi-file VOD is not supported"),
    ] {
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            p.resolve_content_playback_with_catalog(CID, ready(Err(failure))),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(ProviderError::Unresolvable(_))));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn authenticated_cid_seed_preserves_native_session_identity_without_second_fetch() {
    let p = provider();
    let (bytes, _) = test_support::synthetic_live_transport(524_288);
    let expected = stream_info_from_transport(&bytes).unwrap();
    p.remember_catalog_descriptor(&CID.to_ascii_uppercase(), &expected);
    let source = p
        .resolve_content_playback_with_catalog(CID, async {
            panic!("the signed CID binding should already be cached")
        })
        .await
        .unwrap();
    assert!(matches!(source, ResolvedTransport::Swarm(info) if info == expected));
    assert_eq!(
        p.verified_info_for_infohash(&infohash_hex(&expected.infohash))
            .unwrap(),
        expected
    );
    assert!(p.peer_resolve_cache.get(CID).is_none());
}
