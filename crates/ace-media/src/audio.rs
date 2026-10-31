//! Conservative, single-packet PSI normalization for live HLS only.
//! Unsupported PSI disables selection; malformed/non-current tables never replace good metadata.
use crate::mpegts::{mpeg_crc32, ts_pid, ts_timing};

const N: usize = 188;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
    AdtsAac,
    LatmAac,
    MpegAudio,
    Ac3,
    Eac3,
    Dts,
}

/// Strict normalization policy, independent of the legacy packet filter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LayoutDecision {
    #[default]
    Pending,
    Supported(TransportLayout),
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedAudio {
    pub pid: u16,
    pub codec: AudioCodec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportLayout {
    pub transport_stream_id: u16,
    pub program_id: u16,
    pub pmt_pid: u16,
    pub pcr_pid: u16,
    pub video_pid: u16,
    pub video_type: u8,
    pub audio: Option<SelectedAudio>,
    /// Source PSI semantics, excluding version, CRC and packet framing/continuity.
    pub semantic_identity: Vec<u8>,
}

/// Validated PMT entry shared by packet filtering and strict layout classification.
struct EsEntry {
    start: usize,
    end: usize,
    pid: u16,
    codec: Option<AudioCodec>,
    language: Option<LanguageTag>,
}

#[derive(Clone, Copy)]
struct LanguageTag {
    code: [u8; 3],
    audio_type: u8,
}

impl LanguageTag {
    fn is_accessibility(self) -> bool {
        // ISO 639 audio_type: hearing impaired, visual-impaired commentary.
        matches!(self.audio_type, 0x02 | 0x03)
    }
}

/// Select the same native ADTS AAC track for the rewritten PMT and the layout.
/// Prefer primary mixes, then the operator's language order, then PMT order.
/// A missing language never removes all audio; accessibility-only muxes retain one track.
fn natively_copied_audio(
    entries: &[EsEntry],
    preferred_languages: &[[u8; 3]],
) -> Option<SelectedAudio> {
    let aac = || {
        entries
            .iter()
            .filter(|e| e.codec == Some(AudioCodec::AdtsAac))
    };
    let has_primary = aac().any(|e| !e.language.is_some_and(LanguageTag::is_accessibility));
    let eligible =
        || aac().filter(|e| !has_primary || !e.language.is_some_and(LanguageTag::is_accessibility));
    let chosen = preferred_languages
        .iter()
        .find_map(|language| eligible().find(|e| e.language.map(|tag| tag.code) == Some(*language)))
        .or_else(|| eligible().next())?;
    Some(SelectedAudio {
        pid: chosen.pid,
        codec: AudioCodec::AdtsAac,
    })
}

#[derive(Default)]
struct DescriptorCodecs {
    // Native selection historically uses the first recognized signal.
    first: Option<AudioCodec>,
    language: Option<LanguageTag>,
    conflicting: bool,
}

/// Validate once, retaining both legacy first-match and strict ambiguity evidence.
fn descriptors(mut bytes: &[u8]) -> Option<DescriptorCodecs> {
    let mut codec = None;
    let mut language = None;
    let mut conflicting = false;
    while !bytes.is_empty() {
        let len = *bytes.get(1)? as usize;
        let body = bytes.get(2..2 + len)?;
        // A descriptor consists of four-byte language/audio_type entries. Use its first
        // language as the track's label; malformed language metadata supplies no preference.
        if bytes[0] == 0x0a && body.len() >= 4 && body.len() % 4 == 0 && language.is_none() {
            language = Some(LanguageTag {
                code: [
                    body[0].to_ascii_lowercase(),
                    body[1].to_ascii_lowercase(),
                    body[2].to_ascii_lowercase(),
                ],
                audio_type: body[3],
            });
        }
        let signal = match bytes[0] {
            0x6a if body
                .first()
                .is_some_and(|flags| body.len() > (flags & 0xf0).count_ones() as usize) =>
            {
                Some(AudioCodec::Ac3)
            }
            0x7a if body
                .first()
                .is_some_and(|flags| body.len() > (flags & 0xf7).count_ones() as usize) =>
            {
                Some(AudioCodec::Eac3)
            }
            0x7b if body.len() >= 5 => Some(AudioCodec::Dts),
            0x05 if body.len() >= 4 => match &body[..4] {
                b"AC-3" => Some(AudioCodec::Ac3),
                b"EAC3" => Some(AudioCodec::Eac3),
                b"DTS1" | b"DTS2" | b"DTS3" => Some(AudioCodec::Dts),
                _ => None,
            },
            _ => None,
        };
        if let Some(signal) = signal {
            conflicting |= codec.is_some_and(|first| first != signal);
            codec = codec.or(Some(signal));
        }
        bytes = &bytes[2 + len..];
    }
    Some(DescriptorCodecs {
        first: codec,
        language,
        conflicting,
    })
}

fn audio_codec(ty: u8, desc: &[u8]) -> Option<DescriptorCodecs> {
    let mut private = descriptors(desc)?;
    let implied = match ty {
        0x0f => Some(AudioCodec::AdtsAac),
        0x11 => Some(AudioCodec::LatmAac),
        0x03 | 0x04 => Some(AudioCodec::MpegAudio),
        0x06 => private.first,
        _ => None,
    };
    // Typed audio and private codec descriptors must agree. Private streams (0x06)
    // deliberately take their codec from the descriptor instead.
    if matches!(ty, 0x0f | 0x11 | 0x03 | 0x04) {
        private.conflicting |= private.first.is_some_and(|codec| Some(codec) != implied);
    }
    private.first = implied;
    Some(private)
}

#[derive(Debug)]
enum PsiError {
    Invalid,
    Unsupported,
}

fn payload(packet: &[u8; N]) -> Option<usize> {
    match packet[3] & 0x30 {
        0x10 => Some(4),
        0x30 => (5 + (packet[4] as usize) < N).then_some(5 + packet[4] as usize),
        _ => None,
    }
}

fn section(packet: &[u8; N], id: u8, min: usize, program: Option<u16>) -> Result<&[u8], PsiError> {
    use PsiError::*;
    if packet[0] != 0x47 || packet[1] & 0x80 != 0 || packet[3] & 0xc0 != 0 {
        return Err(Invalid);
    }
    let off = payload(packet).ok_or(Invalid)?;
    if packet[1] & 0x40 == 0 {
        return Err(Unsupported);
    }
    let start = off + 1 + packet[off] as usize;
    let header = packet.get(start..start + 3).ok_or(Invalid)?;
    if header[0] != id || header[1] & 0xf0 != 0xb0 {
        return Err(Invalid);
    }
    let len = (((header[1] & 15) as usize) << 8) | header[2] as usize;
    if len < min || len > 1021 {
        return Err(Invalid);
    }
    // Reject a header for another program or a future table before classifying a fragment as
    // unsupported. Neither is evidence that our current program changed transport layout.
    if packet.get(start + 5).is_none_or(|flags| flags & 1 == 0) {
        return Err(Invalid);
    }
    if let Some(program) = program {
        let bytes = packet.get(start + 3..start + 5).ok_or(Invalid)?;
        if u16::from_be_bytes([bytes[0], bytes[1]]) != program {
            return Err(Invalid);
        }
    }
    let end = start + 3 + len;
    let s = packet.get(start..end).ok_or(Unsupported)?;
    if s[5] & 1 == 0 || mpeg_crc32(s) != 0 {
        return Err(Invalid);
    }
    if s[6] != 0 || s[7] != 0 || packet[off] != 0 || packet[end..].iter().any(|b| *b != 0xff) {
        return Err(Unsupported);
    }
    Ok(s)
}

fn pid(bytes: &[u8]) -> u16 {
    ((bytes[0] as u16 & 31) << 8) | bytes[1] as u16
}

/// Result of observing one source packet. `changed` requires discarding an unfinished HLS run
/// and refreshing the video/table cache from `tables` before accepting another access point.
pub struct AudioPacket {
    pub packet: Option<[u8; N]>,
    pub changed: bool,
}

#[derive(Default)]
pub struct LiveAudioSelection {
    preferred_languages: Vec<[u8; 3]>,
    layout: LayoutDecision,
    mapping: Option<(u16, u16)>,
    transport_stream_id: Option<u16>,
    pat: Option<[u8; N]>,
    pmt: Option<[u8; N]>,
    source_pmt: Option<Vec<u8>>,
    removed: Vec<u16>,
    pcr: Option<u16>,
    effective: Option<Vec<u8>>,
    version: u8,
    rewrite: bool,
    output_cc: u8,
}

impl LiveAudioSelection {
    /// Immutable per-stream preference, in priority order, for native ADTS AAC tracks.
    /// Absent matches fall back to the first primary AAC track in PMT order.
    pub fn with_preferred_languages(languages: Vec<[u8; 3]>) -> Self {
        Self {
            preferred_languages: languages,
            ..Self::default()
        }
    }

    pub fn layout(&self) -> LayoutDecision {
        self.layout.clone()
    }

    pub fn reset(&mut self) {
        let preferred_languages = std::mem::take(&mut self.preferred_languages);
        *self = Self::with_preferred_languages(preferred_languages);
    }

    pub fn pcr_pid(&self) -> Option<u16> {
        self.pcr
    }

    pub fn tables(&self) -> impl Iterator<Item = &[u8; N]> {
        self.pat.iter().chain(self.pmt.iter())
    }

    fn disable(&mut self) -> bool {
        let changed = self.pmt.is_some() || !self.removed.is_empty();
        self.pmt = None;
        self.source_pmt = None;
        self.removed.clear();
        self.pcr = None;
        self.effective = None;
        self.rewrite = false;
        changed
    }

    pub fn observe(&mut self, mut packet: [u8; N]) -> AudioPacket {
        // Inspect the original packet, including payload on PIDs the native filter drops.
        // This metadata never changes the legacy packet return value or rewrite rules.
        if packet[3] & 0xc0 != 0 {
            self.layout = LayoutDecision::Unsupported;
        }
        let source_pid = ts_pid(&packet);
        let mut changed = false;
        if source_pid == 0 {
            match section(&packet, 0, 9, None) {
                Ok(s) => {
                    let entries = &s[8..s.len() - 4];
                    if entries.len() % 4 != 0 {
                        return AudioPacket {
                            packet: None,
                            changed: false,
                        };
                    }
                    let programs: Vec<_> = entries
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .filter(|e| e[0] != 0 || e[1] != 0)
                        .map(|e| (u16::from_be_bytes([e[0], e[1]]), pid(&e[2..])))
                        .collect();
                    if programs.len() != 1 {
                        self.layout = LayoutDecision::Unsupported;
                        changed = self.disable();
                        self.mapping = None;
                        self.transport_stream_id = None;
                        self.pat = None;
                    } else {
                        let transport_stream_id = u16::from_be_bytes([s[3], s[4]]);
                        if self.mapping != Some(programs[0])
                            || self.transport_stream_id != Some(transport_stream_id)
                        {
                            self.layout = LayoutDecision::Pending;
                            changed = self.disable();
                            self.mapping = Some(programs[0]);
                            self.transport_stream_id = Some(transport_stream_id);
                        }
                        self.pat = Some(packet);
                        if programs[0].1 < 0x20 || programs[0].1 == 0x1fff {
                            self.layout = LayoutDecision::Unsupported;
                        }
                    }
                }
                Err(PsiError::Unsupported) => {
                    self.layout = LayoutDecision::Unsupported;
                    changed = self.disable();
                    self.mapping = None;
                    self.transport_stream_id = None;
                    self.pat = None;
                }
                Err(PsiError::Invalid) => {
                    // Preserve legacy passthrough until strict PSI has been established.
                    if self.pat.is_some() {
                        return AudioPacket {
                            packet: None,
                            changed: false,
                        };
                    }
                }
            }
        } else if self.mapping.is_some_and(|(_, p)| p == source_pid) && payload(&packet).is_some() {
            match self.update_pmt(&packet) {
                Ok(different) => {
                    changed = different;
                    if let Some(pmt) = self.pmt {
                        packet = pmt;
                    }
                }
                Err(PsiError::Unsupported) => {
                    self.layout = LayoutDecision::Unsupported;
                    changed = self.disable();
                }
                Err(PsiError::Invalid) => {
                    return AudioPacket {
                        packet: None,
                        changed: false,
                    };
                }
            }
        } else if self.removed.contains(&source_pid) {
            let timing = ts_timing(&packet);
            if Some(source_pid) == self.pcr && (timing.pcr.is_some() || timing.discontinuity) {
                // Retain the full bounded adaptation field, replacing PES with stuffing.
                let len = packet[4] as usize;
                if packet[3] & 0x20 == 0 || len == 0 || len > 183 {
                    return AudioPacket {
                        packet: None,
                        changed,
                    };
                }
                packet[1] &= !0x40;
                packet[3] = 0x20;
                packet[4] = 183;
                packet[5 + len..].fill(0xff);
            } else {
                return AudioPacket {
                    packet: None,
                    changed,
                };
            }
        }
        AudioPacket {
            packet: Some(packet),
            changed,
        }
    }

    fn update_pmt(&mut self, packet: &[u8; N]) -> Result<bool, PsiError> {
        use PsiError::*;
        let s = section(packet, 2, 13, self.mapping.map(|m| m.0))?;
        if Some(u16::from_be_bytes([s[3], s[4]])) != self.mapping.map(|m| m.0) {
            return Err(Invalid);
        }
        let end = s.len() - 4;
        let info = (((s[10] & 15) as usize) << 8) | s[11] as usize;
        let begin = 12 + info;
        if begin > end {
            return Err(Invalid);
        }
        let mut conflicting = descriptors(s.get(12..begin).ok_or(Invalid)?)
            .ok_or(Invalid)?
            .conflicting;
        let mut pos = begin;
        let mut entries = Vec::new();
        while pos < end {
            let h = s
                .get(pos..pos + 5)
                .filter(|_| pos + 5 <= end)
                .ok_or(Invalid)?;
            let len = (((h[3] & 15) as usize) << 8) | h[4] as usize;
            let next = pos + 5 + len;
            if next > end {
                return Err(Invalid);
            }
            let codec = audio_codec(h[0], &s[pos + 5..next]).ok_or(Invalid)?;
            conflicting |= codec.conflicting;
            let es_pid = pid(&h[1..]);
            if entries.iter().any(|entry: &EsEntry| entry.pid == es_pid) {
                return Err(Unsupported);
            }
            entries.push(EsEntry {
                start: pos,
                end: next,
                pid: es_pid,
                codec: codec.first,
                language: codec.language,
            });
            pos = next;
        }
        self.layout = if conflicting {
            LayoutDecision::Unsupported
        } else {
            self.decide_layout(s, &entries)
        };
        let selected =
            natively_copied_audio(&entries, &self.preferred_languages).map(|audio| audio.pid);
        let removed: Vec<_> = entries
            .iter()
            .filter(|e| selected.is_some() && e.codec.is_some() && Some(e.pid) != selected)
            .map(|e| e.pid)
            .collect();
        let mut out = s[..begin].to_vec();
        for entry in entries {
            if !removed.contains(&entry.pid) {
                out.extend_from_slice(&s[entry.start..entry.end]);
            }
        }
        let len = out.len() + 1; // excludes first three bytes, includes CRC
        out[1] = 0xb0 | ((len >> 8) as u8 & 15);
        out[2] = len as u8;
        let upstream_version = (s[5] >> 1) & 31;
        out[5] &= 0xc1; // effective content comparison excludes upstream version and CRC
        let mut source_content = s[..end].to_vec();
        source_content[5] &= 0xc1;
        let different = self.source_pmt.as_ref() != Some(&source_content);
        if self.effective.as_ref() != Some(&out) {
            self.version = if self.effective.is_some() {
                (self.version + 1) & 31
            } else {
                (upstream_version + u8::from(!removed.is_empty())) & 31
            };
        }
        self.effective = Some(out.clone());
        self.rewrite = selected.is_some();
        if !self.rewrite {
            // Passthrough restores the source version too; use it as the baseline if AAC returns.
            self.version = upstream_version;
        }
        out[5] |= self.version << 1;
        let crc = mpeg_crc32(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        let mut rewritten = [0xff; N];
        rewritten[..4].copy_from_slice(&packet[..4]);
        rewritten[3] = (rewritten[3] & 0x0f) | 0x10;
        rewritten[4] = 0;
        rewritten[5..5 + out.len()].copy_from_slice(&out);
        self.pmt = Some(if self.rewrite { rewritten } else { *packet });
        self.source_pmt = Some(source_content);
        self.removed = removed;
        self.pcr = Some(pid(&s[8..]));
        Ok(different)
    }

    /// Consume the same validated descriptor/ES parse as the native rewriter.
    fn decide_layout(&self, s: &[u8], entries: &[EsEntry]) -> LayoutDecision {
        use LayoutDecision::*;
        let (Some((program_id, pmt_pid)), Some(transport_stream_id)) =
            (self.mapping, self.transport_stream_id)
        else {
            return Pending;
        };
        if pmt_pid < 0x20 || pmt_pid == 0x1fff {
            return Unsupported;
        }
        let pcr_pid = pid(&s[8..]);
        if !entries.iter().any(|e| e.pid == pcr_pid)
            || entries
                .iter()
                .any(|e| e.pid < 0x20 || e.pid == 0x1fff || e.pid == pmt_pid)
        {
            return Unsupported;
        }
        let mut video = None;
        let mut first_audio = None;
        let mut has_audio = false;
        for entry in entries {
            let es_pid = entry.pid;
            let codec = entry.codec;
            let ty = s[entry.start];
            if matches!(ty, 0x02 | 0x1b | 0x24) {
                if video.replace((es_pid, ty)).is_some() {
                    return Unsupported;
                }
            } else if !matches!(ty, 0x03 | 0x04 | 0x06 | 0x0f | 0x11
                | 0x05 | 0x0a..=0x0e | 0x15..=0x19)
            {
                // Only known audio or data (private sections, DSM-CC, auxiliary,
                // metadata) may accompany video. Unknown types can contain A/V.
                return Unsupported;
            }
            if ty == 0x06 && codec.is_none() {
                return Unsupported;
            }
            if let Some(codec) = codec {
                has_audio = true;
                if matches!(codec, AudioCodec::MpegAudio | AudioCodec::Ac3) && first_audio.is_none()
                {
                    first_audio = Some(SelectedAudio { pid: es_pid, codec });
                }
            }
        }
        let Some((video_pid, video_type)) = video else {
            return Unsupported;
        };
        let audio = natively_copied_audio(entries, &self.preferred_languages).or(first_audio);
        if has_audio && audio.is_none() {
            return Unsupported;
        }
        let mut semantic_identity = Vec::with_capacity(s.len() + 4);
        semantic_identity.extend_from_slice(&transport_stream_id.to_be_bytes());
        semantic_identity.extend_from_slice(&pmt_pid.to_be_bytes());
        semantic_identity.extend_from_slice(&s[..s.len() - 4]);
        semantic_identity[9] &= 0xc1; // PMT byte 5, after the four identity bytes
        Supported(TransportLayout {
            transport_stream_id,
            program_id,
            pmt_pid,
            pcr_pid,
            video_pid,
            video_type,
            audio,
            semantic_identity,
        })
    }

    /// Assign continuity to actual emitted PMT payloads, including injected prefixes.
    pub fn emitted(&mut self, bytes: &mut [u8]) {
        if !self.rewrite {
            return;
        }
        for packet in bytes.as_chunks_mut::<N>().0 {
            if self.mapping.is_some_and(|(_, p)| p == ts_pid(packet)) {
                let cc = if packet[3] & 0x10 != 0 {
                    let cc = self.output_cc;
                    self.output_cc = (self.output_cc + 1) & 15;
                    cc
                } else {
                    self.output_cc.wrapping_sub(1) & 15
                };
                packet[3] = (packet[3] & 0xf0) | cc;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestEntries<'a> = &'a [(u8, u16, &'a [u8])];
    type AudioSelectionCase<'a> = (TestEntries<'a>, u16, AudioCodec);
    type RejectionCase<'a> = (&'a str, TestEntries<'a>);

    fn table(pid: u16, mut s: Vec<u8>) -> [u8; N] {
        let len = s.len() + 1;
        s[1] = 0xb0 | (len >> 8) as u8;
        s[2] = len as u8;
        s.extend_from_slice(&mpeg_crc32(&s).to_be_bytes());
        let mut p = [0xff; N];
        p[..5].copy_from_slice(&[0x47, 0x40 | (pid >> 8) as u8, pid as u8, 0x10, 0]);
        p[5..5 + s.len()].copy_from_slice(&s);
        p
    }
    fn pat(program: u16, pmt: u16) -> [u8; N] {
        table(
            0,
            vec![
                0,
                0xb0,
                0,
                0,
                1,
                0xc1,
                0,
                0,
                (program >> 8) as u8,
                program as u8,
                0xe0 | (pmt >> 8) as u8,
                pmt as u8,
            ],
        )
    }
    fn pmt(version: u8, entries: &[(u8, u16, &[u8])]) -> [u8; N] {
        let mut s = vec![
            2,
            0xb0,
            0,
            0,
            1,
            0xc1 | version << 1,
            0,
            0,
            0xe1,
            1,
            0xf0,
            3,
            0x52,
            1,
            0xab,
        ];
        for (ty, p, d) in entries {
            s.extend_from_slice(&[*ty, 0xe0 | (p >> 8) as u8, *p as u8, 0xf0, d.len() as u8]);
            s.extend_from_slice(d);
        }
        table(256, s)
    }
    fn source() -> [u8; N] {
        pmt(
            31,
            &[
                (0x1b, 257, &[]),
                (3, 258, &[]),
                (15, 259, &[0x0a, 4, b'e', b'n', b'g', 0]),
            ],
        )
    }
    fn state() -> LiveAudioSelection {
        let mut st = LiveAudioSelection::default();
        st.observe(pat(1, 256));
        st.observe(source());
        st
    }
    fn es_pids(packet: &[u8; N]) -> Vec<u16> {
        let s = section(packet, 2, 13, None).unwrap();
        let mut pos = 12 + (((s[10] & 15) as usize) << 8 | s[11] as usize);
        let mut out = Vec::new();
        while pos < s.len() - 4 {
            out.push(pid(&s[pos + 1..]));
            pos += 5 + (((s[pos + 3] & 15) as usize) << 8 | s[pos + 4] as usize);
        }
        out
    }
    fn elementary(pid: u16) -> [u8; N] {
        let mut p = [0xff; N];
        p[..4].copy_from_slice(&[0x47, (pid >> 8) as u8, pid as u8, 0x10]);
        p
    }
    fn repair(mut p: [u8; N]) -> [u8; N] {
        let end = 8 + (((p[6] & 15) as usize) << 8 | p[7] as usize);
        let crc = mpeg_crc32(&p[5..end - 4]);
        p[end - 4..end].copy_from_slice(&crc.to_be_bytes());
        p
    }

    #[test]
    fn pat_transport_switch_invalidates_selection_but_version_repeat_does_not() {
        let mut st = state();
        let cached = st.pmt;
        let mut repeated = pat(1, 256);
        repeated[10] = 0xc7;
        repeated = repair(repeated);
        assert!(!st.observe(repeated).changed);
        assert_eq!(st.pmt, cached);
        let mut switched = repeated;
        switched[9] = 2; // same program/PMT mapping, different transport_stream_id
        switched = repair(switched);
        assert!(st.observe(switched).changed);
        assert!(st.pmt.is_none());
        assert_eq!(st.pcr_pid(), None);
        assert_eq!(st.tables().copied().collect::<Vec<_>>(), vec![switched]);
        let audio = elementary(258);
        assert_eq!(st.observe(audio).packet, Some(audio));
        let fresh = pmt(17, &[(0x1b, 257, &[]), (3, 258, &[])]);
        assert_eq!(st.observe(fresh).packet, Some(fresh));
        assert_eq!(
            st.tables().copied().collect::<Vec<_>>(),
            vec![switched, fresh]
        );
    }

    #[test]
    fn losing_adts_restores_original_pmt_adaptation_version_and_es_passthrough() {
        let mut st = state();
        let plain = pmt(
            17,
            &[
                (0x1b, 257, &[]),
                (3, 258, &[]),
                (0x11, 259, &[]),
                (6, 260, &[]),
            ],
        );
        let mut original = [0xff; N];
        original[..4].copy_from_slice(&plain[..4]);
        original[3] = 0x39;
        original[4] = 2;
        original[5] = 0;
        original[7..].copy_from_slice(&plain[4..N - 3]);
        let result = st.observe(original);
        assert!(result.changed);
        assert_eq!(result.packet, Some(original));
        assert_eq!(st.pmt, Some(original));
        let mut emitted = original;
        st.emitted(&mut emitted);
        assert_eq!(emitted, original);
        let repeat = st.observe(original);
        assert!(!repeat.changed);
        assert_eq!(repeat.packet, Some(original));
        for pid in [258, 259, 260] {
            let audio = elementary(pid);
            assert_eq!(st.observe(audio).packet, Some(audio));
        }
    }

    #[test]
    fn selection_preserves_program_and_retained_es_descriptors() {
        let mut st = state();
        let out = st.observe(source()).packet.unwrap();
        assert_eq!(es_pids(&out), vec![257, 259]);
        let s = section(&out, 2, 13, None).unwrap();
        assert_eq!(&s[12..15], &[0x52, 1, 0xab]);
        assert_eq!(&s[25..31], &[0x0a, 4, b'e', b'n', b'g', 0]);
        assert_eq!((s[5] >> 1) & 31, 0);
        assert!(st.observe(elementary(258)).packet.is_none());
        assert!(st.observe(elementary(259)).packet.is_some());
    }

    #[test]
    fn private_descriptor_signals_are_bounded_and_unknown_private_survives() {
        let cases: &[(u8, &[u8], bool)] = &[
            (6, &[], true),
            (6, &[0x6a, 0], true),
            (6, &[0x6a, 1, 0], false),
            (6, &[0x7a, 1, 0], false),
            (6, &[0x7b, 5, 0, 0, 0, 0, 0], false),
            (6, &[5, 4, b'A', b'C', b'-', b'3'], false),
            (6, &[5, 4, b'E', b'A', b'C', b'3'], false),
            (6, &[5, 4, b'D', b'T', b'S', b'1'], false),
            (6, &[5, 4, b'N', b'O', b'N', b'E'], true),
            (0x11, &[], false),
        ];
        for (ty, desc, retained) in cases {
            let mut st = LiveAudioSelection::default();
            st.observe(pat(1, 256));
            let out = st
                .observe(pmt(0, &[(15, 259, &[]), (*ty, 258, desc)]))
                .packet
                .unwrap();
            assert_eq!(
                es_pids(&out).contains(&258),
                *retained,
                "descriptor {desc:?}"
            );
        }
    }

    #[test]
    fn invalid_tables_cannot_replace_known_good_selection() {
        let mutations: &[fn(&mut [u8; N])] = &[
            |p| p[25] ^= 1, // CRC corruption
            |p| {
                p[9] = 2;
                *p = repair(*p);
            }, // wrong program
            |p| {
                p[10] &= !1;
                *p = repair(*p);
            }, // not current
            |p| p[4] = 184, // pointer outside packet
            |p| {
                p[18] = 180;
                *p = repair(*p);
            }, // program descriptor bounds
            |p| {
                p[34] = 255;
                *p = repair(*p);
            }, // ES descriptor length bounds
        ];
        for (i, mutate) in mutations.iter().enumerate() {
            let mut st = state();
            let before = st.pmt;
            let mut bad = source();
            mutate(&mut bad);
            let result = st.observe(bad);
            assert!(!result.changed, "mutation {i}");
            assert_eq!(st.pmt, before);
            assert!(st.observe(elementary(258)).packet.is_none());
        }
    }

    #[test]
    fn unsupported_psi_disables_filter_and_drops_cached_rewrite() {
        let mutations: &[fn(&mut [u8; N])] = &[
            |p| p[1] &= !0x40, // continuation
            |p| {
                p[7] = 200;
            }, // fragmented section
            |p| {
                p[11] = 1;
                *p = repair(*p);
            }, // section_number
            |p| {
                p[12] = 1;
                *p = repair(*p);
            }, // last_section_number
            |p| {
                let end = 8 + p[7] as usize;
                p[end] = 0;
            }, // another section after CRC
        ];
        for mutate in mutations {
            let mut st = state();
            let mut bad = source();
            mutate(&mut bad);
            assert!(st.observe(bad).changed);
            assert!(st.pmt.is_none());
            assert!(st.observe(elementary(258)).packet.is_some());
        }
        let mut st = state();
        let multiple = table(
            0,
            vec![0, 0xb0, 0, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0, 0, 2, 0xe2, 0],
        );
        assert!(st.observe(multiple).changed);
        assert!(st.mapping.is_none());
        assert!(st.pmt.is_none());
    }

    #[test]
    fn remap_and_program_change_require_fresh_matching_pmt() {
        for mapping in [(1, 300), (2, 256)] {
            let mut st = state();
            assert!(st.observe(pat(mapping.0, mapping.1)).changed);
            assert!(st.pmt.is_none());
            assert!(st.removed.is_empty());
            st.observe(source());
            assert!(st.pmt.is_none());
        }
    }

    #[test]
    fn repeats_keep_version_and_content_changes_increment_modulo_32() {
        let mut st = state();
        let first = st.pmt.unwrap();
        assert!(!st.observe(source()).changed);
        assert_eq!(st.pmt.unwrap(), first);
        let changed = pmt(31, &[(0x1b, 257, &[]), (3, 258, &[]), (15, 260, &[])]);
        assert!(st.observe(changed).changed);
        assert_eq!((st.pmt.unwrap()[10] >> 1) & 31, 1);
        let version_only = pmt(0, &[(0x1b, 257, &[]), (3, 258, &[]), (15, 260, &[])]);
        assert!(!st.observe(version_only).changed);
        assert_eq!((st.pmt.unwrap()[10] >> 1) & 31, 1);
        let removed = pmt(1, &[(0x1b, 257, &[]), (3, 258, &[])]);
        assert!(st.observe(removed).changed);
        assert!(st.removed.is_empty());
        assert_eq!(es_pids(&st.pmt.unwrap()), vec![257, 258]);
    }

    #[test]
    fn wrong_program_or_noncurrent_fragment_cannot_disable_known_good_selection() {
        for noncurrent in [false, true] {
            let mut st = state();
            let before = st.pmt;
            let mut bad = source();
            bad[7] = 200; // unsupported fragmented length
            if noncurrent {
                bad[10] &= !1;
            } else {
                bad[9] = 2;
            }
            let result = st.observe(bad);
            assert!(!result.changed);
            assert_eq!(st.pmt, before);
            assert!(st.observe(elementary(258)).packet.is_none());
        }
    }

    #[test]
    fn single_adts_track_still_counts_injected_pmt_payloads() {
        let mut st = LiveAudioSelection::default();
        st.observe(pat(1, 256));
        let original = pmt(7, &[(0x1b, 257, &[]), (15, 259, &[])]);
        let normalized = st.observe(original).packet.unwrap();
        assert_eq!((normalized[10] >> 1) & 31, 7); // unchanged content retains source version
        let mut emitted = normalized.repeat(2);
        st.emitted(&mut emitted);
        assert_eq!(emitted[N + 3] & 15, 1);
    }

    #[test]
    fn no_audio_or_no_adts_keeps_original_table_and_es_packets() {
        for entries in [
            vec![(0x1b, 257, &[][..])],
            vec![(0x11, 258, &[][..]), (3, 259, &[][..]), (6, 260, &[][..])],
        ] {
            let mut st = LiveAudioSelection::default();
            st.observe(pat(1, 256));
            let original = pmt(3, &entries);
            assert_eq!(st.observe(original).packet, Some(original));
            for (_, pid, _) in entries {
                let input = elementary(pid);
                assert_eq!(st.observe(input).packet, Some(input));
            }
        }
    }

    #[test]
    fn pmt_adaptation_only_packets_hold_last_emitted_payload_counter() {
        let mut st = state();
        let mut payload = st.pmt.unwrap();
        st.emitted(&mut payload);
        let mut adaptation = elementary(256);
        adaptation[3] = 0x29;
        adaptation[4] = 183;
        adaptation[5] = 0;
        st.emitted(&mut adaptation);
        assert_eq!(adaptation[3] & 15, 0);
        let mut next = st.pmt.unwrap();
        st.emitted(&mut next);
        assert_eq!(next[3] & 15, 1);
    }

    #[test]
    fn emitted_prefix_and_repeated_pmt_share_payload_continuity() {
        let mut st = state();
        let p = st.pmt.unwrap();
        let mut out = p.repeat(18);
        st.emitted(&mut out);
        let cc: Vec<_> = out.as_chunks::<N>().0.iter().map(|p| p[3] & 15).collect();
        assert_eq!(
            cc,
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0, 1]
        );
        let mut next = p;
        st.emitted(&mut next);
        assert_eq!(next[3] & 15, 2);
    }

    fn layout_state(entries: &[(u8, u16, &[u8])]) -> LiveAudioSelection {
        let mut st = LiveAudioSelection::default();
        st.observe(pat(1, 256));
        st.observe(pmt(0, entries));
        st
    }

    fn normalization_policy_layout(entries: &[(u8, u16, &[u8])]) -> LayoutDecision {
        layout_state(entries).layout()
    }

    const ENG: &[u8] = &[0x0a, 4, b'e', b'n', b'g', 0];
    const RUS: &[u8] = &[0x0a, 4, b'r', b'u', b's', 0];
    const ENG_ACCESSIBLE: &[u8] = &[0x0a, 4, b'e', b'n', b'g', 2];

    fn selected_aac_pid(state: &LiveAudioSelection) -> u16 {
        let LayoutDecision::Supported(layout) = state.layout() else {
            panic!("expected a supported layout");
        };
        let audio = layout.audio.expect("selected audio");
        assert_eq!(audio.codec, AudioCodec::AdtsAac);
        audio.pid
    }

    #[test]
    fn aac_language_selection_keeps_layout_packets_and_rewritten_pmt_consistent() {
        let tracks: &[(u8, u16, &[u8])] = &[(0x1b, 257, &[]), (0x0f, 300, RUS), (0x0f, 258, ENG)];
        for (languages, expected) in [
            (vec![], 300),
            (vec![*b"eng"], 258),
            (vec![*b"rus", *b"eng"], 300),
            (vec![*b"deu", *b"eng"], 258),
            (vec![*b"deu"], 300),
        ] {
            let mut state = LiveAudioSelection::with_preferred_languages(languages);
            // Recovery clears media state, but must retain the operator's preference.
            for _ in 0..2 {
                state.observe(pat(1, 256));
                let output = state.observe(pmt(0, tracks)).packet.unwrap();
                assert_eq!(selected_aac_pid(&state), expected);
                assert_eq!(es_pids(&output), vec![257, expected]);
                for pid in [258, 300] {
                    assert_eq!(
                        state.observe(elementary(pid)).packet.is_some(),
                        pid == expected
                    );
                }
                state.reset();
            }
        }
    }

    #[test]
    fn aac_preference_survives_pmt_reordering_and_moves_only_when_the_track_changes() {
        let mut state = LiveAudioSelection::with_preferred_languages(vec![*b"eng"]);
        state.observe(pat(1, 256));
        let original = [(0x1b, 257, &[][..]), (0x0f, 300, RUS), (0x0f, 258, ENG)];
        assert!(state.observe(pmt(0, &original)).changed);
        assert_eq!(selected_aac_pid(&state), 258);
        assert!(
            !state.observe(pmt(1, &original)).changed,
            "version-only PMT does not reset media"
        );
        let reordered = [(0x1b, 257, &[][..]), (0x0f, 258, ENG), (0x0f, 300, RUS)];
        assert!(state.observe(pmt(2, &reordered)).changed);
        assert_eq!(selected_aac_pid(&state), 258);
        let moved = [(0x1b, 257, &[][..]), (0x0f, 260, ENG), (0x0f, 300, RUS)];
        assert!(state.observe(pmt(3, &moved)).changed);
        assert_eq!(selected_aac_pid(&state), 260);
        assert_eq!(es_pids(&state.pmt.unwrap()), vec![257, 260]);
        // A removed preferred track falls back to an available AAC stream.
        assert!(
            state
                .observe(pmt(4, &[(0x1b, 257, &[]), (0x0f, 300, RUS)]))
                .changed
        );
        assert_eq!(selected_aac_pid(&state), 300);
    }

    #[test]
    fn aac_primary_fallback_and_accessibility_only_selection_are_explicit() {
        let mut state = LiveAudioSelection::with_preferred_languages(vec![*b"eng"]);
        state.observe(pat(1, 256));
        state.observe(pmt(
            0,
            &[
                (0x1b, 257, &[]),
                (0x0f, 258, ENG_ACCESSIBLE),
                (0x0f, 300, RUS),
            ],
        ));
        assert_eq!(
            selected_aac_pid(&state),
            300,
            "primary mix takes precedence"
        );
        state.observe(pmt(1, &[(0x1b, 257, &[]), (0x0f, 258, ENG_ACCESSIBLE)]));
        assert_eq!(
            selected_aac_pid(&state),
            258,
            "accessibility-only stream keeps audio"
        );
        // No ISO descriptor, or a malformed language descriptor, falls back to PMT order.
        state.observe(pmt(
            2,
            &[
                (0x1b, 257, &[]),
                (0x0f, 300, &[0x0a, 3, b'e', b'n', b'g']),
                (0x0f, 258, &[]),
            ],
        ));
        assert_eq!(selected_aac_pid(&state), 300);
        // Recognize case consistently with operator configuration.
        state.observe(pmt(
            3,
            &[
                (0x1b, 257, &[]),
                (0x0f, 300, RUS),
                (0x0f, 258, &[0x0a, 4, b'E', b'N', b'G', 0]),
            ],
        ));
        assert_eq!(selected_aac_pid(&state), 258);
    }

    #[test]
    fn typed_audio_cannot_contradict_private_codec_descriptors() {
        for ty in [0x0f, 0x11, 0x03, 0x04] {
            for descriptor in [&[0x6a, 1, 0][..], &[5, 4, b'A', b'C', b'-', b'3'][..]] {
                assert_eq!(
                    normalization_policy_layout(&[(0x1b, 257, &[]), (ty, 258, descriptor)]),
                    LayoutDecision::Unsupported,
                    "stream type {ty:#x} contradicts {descriptor:?}"
                );
            }
        }
        assert_eq!(
            selected_aac_pid(&layout_state(&[(0x1b, 257, &[]), (0x0f, 258, ENG)])),
            258
        );
        assert!(matches!(
            normalization_policy_layout(&[(0x1b, 257, &[]), (0x06, 258, &[0x6a, 1, 0])]),
            LayoutDecision::Supported(_)
        ));
    }

    #[test]
    fn normalization_policy_selects_aac_then_first_supported_audio() {
        let cases: &[AudioSelectionCase<'_>] = &[
            (
                &[
                    (0x1b, 257, &[]),
                    (3, 258, &[]),
                    (6, 260, &[0x6a, 1, 0]),
                    (15, 259, &[]),
                ],
                259,
                AudioCodec::AdtsAac,
            ),
            // With no AAC, choose the first supported entry in PMT order.
            (
                &[(0x1b, 257, &[]), (3, 258, &[]), (6, 260, &[0x6a, 1, 0])],
                258,
                AudioCodec::MpegAudio,
            ),
            (
                &[(0x1b, 257, &[]), (6, 260, &[0x6a, 1, 0]), (3, 258, &[])],
                260,
                AudioCodec::Ac3,
            ),
        ];
        for (entries, selected_pid, codec) in cases {
            let LayoutDecision::Supported(layout) = normalization_policy_layout(entries) else {
                panic!("expected supported layout for {entries:?}");
            };
            assert_eq!(
                layout.audio,
                Some(SelectedAudio {
                    pid: *selected_pid,
                    codec: *codec
                })
            );
        }
    }

    #[test]
    fn normalization_policy_maps_both_mpeg_stream_types_without_layer_claims() {
        for ty in [0x03, 0x04] {
            let LayoutDecision::Supported(layout) =
                normalization_policy_layout(&[(0x1b, 257, &[]), (ty, 258, &[])])
            else {
                panic!("expected MPEG audio stream type {ty:#x} to be supported");
            };
            assert_eq!(
                layout.audio,
                Some(SelectedAudio {
                    pid: 258,
                    codec: AudioCodec::MpegAudio
                })
            );
        }
    }

    #[test]
    fn normalization_policy_accepts_video_only_with_known_video() {
        let LayoutDecision::Supported(layout) = normalization_policy_layout(&[(0x1b, 257, &[])])
        else {
            panic!("known H.264 video without audio should be supported");
        };
        assert_eq!(layout.video_pid, 257);
        assert_eq!(layout.video_type, 0x1b);
        assert_eq!(layout.audio, None);
    }

    #[test]
    fn normalization_policy_accepts_each_supported_video_stream_type() {
        for video_type in [0x02, 0x1b, 0x24] {
            let LayoutDecision::Supported(layout) =
                normalization_policy_layout(&[(video_type, 257, &[]), (15, 258, &[])])
            else {
                panic!("known video stream type {video_type:#x} should be supported");
            };
            assert_eq!(layout.video_type, video_type);
        }
    }

    #[test]
    fn normalization_policy_rejects_unusable_or_ambiguous_layouts() {
        let cases: &[RejectionCase<'_>] = &[
            ("LATM", &[(0x1b, 257, &[]), (0x11, 258, &[])]),
            ("EAC3", &[(0x1b, 257, &[]), (6, 258, &[0x7a, 1, 0])]),
            (
                "DTS",
                &[(0x1b, 257, &[]), (6, 258, &[0x7b, 5, 0, 0, 0, 0, 0])],
            ),
            ("unknown private", &[(0x1b, 257, &[]), (6, 258, &[])]),
            ("audio only", &[(15, 258, &[])]),
            ("multiple videos", &[(0x1b, 257, &[]), (0x02, 261, &[])]),
            (
                "unidentified private alongside supported audio",
                &[(0x1b, 257, &[]), (15, 258, &[]), (6, 260, &[])],
            ),
        ];
        for (label, entries) in cases {
            assert_eq!(
                normalization_policy_layout(entries),
                LayoutDecision::Unsupported,
                "{label}"
            );
        }

        let mut multi_program = LiveAudioSelection::default();
        multi_program.observe(table(
            0,
            vec![0, 0xb0, 0, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0, 0, 2, 0xe2, 0],
        ));
        assert_eq!(multi_program.layout(), LayoutDecision::Unsupported);

        let mut invalid_pcr = layout_state(&[(0x1b, 257, &[]), (15, 258, &[])]);
        let mut bad = pmt(0, &[(0x1b, 257, &[]), (15, 258, &[])]);
        bad[13] = 0xe1; // PMT PCR PID points at an undeclared elementary stream.
        bad[14] = 99;
        bad = repair(bad);
        invalid_pcr.observe(bad);
        assert_eq!(invalid_pcr.layout(), LayoutDecision::Unsupported);

        // PID 259 is a known but excluded subtitle/data stream beside selected AAC.
        let mut scrambled = layout_state(&[(0x1b, 257, &[]), (15, 258, &[]), (0x59, 259, &[])]);
        let mut packet = elementary(259);
        packet[3] |= 0x80;
        scrambled.observe(packet);
        assert_eq!(scrambled.layout(), LayoutDecision::Unsupported);
    }

    #[test]
    fn normalization_policy_rejects_unsupported_audio_stream_type() {
        // The fixture retains declared video PCR; 0x1c is MPEG-4 audio.
        assert_eq!(
            normalization_policy_layout(&[(0x1b, 257, &[]), (0x1c, 258, &[])]),
            LayoutDecision::Unsupported,
            "unsupported MPEG-4 audio must not be mistaken for video-only input"
        );
    }

    #[test]
    fn normalization_policy_rejects_additional_unsupported_video_stream_type() {
        // The fixture retains declared video PCR; 0x20 is an additional MVC video.
        assert_eq!(
            normalization_policy_layout(&[(0x1b, 257, &[]), (0x20, 258, &[])]),
            LayoutDecision::Unsupported,
            "additional MVC video must not be mistaken for data"
        );
    }

    #[test]
    fn normalization_policy_allows_known_private_sections_as_data() {
        // Private sections (0x05) are a known non-audio/data stream type.
        assert!(matches!(
            normalization_policy_layout(&[(0x1b, 257, &[]), (0x05, 258, &[])]),
            LayoutDecision::Supported(_)
        ));
        assert!(matches!(
            normalization_policy_layout(&[(0x1b, 257, &[]), (0x05, 258, &[]), (15, 259, &[])]),
            LayoutDecision::Supported(_)
        ));
        assert_eq!(
            normalization_policy_layout(&[(0x1b, 257, &[]), (0x05, 258, &[]), (0x1c, 259, &[]),]),
            LayoutDecision::Unsupported,
            "unclassified audio alongside known private sections must still reject"
        );
    }

    #[test]
    fn normalization_policy_rejects_conflicting_private_codec_signals_in_any_order() {
        let ac3 = [0x6a, 1, 0];
        let eac3 = [0x7a, 1, 0];
        let dts = [0x7b, 5, 0, 0, 0, 0, 0];
        let ac3_registration = [0x05, 4, b'A', b'C', b'-', b'3'];
        let eac3_registration = [0x05, 4, b'E', b'A', b'C', b'3'];

        let contradictory_descriptors = [
            [ac3.as_slice(), eac3.as_slice()].concat(),
            [eac3.as_slice(), ac3.as_slice()].concat(),
            [ac3.as_slice(), dts.as_slice()].concat(),
            [dts.as_slice(), ac3.as_slice()].concat(),
            [ac3.as_slice(), eac3_registration.as_slice()].concat(),
            [eac3_registration.as_slice(), ac3.as_slice()].concat(),
        ];
        let mut accepted_conflicts = Vec::new();
        for desc in &contradictory_descriptors {
            let without_aac = normalization_policy_layout(&[(0x1b, 257, &[]), (6, 258, desc)]);
            if without_aac != LayoutDecision::Unsupported {
                accepted_conflicts.push(format!("without AAC: {desc:?} => {without_aac:?}"));
            }
            let with_aac =
                normalization_policy_layout(&[(0x1b, 257, &[]), (15, 259, &[]), (6, 258, desc)]);
            if with_aac != LayoutDecision::Unsupported {
                accepted_conflicts.push(format!("with preferred AAC: {desc:?} => {with_aac:?}"));
            }
        }
        assert!(
            accepted_conflicts.is_empty(),
            "conflicting descriptor and registration signals must reject in every order: {accepted_conflicts:?}"
        );

        // Identical AC3 evidence from a registration and AC3 descriptor agrees.
        let agreeing = [ac3_registration.as_slice(), ac3.as_slice()].concat();
        let LayoutDecision::Supported(layout) =
            normalization_policy_layout(&[(0x1b, 257, &[]), (6, 258, &agreeing)])
        else {
            panic!("agreeing AC3 descriptor and registration should be accepted");
        };
        assert_eq!(
            layout.audio,
            Some(SelectedAudio {
                pid: 258,
                codec: AudioCodec::Ac3,
            })
        );
    }

    #[test]
    fn normalization_policy_rejects_fragmented_current_psi() {
        let mut st = LiveAudioSelection::default();
        st.observe(pat(1, 256));
        let mut fragment = pmt(0, &[(0x1b, 257, &[]), (15, 258, &[])]);
        fragment[7] = 200;
        // Legacy disabled-mode PSI passthrough remains intact; strict layout policy is
        // evaluated independently above and must reject the fragmented evidence.
        assert_eq!(st.observe(fragment).packet, Some(fragment));
        assert_eq!(st.layout(), LayoutDecision::Unsupported);
    }

    #[test]
    fn normalization_policy_invalid_noncurrent_and_wrong_program_preserve_layout() {
        let mut st = layout_state(&[(0x1b, 257, &[]), (15, 258, &[])]);
        let before = st.layout();
        let mutations: [fn(&mut [u8; N]); 3] = [
            |p: &mut [u8; N]| p[25] ^= 1,
            |p: &mut [u8; N]| {
                p[10] &= !1;
                *p = repair(*p);
            },
            |p: &mut [u8; N]| {
                p[9] = 2;
                *p = repair(*p);
            },
        ];
        for mutate in mutations {
            let mut bad = pmt(0, &[(0x1b, 257, &[]), (15, 258, &[])]);
            mutate(&mut bad);
            st.observe(bad);
            assert_eq!(st.layout(), before);
        }

        let mut malformed_descriptor = pmt(0, &[(0x1b, 257, &[]), (6, 258, &[0x6a, 1, 0])]);
        malformed_descriptor[31] = 4; // AC3 descriptor claims more bytes than its loop contains.
        malformed_descriptor = repair(malformed_descriptor);
        st.observe(malformed_descriptor);
        assert_eq!(st.layout(), before);
    }

    #[test]
    fn normalization_policy_current_unsupported_evidence_is_unsupported() {
        let mut st = layout_state(&[(0x1b, 257, &[]), (15, 258, &[])]);
        st.observe(pmt(1, &[(0x1b, 257, &[]), (0x11, 258, &[])]));
        assert_eq!(st.layout(), LayoutDecision::Unsupported);
    }

    #[test]
    fn normalization_policy_identity_ignores_version_but_tracks_semantic_changes() {
        let baseline =
            layout_state(&[(0x1b, 257, &[]), (15, 258, &[0x0a, 4, b'e', b'n', b'g', 0])]);
        let mut version_only = LiveAudioSelection::default();
        let mut version_pat = pat(1, 256);
        version_pat[10] = 0xc3; // PAT version 1, current, same program map.
        version_pat = repair(version_pat);
        version_pat[3] = 0x11; // Continuity counter is not part of layout identity.
        version_only.observe(version_pat);
        version_only.observe(pmt(
            17,
            &[(0x1b, 257, &[]), (15, 258, &[0x0a, 4, b'e', b'n', b'g', 0])],
        ));
        let mut version_pmt = pmt(
            17,
            &[(0x1b, 257, &[]), (15, 258, &[0x0a, 4, b'e', b'n', b'g', 0])],
        );
        version_pmt[3] = 0x13;
        version_only.observe(version_pmt);
        assert_eq!(version_only.layout(), baseline.layout());
        for changed in [
            layout_state(&[(0x1b, 257, &[]), (15, 258, &[0x0a, 4, b's', b'p', b'a', 0])]),
            layout_state(&[(0x1b, 257, &[]), (15, 259, &[])]),
            layout_state(&[(0x02, 257, &[]), (15, 258, &[])]),
        ] {
            assert_ne!(changed.layout(), baseline.layout());
        }
        let mut pcr_changed = LiveAudioSelection::default();
        pcr_changed.observe(pat(1, 256));
        let mut different_pcr = pmt(0, &[(0x1b, 257, &[]), (15, 258, &[])]);
        different_pcr[14] = 2;
        different_pcr = repair(different_pcr);
        pcr_changed.observe(different_pcr);
        assert_ne!(
            pcr_changed.layout(),
            layout_state(&[(0x1b, 257, &[]), (15, 258, &[])]).layout()
        );
        let ac3_descriptor = [0x6a, 1, 0];
        let ac3 = layout_state(&[(0x1b, 257, &[]), (6, 258, &ac3_descriptor)]);
        let LayoutDecision::Supported(ac3_layout) = ac3.layout() else {
            panic!("validated AC3 registration with declared video PCR should be supported");
        };
        assert_eq!(
            ac3_layout.audio,
            Some(SelectedAudio {
                pid: 258,
                codec: AudioCodec::Ac3
            })
        );
        assert_ne!(ac3.layout(), baseline.layout());
        let LayoutDecision::Supported(layout) = baseline.layout() else {
            panic!("expected baseline");
        };
        assert_eq!(layout.transport_stream_id, 1);
        assert_eq!(layout.program_id, 1);
        assert_eq!(layout.pmt_pid, 256);
        assert_eq!(layout.pcr_pid, 257); // fixture's PMT PCR field
        assert!(!layout.semantic_identity.is_empty());
    }

    #[test]
    fn normalization_policy_disabled_observe_keeps_legacy_bytes_and_passthrough() {
        let mut st = LiveAudioSelection::default();
        let pat = pat(1, 256);
        assert_eq!(st.observe(pat).packet, Some(pat));
        let original = source();
        let normalized = st.observe(original).packet.unwrap();
        assert_eq!(es_pids(&normalized), vec![257, 259]);
        let expected = st.pmt.unwrap();
        assert_eq!(normalized, expected);

        let legacy = pmt(3, &[(0x1b, 257, &[]), (0x11, 258, &[]), (6, 260, &[])]);
        let passthrough = st.observe(legacy).packet.unwrap();
        assert_eq!(passthrough, legacy);
        for pid in [258, 260] {
            let packet = elementary(pid);
            assert_eq!(st.observe(packet).packet, Some(packet));
        }
    }

    #[test]
    fn normalization_policy_disabled_conflicts_and_unknown_types_keep_legacy_filtering() {
        let ac3 = [0x6a, 1, 0];
        let eac3 = [0x7a, 1, 0];
        for descriptors in [
            [ac3.as_slice(), eac3.as_slice()].concat(),
            [eac3.as_slice(), ac3.as_slice()].concat(),
        ] {
            // Without AAC, legacy selection leaves the source PMT byte-for-byte intact.
            let mut st = LiveAudioSelection::default();
            let program = pat(1, 256);
            assert_eq!(st.observe(program).packet, Some(program));
            let source_pmt = pmt(3, &[(0x1b, 257, &[]), (0x06, 258, &descriptors)]);
            assert_eq!(st.observe(source_pmt).packet, Some(source_pmt));

            // With AAC present, the legacy native filter removes the recognized competitor.
            let mut st = LiveAudioSelection::default();
            st.observe(pat(1, 256));
            let source_pmt = pmt(
                3,
                &[
                    (0x1b, 257, &[]),
                    (0x06, 258, &descriptors),
                    (0x0f, 259, &[]),
                ],
            );
            let rewritten = st.observe(source_pmt).packet.unwrap();
            assert_eq!(es_pids(&rewritten), vec![257, 259]);
            assert!(st.observe(elementary(258)).packet.is_none());
            assert_eq!(st.observe(elementary(259)).packet, Some(elementary(259)));
        }

        // Stream types that strict policy cannot classify remain legacy passthrough bytes.
        let mut st = LiveAudioSelection::default();
        st.observe(pat(1, 256));
        let unknown_types = pmt(3, &[(0x1b, 257, &[]), (0x1c, 258, &[]), (0x20, 260, &[])]);
        assert_eq!(st.observe(unknown_types).packet, Some(unknown_types));
        for pid in [258, 260] {
            let packet = elementary(pid);
            assert_eq!(st.observe(packet).packet, Some(packet));
        }
    }

    #[test]
    fn normalization_policy_reset_returns_to_pending() {
        let mut st = layout_state(&[(0x1b, 257, &[]), (15, 258, &[])]);
        assert!(matches!(st.layout(), LayoutDecision::Supported(_)));
        st.reset();
        assert_eq!(st.layout(), LayoutDecision::Pending);
    }
}
