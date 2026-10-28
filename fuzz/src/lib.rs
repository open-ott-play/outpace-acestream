//! Offline invariants over production protocol and media parsers.
//! The same functions run with fixed seeds in ordinary CI and libFuzzer for RCs.

use ace_media::mpegts::{self, SelectedPcrClock, TsResync, TS_PACKET_LEN};
use ace_wire::bencode::Bencode;
use ace_wire::handshake::{Handshake, HandshakeProtocol};
use ace_wire::message::PeerMessage;
use ace_wire::metainfo::decode_metainfo;
use ace_wire::peer_exchange::{
    parse_peer_announce, parse_peer_exchange, parse_peer_exchange_detailed, rank_by_window_coverage,
};
use ace_wire::ut_metadata::{self, MetadataMessage};

pub const MAX_INPUT: usize = 16 * 1024;

/// Exercise the same bounded parser and byte-span rewriting used by the origin relay.
/// No URL is resolved or fetched, and replacement text cannot enlarge an input line.
pub fn hls_playlist(data: &[u8]) {
    use ace_media::hls_input::{Playlist, MAX_REFERENCES};
    use std::time::Duration;

    if data.len() > MAX_INPUT {
        return;
    }
    let cap = Duration::from_secs(6 * 60 * 60);
    let parsed = Playlist::parse(data, cap);
    assert_eq!(parsed, Playlist::parse(data, cap));
    if let Ok(playlist) = parsed {
        let count = playlist.references().len();
        assert!(count <= MAX_REFERENCES);
        assert!(playlist.retention() <= cap);
        let timeline = playlist.media_timeline();
        let discontinuity_sequence = playlist.discontinuity_sequence();
        let retention = playlist.retention();
        let segments = playlist.segments().to_vec();
        let simple_replay = playlist.supports_simple_replay();
        assert!(segments.len() <= count);
        for segment in &segments {
            assert!(segment.reference_index < count);
            assert_eq!(
                playlist.references()[segment.reference_index].kind,
                ace_media::hls_input::ResourceKind::Media
            );
        }
        for pair in segments.windows(2) {
            assert!(pair[0].sequence < pair[1].sequence);
            assert!(pair[0].discontinuity_sequence <= pair[1].discontinuity_sequence);
        }
        let unchanged: Vec<_> = playlist
            .references()
            .iter()
            .map(|r| r.uri.clone())
            .collect();
        let original = playlist.render(&unchanged).unwrap();
        assert_eq!(original.as_bytes(), data);

        let shorter = vec!["x".to_owned(); count];
        let rewritten = Playlist::parse(data, cap)
            .unwrap()
            .render(&shorter)
            .unwrap();
        assert!(rewritten.len() <= data.len());
        let reparsed = Playlist::parse(rewritten.as_bytes(), cap).unwrap();
        assert_eq!(reparsed.references().len(), count);
        assert_eq!(reparsed.media_timeline(), timeline);
        assert_eq!(reparsed.discontinuity_sequence(), discontinuity_sequence);
        assert_eq!(reparsed.retention(), retention);
        assert_eq!(reparsed.segments(), segments);
        // Eligibility here depends on tags; URI uniqueness and media bytes are
        // separately checked by the recording backend before capture.
        assert_eq!(reparsed.supports_simple_replay(), simple_replay);
        assert!(reparsed.references().iter().all(|r| r.uri == "x"));

        let wrong_count = vec!["x".to_owned(); count + 1];
        assert!(Playlist::parse(data, cap)
            .unwrap()
            .render(&wrong_count)
            .is_err());
    }
}

pub fn metadata(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    if let Ok((value, consumed)) = Bencode::parse_prefix(data) {
        assert!(consumed > 0 && consumed <= data.len());
        assert_eq!(Bencode::parse(&data[..consumed]), Ok(value.clone()));
        let encoded = value.encode();
        assert!(encoded.len() <= data.len());
        assert_eq!(Bencode::parse(&encoded), Ok(value));
        assert_eq!(Bencode::parse(data).is_ok(), consumed == data.len());
    }
    if let Ok(value) = decode_metainfo(data) {
        assert!(value.piece_length > 0 && value.piece_length <= 8 * 1024 * 1024);
        assert!(value.piece_length.is_multiple_of(16_384));
        assert!(value.total_length > 0);
        assert_eq!(
            value.piece_hashes.len() as u64,
            value.total_length.div_ceil(value.piece_length)
        );
        assert!(value.piece_hashes.len() <= data.len() / 20);
        assert!(value.trackers.len() <= 128);
        if let Some(name) = &value.filename {
            assert!(!name.is_empty() && name.len() <= 4096);
            assert!(!name.chars().any(char::is_control));
        }
        // Never allocate from advertised geometry or fetch any tracker.
        assert_eq!(decode_metainfo(data), Ok(value));
    }
}

pub fn peer_frames(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    if let Ok(Some((message, consumed))) = PeerMessage::decode(data) {
        assert!((4..=data.len()).contains(&consumed));
        let encoded = message.encode();
        assert_eq!(encoded, data[..consumed]);
        assert_eq!(PeerMessage::decode(&encoded), Ok(Some((message, consumed))));
        // An otherwise-valid frame must not be emitted before its last byte.
        assert_eq!(PeerMessage::decode(&data[..consumed - 1]), Ok(None));
    }
    for protocol in [HandshakeProtocol::AceStream, HandshakeProtocol::BitTorrent] {
        if let Ok(handshake) = Handshake::decode_with_protocol(data, protocol) {
            let encoded = handshake.encode_with_protocol(protocol);
            assert!(encoded.len() <= data.len());
            assert_eq!(encoded, data[..encoded.len()]);
            assert_eq!(
                Handshake::decode_with_protocol(&encoded, protocol),
                Ok(handshake)
            );
        }
    }
}

