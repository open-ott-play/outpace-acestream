//! Narrow RTMP H.264/AAC to MPEG-TS remuxing for broadcast ingest.

use rtmp_rs::media::aac::{generate_adts_header, AudioSpecificConfig};
use rtmp_rs::media::h264::{AvcConfig, NaluIterator};
use rtmp_rs::media::{AacData, H264Data};

const PAT_PID: u16 = 0x0000;
const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x0100;
const AUDIO_PID: u16 = 0x0101;
const VIDEO_STREAM_ID: u8 = 0xe0;
const AUDIO_STREAM_ID: u8 = 0xc0;
const TIMESTAMP_MODULUS: i64 = 1 << 33;
const TABLE_INTERVAL_TICKS: u64 = 9_000;
const MAX_ADTS_PAYLOAD: usize = (1 << 13) - 1 - 7;

pub struct RtmpTsMuxer {
    video_config: Option<AvcConfig>,
    audio_config: Option<AudioSpecificConfig>,
    pat_cc: u8,
    pmt_cc: u8,
    video_cc: u8,
    audio_cc: u8,
    last_tables_dts: Option<u64>,
}

impl Default for RtmpTsMuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl RtmpTsMuxer {
    pub fn new() -> Self {
        Self {
            video_config: None,
            audio_config: None,
            pat_cc: 0,
            pmt_cc: 0,
            video_cc: 0,
            audio_cc: 0,
            last_tables_dts: None,
        }
    }

    pub fn push_video(&mut self, frame: &H264Data, timestamp_ms: u32) -> Vec<u8> {
        match frame {
            H264Data::SequenceHeader(config) => {
                self.video_config = Some(config.clone());
                Vec::new()
            }
            H264Data::Frame {
                keyframe,
                composition_time,
                nalus,
            } => {
                let Some(config) = self.video_config.clone() else {
                    return Vec::new();
                };
                let mut access_unit = Vec::new();
                access_unit.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x09, 0xf0]);
                if *keyframe {
                    for sps in &config.sps {
                        push_annex_b_nalu(&mut access_unit, sps);
                    }
                    for pps in &config.pps {
                        push_annex_b_nalu(&mut access_unit, pps);
                    }
                }
                for nalu in NaluIterator::new(nalus.as_ref(), config.nalu_length_size) {
                    push_annex_b_nalu(&mut access_unit, nalu);
                }
                let dts = pts90(timestamp_ms);
                let pts = ((i64::from(timestamp_ms) + i64::from(*composition_time)) * 90)
                    .rem_euclid(TIMESTAMP_MODULUS) as u64;
                self.packetize_pes(
                    VIDEO_PID,
                    VIDEO_STREAM_ID,
                    dts,
                    Some(pts),
                    &access_unit,
                    *keyframe,
                )
            }
            H264Data::EndOfSequence => Vec::new(),
        }
    }

    pub fn push_audio(&mut self, frame: &AacData, timestamp_ms: u32) -> Vec<u8> {
        match frame {
            AacData::SequenceHeader(config) => {
                self.audio_config = Some(config.clone());
                Vec::new()
            }
            AacData::Frame { data } => {
                // ADTS has a 13-bit frame length, including its seven-byte header. Do not
                // silently truncate this field for an invalid oversized RTMP audio frame.
                if data.len() > MAX_ADTS_PAYLOAD {
                    return Vec::new();
                }
                let Some(config) = self.audio_config.clone() else {
                    return Vec::new();
                };
                let mut aac = Vec::with_capacity(7 + data.len());
                aac.extend_from_slice(&generate_adts_header(&config, data.len()));
                aac.extend_from_slice(data);
                let pts = pts90(timestamp_ms);
                self.packetize_pes(AUDIO_PID, AUDIO_STREAM_ID, pts, None, &aac, false)
            }
        }
    }

    fn packetize_pes(
        &mut self,
        pid: u16,
        stream_id: u8,
        dts: u64,
        pts: Option<u64>,
        payload: &[u8],
        random_access: bool,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        // A live follower joins after publication began. Repeat tables periodically and
        // immediately before keyframes so the keyframe gate can discover the video PID.
        if random_access
            || self
                .last_tables_dts
                .is_none_or(|last| dts.wrapping_sub(last) >= TABLE_INTERVAL_TICKS)
        {
            self.write_tables(&mut out);
            self.last_tables_dts = Some(dts);
        }
        let pes = pes_packet(stream_id, dts, pts, payload);
        let cc = if pid == VIDEO_PID {
            &mut self.video_cc
        } else {
            &mut self.audio_cc
        };
        write_ts_payload_with_clock(
            &mut out,
            pid,
            cc,
            &pes,
            (pid == VIDEO_PID).then_some(dts),
            random_access,
        );
        out
    }

    fn write_tables(&mut self, out: &mut Vec<u8>) {
        let pat = pat_section();
        let pmt = pmt_section();
        write_ts_payload_with_clock(out, PAT_PID, &mut self.pat_cc, &pat, None, false);
        write_ts_payload_with_clock(out, PMT_PID, &mut self.pmt_cc, &pmt, None, false);
    }
}

