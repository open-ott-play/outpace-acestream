use std::path::Path;

type Target = (&'static str, fn(&[u8]));

#[test]
fn replay_committed_synthetic_inputs() {
    let targets: [Target; 5] = [
        ("metadata", outpace_fuzz::metadata),
        ("peer_frames", outpace_fuzz::peer_frames),
        ("peer_extensions", outpace_fuzz::peer_extensions),
        ("media_resync", outpace_fuzz::media_resync),
        ("hls_playlist", outpace_fuzz::hls_playlist),
    ];
    for (name, target) in targets {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("corpus")
            .join(name);
        let mut seeds: Vec<_> = std::fs::read_dir(root)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        seeds.sort_by_key(|seed| seed.file_name());
        assert!(seeds.len() >= 4);
        for seed in seeds {
            assert!(seed.file_type().unwrap().is_file());
            let data = std::fs::read(seed.path()).unwrap();
            assert!(data.len() <= outpace_fuzz::MAX_INPUT);
            target(&data);
        }
    }
}

#[test]
fn positive_controls_reach_accepted_paths() {
    use ace_wire::bencode::Bencode;
    use ace_wire::message::PeerMessage;
    for seed in [
        include_bytes!("../corpus/hls_playlist/master").as_slice(),
        include_bytes!("../corpus/hls_playlist/fmp4-ranges").as_slice(),
    ] {
        let playlist = ace_media::hls_input::Playlist::parse(
            seed,
            std::time::Duration::from_secs(6 * 60 * 60),
        )
        .unwrap();
        assert!(!playlist.references().is_empty());
        outpace_fuzz::hls_playlist(seed);
    }
    let metadata = include_bytes!("../corpus/metadata/single-piece");
    assert!(ace_wire::metainfo::decode_metainfo(metadata).is_ok());
    assert!(Bencode::parse(metadata).is_ok());
    assert_eq!(
        PeerMessage::decode(&[0, 0, 0, 1, 2]).unwrap(),
        Some((PeerMessage::Interested, 5))
    );
    let message = include_bytes!("../corpus/peer_extensions/metadata-data");
    assert!(ace_wire::ut_metadata::MetadataMessage::parse(message).is_some());
    let peers = include_bytes!("../corpus/peer_extensions/peer-windows");
    assert!(!ace_wire::peer_exchange::parse_peer_exchange_detailed(peers).is_empty());
    let ts = include_bytes!("../corpus/media_resync/aligned-packets");
    let output = ace_media::mpegts::TsResync::new().push(ts);
    assert!(!output.is_empty() && ace_media::mpegts::is_aligned(&output));
    for seed in [
        include_bytes!("../corpus/media_resync/h264-pes").as_slice(),
        include_bytes!("../corpus/media_resync/hevc-pes").as_slice(),
    ] {
        let mut state = ace_media::mpegts::VideoAccessPointState::new();
        let accepted: Vec<_> = seed
            .as_chunks::<188>()
            .0
            .iter()
            .map(|p| state.observe(p))
            .collect();
        assert_eq!(accepted, vec![false, false, true]);
        let mut gate = ace_media::mpegts::KeyframeGate::new();
        assert_eq!(gate.push(seed), seed);
        assert!(gate.is_locked());
        outpace_fuzz::media_resync(seed);
    }
}