pub fn peer_extensions(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    if let Some(message) = MetadataMessage::parse(data) {
        let (control, consumed) = Bencode::parse_prefix(data).unwrap();
        match message {
            MetadataMessage::Request { piece } => {
                let encoded = ut_metadata::request_piece(piece);
                assert_eq!(
                    MetadataMessage::parse(&encoded),
                    Some(MetadataMessage::Request { piece })
                );
            }
            MetadataMessage::Data {
                piece,
                total_size,
                data: payload,
            } => {
                assert_eq!(payload, data[consumed..]);
                assert_eq!(control.get(b"piece").and_then(Bencode::as_int), Some(piece));
                if let Some(total) = total_size {
                    let encoded = ut_metadata::data_piece(piece, total, &payload);
                    assert_eq!(
                        MetadataMessage::parse(&encoded),
                        Some(MetadataMessage::Data {
                            piece,
                            total_size,
                            data: payload,
                        })
                    );
                }
            }
            MetadataMessage::Reject { piece } => {
                assert_eq!(control.get(b"piece").and_then(Bencode::as_int), Some(piece));
            }
        }
    }
    let peers = parse_peer_exchange_detailed(data);
    assert!(peers.len() <= data.len() / 17);
    assert_eq!(
        parse_peer_exchange(data),
        peers.iter().map(|peer| peer.addr).collect::<Vec<_>>()
    );
    for peer in &peers {
        assert!(!peer.addr.ip().is_unspecified() && peer.addr.port() != 0);
        if let Some(window) = peer.window {
            assert!(window.min > 0 && window.min <= window.max && window.max < u32::MAX as u64);
            assert!(window.covers(window.min) && window.covers(window.max));
        }
    }
    let cursor = data
        .get(..4)
        .map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()))
        .unwrap_or(0) as u64;
    let ranked = rank_by_window_coverage(&peers, cursor);
    let expected: Vec<_> = [0, 1, 2]
        .into_iter()
        .flat_map(|rank| {
            peers
                .iter()
                .filter(move |peer| {
                    let actual = match peer.window {
                        Some(window) if window.covers(cursor) => 0,
                        None => 1,
                        Some(_) => 2,
                    };
                    actual == rank
                })
                .map(|peer| peer.addr)
        })
        .collect();
    assert_eq!(ranked, expected);
    if let Some(peer) = parse_peer_announce(data) {
        assert!(!peer.ip().is_unspecified() && peer.port() != 0);
    }
}

fn resync(data: &[u8], chunk_size: usize) -> Vec<u8> {
    let mut parser = TsResync::new();
    let mut output = Vec::new();
    let mut discarded = 0;
    for chunk in data.chunks(chunk_size) {
        let report = parser.push_report(chunk);
        assert!(report.bytes.is_empty() || mpegts::is_aligned(&report.bytes));
        assert_eq!(
            report
                .boundaries
                .iter()
                .map(|boundary| boundary.discarded_bytes)
                .sum::<usize>(),
            report.discarded_bytes
        );
        let mut last = 0;
        for boundary in report.boundaries {
            assert!(boundary.output_offset >= last && boundary.output_offset <= report.bytes.len());
            assert!(boundary.output_offset.is_multiple_of(TS_PACKET_LEN));
            assert!(boundary.discarded_bytes > 0);
            last = boundary.output_offset;
        }
        discarded += report.discarded_bytes;
        output.extend(report.bytes);
        assert!(output.len() + discarded <= data.len());
    }
    output
}

pub fn media_resync(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let whole = resync(data, MAX_INPUT);
    // Packet cuts, a one-byte prefix and arbitrary short chunks model read boundaries.
    let chunk_size = 1 + usize::from(data.first().copied().unwrap_or(0));
    assert_eq!(whole, resync(data, chunk_size));
    assert_eq!(whole, resync(data, TS_PACKET_LEN));
    let mut clock = SelectedPcrClock::new();
    let mut access = mpegts::VideoAccessPointState::new();
    access.enable_mpeg2();
    for packet in data.as_chunks::<TS_PACKET_LEN>().0 {
        let timing = mpegts::ts_timing(packet);
        assert!(timing.pid < 8192);
        assert!(timing.pcr.is_none_or(|value| value < (1 << 33)));
        clock.observe(timing);
        if access.observe(packet) {
            assert_eq!(packet[0], 0x47);
            assert_eq!(packet[1] & 0x80, 0);
            assert_ne!(packet[1] & 0x40, 0);
            let tables = access.table_prefix().expect("access point requires PSI");
            assert!(mpegts::is_aligned(&tables));
            assert!(tables.len() <= TS_PACKET_LEN * 3);
        }
    }
    if let Some(elapsed) = clock.elapsed() {
        assert!(elapsed.as_secs() < (1 << 32) / 90_000 + 1);
    }
    if !whole.is_empty() {
        let parts = ace_media::hls::segment(&whole, 1 + chunk_size % 16).unwrap();
        assert!(parts.iter().all(|part| mpegts::is_aligned(part)));
        assert_eq!(parts.concat(), whole);
        // Read boundaries must not change PES-aware keyframe admission or player bytes.
        let expected = mpegts::KeyframeGate::new().push(&whole);
        let mut gate = mpegts::KeyframeGate::new();
        let actual: Vec<_> = whole
            .chunks(chunk_size)
            .flat_map(|part| gate.push(part))
            .collect();
        assert_eq!(actual, expected);
        assert!(actual.len() <= whole.len() + TS_PACKET_LEN * 3);
        assert!(actual.is_empty() || mpegts::is_aligned(&actual));
    }
}