fn push_annex_b_nalu(out: &mut Vec<u8>, nalu: &[u8]) {
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    out.extend_from_slice(nalu);
}

fn pts90(ms: u32) -> u64 {
    u64::from(ms) * 90
}

fn pes_packet(stream_id: u8, dts: u64, pts: Option<u64>, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(19 + payload.len());
    out.extend_from_slice(&[0x00, 0x00, 0x01, stream_id]);
    out.extend_from_slice(&[0x00, 0x00]);
    out.push(0x80);
    match pts {
        Some(pts) if pts != dts => {
            out.push(0xc0);
            out.push(10);
            write_ts_timestamp(&mut out, 0x03, pts);
            write_ts_timestamp(&mut out, 0x01, dts);
        }
        Some(pts) => {
            out.push(0x80);
            out.push(5);
            write_ts_timestamp(&mut out, 0x02, pts);
        }
        None => {
            out.push(0x80);
            out.push(5);
            write_ts_timestamp(&mut out, 0x02, dts);
        }
    }
    out.extend_from_slice(payload);
    // Unbounded PES length is permitted for video; audio must carry its actual length.
    if stream_id == AUDIO_STREAM_ID {
        let length = u16::try_from(out.len() - 6).expect("ADTS frame length was bounded");
        out[4..6].copy_from_slice(&length.to_be_bytes());
    }
    out
}

fn write_ts_timestamp(out: &mut Vec<u8>, prefix: u8, ts: u64) {
    let ts = ts & 0x1ffffffff;
    out.push((prefix << 4) | (((ts >> 30) as u8 & 0x07) << 1) | 1);
    out.push((ts >> 22) as u8);
    out.push((((ts >> 15) as u8 & 0x7f) << 1) | 1);
    out.push((ts >> 7) as u8);
    out.push(((ts as u8 & 0x7f) << 1) | 1);
}

fn write_ts_payload_with_clock(
    out: &mut Vec<u8>,
    pid: u16,
    cc: &mut u8,
    payload: &[u8],
    pcr: Option<u64>,
    random_access: bool,
) {
    let mut rest = payload;
    let mut first = true;
    while !rest.is_empty() {
        let payload_start = first;
        let clock = pcr.filter(|_| first);
        let flags = (u8::from(first && random_access) * 0x40) | (u8::from(clock.is_some()) * 0x10);
        let min_adaptation = if clock.is_some() {
            8
        } else if flags != 0 {
            2
        } else {
            0
        };
        let payload_capacity = 184 - min_adaptation;
        let take = rest.len().min(payload_capacity);
        let remaining_space = 184 - take;
        let adaptation = remaining_space > 0;

        out.push(0x47);
        out.push(((payload_start as u8) << 6) | ((pid >> 8) as u8 & 0x1f));
        out.push(pid as u8);
        out.push(if adaptation { 0x30 } else { 0x10 } | (*cc & 0x0f));
        *cc = (*cc + 1) & 0x0f;

        if adaptation {
            out.push(remaining_space as u8 - 1);
            if remaining_space > 1 {
                out.push(flags);
                if let Some(clock) = clock {
                    let base = clock & 0x1ffffffff;
                    out.extend_from_slice(&[
                        (base >> 25) as u8,
                        (base >> 17) as u8,
                        (base >> 9) as u8,
                        (base >> 1) as u8,
                        ((base & 1) as u8) << 7 | 0x7e,
                        0,
                    ]);
                }
                let clock_bytes = if clock.is_some() { 6 } else { 0 };
                out.extend(std::iter::repeat_n(0xff, remaining_space - 2 - clock_bytes));
            }
        }

        out.extend_from_slice(&rest[..take]);
        rest = &rest[take..];
        first = false;
    }
}

fn pat_section() -> Vec<u8> {
    let mut section = vec![
        0x00,
        0x00,
        0xb0,
        0x0d,
        0x00,
        0x01,
        0xc1,
        0x00,
        0x00,
        0x00,
        0x01,
        0xf0 | ((PMT_PID >> 8) as u8 & 0x1f),
        PMT_PID as u8,
    ];
    let crc = ace_media::mpegts::mpeg_crc32(&section[1..]);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

fn pmt_section() -> Vec<u8> {
    let section_len = 9 + 5 + 5 + 4;
    let mut section = vec![
        0x00,
        0x02,
        0xb0 | ((section_len >> 8) as u8 & 0x0f),
        section_len as u8,
        0x00,
        0x01,
        0xc1,
        0x00,
        0x00,
        0xe0 | ((VIDEO_PID >> 8) as u8 & 0x1f),
        VIDEO_PID as u8,
        0xf0,
        0x00,
        0x1b,
        0xe0 | ((VIDEO_PID >> 8) as u8 & 0x1f),
        VIDEO_PID as u8,
        0xf0,
        0x00,
        0x0f,
        0xe0 | ((AUDIO_PID >> 8) as u8 & 0x1f),
        AUDIO_PID as u8,
        0xf0,
        0x00,
    ];
    let crc = ace_media::mpegts::mpeg_crc32(&section[1..]);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rtmp_rs::media::aac::{generate_adts_header, AudioSpecificConfig};
    use rtmp_rs::media::{AacData, H264Data};

    fn avc_config() -> rtmp_rs::media::h264::AvcConfig {
        rtmp_rs::media::h264::AvcConfig {
            profile: 100,
            compatibility: 0,
            level: 31,
            nalu_length_size: 4,
            sps: vec![Bytes::from_static(&[0x67, 0x64, 0x00, 0x1f])],
            pps: vec![Bytes::from_static(&[0x68, 0xef, 0x38])],
            raw: Bytes::new(),
        }
    }

    fn aac_config() -> AudioSpecificConfig {
        AudioSpecificConfig::parse(Bytes::from_static(&[0x12, 0x10])).unwrap()
    }

    #[test]
    fn muxer_emits_aligned_ts_for_h264_and_aac() {
        let mut muxer = RtmpTsMuxer::new();
        let mut out = Vec::new();

        out.extend(muxer.push_video(&H264Data::SequenceHeader(avc_config()), 0));
        out.extend(muxer.push_audio(&AacData::SequenceHeader(aac_config()), 0));
        out.extend(muxer.push_video(
            &H264Data::Frame {
                keyframe: true,
                composition_time: 0,
                nalus: Bytes::from_static(&[0x00, 0x00, 0x00, 0x05, 0x65, 0x88, 0x84, 0x21, 0xa0]),
            },
            40,
        ));
        out.extend(muxer.push_audio(
            &AacData::Frame {
                data: Bytes::from_static(&[0x21, 0x10, 0x56, 0xe5]),
            },
            42,
        ));

        assert!(!out.is_empty());
        assert_eq!(out.len() % ace_media::mpegts::TS_PACKET_LEN, 0);
        assert!(out.chunks(188).all(|packet| packet[0] == 0x47));

        let adts = generate_adts_header(&aac_config(), 4);
        assert!(
            out.windows(adts.len()).any(|window| window == adts),
            "AAC frames must be wrapped in ADTS before MPEG-TS muxing"
        );
        assert!(
            out.windows(4)
                .any(|window| window == [0x00, 0x00, 0x00, 0x01]),
            "H.264 frames must be converted to Annex B start-code form"
        );
    }

    #[test]
    fn psi_section_lengths_match_encoded_payloads() {
        for section in [pat_section(), pmt_section()] {
            let section_length = (usize::from(section[2] & 0x0f) << 8) | usize::from(section[3]);
            assert_eq!(
                section_length,
                section.len() - 4,
                "PSI section_length must cover bytes after length field through CRC"
            );
        }
    }

    fn video_frame(keyframe: bool, composition_time: i32) -> H264Data {
        H264Data::Frame {
            keyframe,
            composition_time,
            nalus: Bytes::from_static(&[0x00, 0x00, 0x00, 0x05, 0x65, 0x88, 0x84, 0x21, 0xa0]),
        }
    }

    fn payload(packet: &[u8]) -> &[u8] {
        let offset = if packet[3] & 0x20 != 0 {
            5 + usize::from(packet[4])
        } else {
            4
        };
        &packet[offset..]
    }

    fn read_timestamp(data: &[u8]) -> u64 {
        (u64::from(data[0] & 0x0e) << 29)
            | (u64::from(data[1]) << 22)
            | (u64::from(data[2] & 0xfe) << 14)
            | (u64::from(data[3]) << 7)
            | u64::from(data[4] >> 1)
    }

    #[test]
    fn video_preserves_signed_composition_offsets_including_zero_crossing() {
        for (dts_ms, offset_ms, expected_pts) in [
            (100, -40, 5_400),
            (20, -40, (1 << 33) - 1_800),
            (100, 40, 12_600),
            (u32::MAX, 40, ((u64::from(u32::MAX) + 40) * 90) % (1 << 33)),
        ] {
            let mut muxer = RtmpTsMuxer::new();
            muxer.push_video(&H264Data::SequenceHeader(avc_config()), 0);
            let bytes = muxer.push_video(&video_frame(true, offset_ms), dts_ms);
            let packet = bytes
                .as_chunks::<188>()
                .0
                .iter()
                .find(|packet| ace_media::mpegts::ts_pid(*packet) == VIDEO_PID)
                .unwrap();
            let pes = payload(packet);
            assert_eq!(pes[7], 0xc0);
            assert_eq!(read_timestamp(&pes[9..14]), expected_pts);
            assert_eq!(read_timestamp(&pes[14..19]), pts90(dts_ms) % (1 << 33));
        }
    }

    #[test]
    fn late_join_keyframe_contains_tables_and_advancing_pcr() {
        use ace_media::mpegts::{ts_timing, SelectedPcrClock, VideoAccessPointState};
        let mut muxer = RtmpTsMuxer::new();
        muxer.push_video(&H264Data::SequenceHeader(avc_config()), 0);
        // A joining player never receives this first frame or its original tables.
        muxer.push_video(&video_frame(true, 0), 0);
        let mut access = VideoAccessPointState::new();
        let mut clock = SelectedPcrClock::new();
        let mut found_access = false;
        for timestamp in [2_000, 2_040, 2_080, 2_120] {
            let bytes = muxer.push_video(&video_frame(timestamp == 2_000, 0), timestamp);
            for packet in bytes.as_chunks::<188>().0.iter() {
                found_access |= access.observe(packet);
                clock.observe(ts_timing(packet));
            }
        }
        assert!(
            found_access,
            "a late join needs matching tables and a video access point"
        );
        assert!(access.table_prefix().is_some());
        assert_eq!(clock.elapsed(), Some(std::time::Duration::from_millis(120)));
    }

    #[test]
    fn pcr_adaptation_preserves_pes_payload_and_continuity() {
        for length in [1, 175, 176, 177, 183, 184, 185, 500] {
            let input = vec![0x55; length];
            let mut output = Vec::new();
            let mut cc = 15;
            write_ts_payload_with_clock(
                &mut output,
                VIDEO_PID,
                &mut cc,
                &input,
                Some((1 << 33) + 90),
                true,
            );
            let mut recovered = Vec::new();
            for (index, packet) in output.as_chunks::<188>().0.iter().enumerate() {
                assert_eq!(packet[3] & 0xf, ((15 + index) & 0xf) as u8);
                assert_eq!(packet[1] & 0x40 != 0, index == 0);
                let timing = ace_media::mpegts::ts_timing(packet);
                assert_eq!(timing.pcr, (index == 0).then_some(90));
                recovered.extend_from_slice(payload(packet));
            }
            assert_eq!(output.len() % 188, 0);
            assert_eq!(recovered, input);
        }
    }

    #[test]
    fn audio_pes_length_matches_payload_and_rejects_adts_overflow() {
        let mut muxer = RtmpTsMuxer::new();
        muxer.push_audio(&AacData::SequenceHeader(aac_config()), 0);
        let bytes = muxer.push_audio(
            &AacData::Frame {
                data: Bytes::from(vec![0x55; MAX_ADTS_PAYLOAD]),
            },
            0,
        );
        let pes: Vec<_> = bytes
            .as_chunks::<188>()
            .0
            .iter()
            .filter(|packet| ace_media::mpegts::ts_pid(*packet) == AUDIO_PID)
            .flat_map(|packet| payload(packet))
            .copied()
            .collect();
        assert_eq!(
            usize::from(u16::from_be_bytes([pes[4], pes[5]])),
            pes.len() - 6
        );
        let adts = &pes[14..];
        let frame_length = (usize::from(adts[3] & 3) << 11)
            | (usize::from(adts[4]) << 3)
            | usize::from(adts[5] >> 5);
        assert_eq!(frame_length, MAX_ADTS_PAYLOAD + 7);
        assert!(muxer
            .push_audio(
                &AacData::Frame {
                    data: Bytes::from(vec![0x55; MAX_ADTS_PAYLOAD + 1])
                },
                20
            )
            .is_empty());
    }
}
