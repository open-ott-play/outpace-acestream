//! Bounded retained HLS media for explicitly requested, finite historical replay.
//!
//! This is local media history, not peer-network history or a wall-clock archive.
//! Views contain coordinates only: they cannot pin pruned media or exceed retention.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_SEGMENTS: usize = 4096;

#[cfg(test)]
mod retention_model;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HlsHistoryConfig {
    pub max_seconds: u64,
    pub max_bytes: usize,
    pub global_max_bytes: usize,
}

impl Default for HlsHistoryConfig {
    fn default() -> Self {
        Self {
            max_seconds: 0,
            max_bytes: 128 * 1024 * 1024,
            global_max_bytes: 256 * 1024 * 1024,
        }
    }
}

impl HlsHistoryConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_seconds > 3600 {
            return Err("OUTPACE_HISTORY_SECONDS must be <= 3600".into());
        }
        let safe = crate::config::safe_in_memory_pool_limit(isize::MAX as u64);
        if self.max_bytes < 188 || self.max_bytes as u64 > safe {
            return Err("OUTPACE_HISTORY_BYTES exceeds the supported memory range".into());
        }
        if self.global_max_bytes < self.max_bytes || self.global_max_bytes as u64 > safe {
            return Err("OUTPACE_HISTORY_TOTAL_BYTES must cover HISTORY_BYTES within the supported memory range".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HlsHistorySnapshot {
    pub first_seq: u64,
    pub last_seq: u64,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct HlsHistoryInterval {
    pub first_seq: u64,
    pub last_seq: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub epoch: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct HlsHistoryStatus {
    pub enabled: bool,
    pub kind: &'static str,
    pub time_basis: &'static str,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    /// Earliest independent entry contiguous with the current retained head.
    /// None when only non-decodable continuations or a stale retained tail exist.
    pub seekable_start_ms: Option<u64>,
    pub retained_bytes: usize,
    pub max_bytes: usize,
    pub max_seconds: u64,
    pub intervals: Vec<HlsHistoryInterval>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HlsHistoryError {
    Disabled,
    NotReady,
    Unavailable,
    Gone,
    InvalidTarget,
}

impl std::fmt::Display for HlsHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Disabled => "history is disabled",
            Self::NotReady => "history is not ready",
            Self::Unavailable => "requested history is unavailable",
            Self::Gone => "history has expired",
            Self::InvalidTarget => "invalid history target",
        })
    }
}
impl std::error::Error for HlsHistoryError {}

/// Shared across all packagers owned by one manager. Counts retained backing allocations,
/// conservatively counting a shared live/history allocation once for each history entry.
#[derive(Clone)]
pub(crate) struct HistoryBudget {
    used: Arc<Mutex<usize>>,
    max: usize,
}
impl HistoryBudget {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            used: Arc::new(Mutex::new(0)),
            max,
        }
    }
    fn reserve(&self, bytes: usize) -> Option<Charge> {
        let mut used = self.used.lock().unwrap();
        if bytes > self.max || *used > self.max - bytes {
            return None;
        }
        *used += bytes;
        Some(Charge {
            budget: self.clone(),
            bytes,
        })
    }
}
struct Charge {
    budget: HistoryBudget,
    bytes: usize,
}
impl Drop for Charge {
    fn drop(&mut self) {
        *self.budget.used.lock().unwrap() -= self.bytes;
    }
}
struct Segment {
    seq: u64,
    bytes: Bytes,
    _charge: Charge,
    duration: f32,
    start_ms: u64,
    end_ms: u64,
    epoch: u64,
    discontinuity: bool,
    independent: bool,
    created: Instant,
}

pub(crate) struct HistoryStore {
    config: HlsHistoryConfig,
    budget: HistoryBudget,
    segments: VecDeque<Segment>,
    used: usize,
    media_ms: u64,
    epoch: u64,
    previous_seq: Option<u64>,
}

impl HistoryStore {
    pub(crate) fn new(config: HlsHistoryConfig, budget: HistoryBudget) -> Self {
        Self {
            config,
            budget,
            segments: VecDeque::new(),
            used: 0,
            media_ms: 0,
            epoch: 0,
            previous_seq: None,
        }
    }
    fn pop_front(&mut self) {
        if let Some(segment) = self.segments.pop_front() {
            self.used -= segment._charge.bytes;
        }
    }
    fn clear(&mut self) {
        while !self.segments.is_empty() {
            self.pop_front();
        }
    }
    pub(crate) fn prune(&mut self, now: Instant) {
        let age = Duration::from_secs(self.config.max_seconds);
        let media_span = self.config.max_seconds.saturating_mul(1000);
        while self.segments.front().is_some_and(|s| {
            now.saturating_duration_since(s.created) >= age
                || self.media_ms.saturating_sub(s.start_ms) > media_span
        }) {
            self.pop_front();
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push(
        &mut self,
        seq: u64,
        bytes: Bytes,
        allocation: usize,
        duration: f32,
        discontinuity: bool,
        independent: bool,
        now: Instant,
    ) {
        if self.config.max_seconds == 0 {
            return;
        }
        // Unsafe/ambiguous timing must not become a seek coordinate.
        if !duration.is_finite() || duration <= 0.0 || duration > 3600.0 {
            self.clear();
            self.previous_seq = Some(seq);
            self.epoch = self.epoch.saturating_add(1);
            return;
        }
        let duration_ms = (f64::from(duration) * 1000.0).round().max(1.0) as u64;
        let Some(end_ms) = self.media_ms.checked_add(duration_ms) else {
            self.clear();
            return;
        };
        let start_ms = self.media_ms;
        self.media_ms = end_ms;
        if discontinuity
            || self
                .previous_seq
                .is_some_and(|p| p.checked_add(1) != Some(seq))
        {
            self.epoch = self.epoch.saturating_add(1);
        }
        self.previous_seq = Some(seq);
        self.prune(now);
        let allocation = allocation.max(bytes.len());
        if allocation > self.config.max_bytes {
            return;
        }
        while self.used > self.config.max_bytes - allocation || self.segments.len() >= MAX_SEGMENTS
        {
            self.pop_front();
        }
        // Reclaim this source's old history before refusing growth under the global cap.
        let charge = loop {
            if let Some(charge) = self.budget.reserve(allocation) {
                break Some(charge);
            }
            if self.segments.is_empty() {
                break None;
            }
            self.pop_front();
        };
        let Some(charge) = charge else {
            return;
        };
        self.used += allocation;
        self.segments.push_back(Segment {
            seq,
            bytes,
            _charge: charge,
            duration,
            start_ms,
            end_ms,
            epoch: self.epoch,
            discontinuity,
            independent,
            created: now,
        });
        self.prune(now);
    }
    pub(crate) fn status(&mut self, now: Instant) -> HlsHistoryStatus {
        self.prune(now);
        let mut seekable_start_ms = None;
        let mut expected = self.previous_seq;
        for segment in self.segments.iter().rev() {
            if expected != Some(segment.seq) {
                break;
            }
            if segment.independent {
                seekable_start_ms = Some(segment.start_ms);
            }
            expected = segment.seq.checked_sub(1);
        }
        let mut intervals: Vec<HlsHistoryInterval> = Vec::new();
        for s in &self.segments {
            if let Some(last) = intervals
                .last_mut()
                .filter(|p| p.epoch == s.epoch && p.last_seq.checked_add(1) == Some(s.seq))
            {
                last.last_seq = s.seq;
                last.end_ms = s.end_ms;
            } else {
                intervals.push(HlsHistoryInterval {
                    first_seq: s.seq,
                    last_seq: s.seq,
                    start_ms: s.start_ms,
                    end_ms: s.end_ms,
                    epoch: s.epoch,
                });
            }
        }
        HlsHistoryStatus {
            enabled: self.config.max_seconds != 0,
            kind: "local_hls_replay",
            time_basis: "retained_media_duration",
            first_seq: self.segments.front().map(|s| s.seq),
            last_seq: self.segments.back().map(|s| s.seq),
            start_ms: self.segments.front().map(|s| s.start_ms),
            end_ms: self.segments.back().map(|s| s.end_ms),
            seekable_start_ms,
            retained_bytes: self.used,
            max_bytes: self.config.max_bytes,
            max_seconds: self.config.max_seconds,
            intervals,
        }
    }
    pub(crate) fn prepare(
        &mut self,
        back: Duration,
        now: Instant,
    ) -> Result<HlsHistorySnapshot, HlsHistoryError> {
        if self.config.max_seconds == 0 {
            return Err(HlsHistoryError::Disabled);
        }
        self.prune(now);
        let back_ms: u64 = back
            .as_millis()
            .try_into()
            .map_err(|_| HlsHistoryError::InvalidTarget)?;
        if back_ms == 0 {
            return Err(HlsHistoryError::InvalidTarget);
        }
        let last = self.current_head()?;
        let target = last
            .end_ms
            .checked_sub(back_ms)
            .ok_or(HlsHistoryError::Unavailable)?;
        self.snapshot_at(target, last)
    }

    /// Capture from an absolute position on this packager's media-duration timeline.
    /// The target is not a wall-clock timestamp and does not move with the live head.
    pub(crate) fn prepare_at(
        &mut self,
        position_ms: u64,
        now: Instant,
    ) -> Result<HlsHistorySnapshot, HlsHistoryError> {
        if self.config.max_seconds == 0 {
            return Err(HlsHistoryError::Disabled);
        }
        self.prune(now);
        let last = self.current_head()?;
        // The completed head is an exclusive boundary: no media exists at/after it.
        if position_ms >= last.end_ms {
            return Err(HlsHistoryError::Unavailable);
        }
        self.snapshot_at(position_ms, last)
    }

    fn current_head(&self) -> Result<&Segment, HlsHistoryError> {
        let last = self.segments.back().ok_or(HlsHistoryError::NotReady)?;
        // If growth was refused, the old retained tail is not the live edge.
        if Some(last.seq) != self.previous_seq {
            return Err(HlsHistoryError::Unavailable);
        }
        Ok(last)
    }

    fn snapshot_at(
        &self,
        position_ms: u64,
        last: &Segment,
    ) -> Result<HlsHistorySnapshot, HlsHistoryError> {
        let first = self
            .segments
            .iter()
            .rev()
            .find(|s| s.start_ms <= position_ms && s.independent)
            .ok_or(HlsHistoryError::Unavailable)?;
        let snapshot = HlsHistorySnapshot {
            first_seq: first.seq,
            last_seq: last.seq,
            start_ms: first.start_ms,
            end_ms: last.end_ms,
        };
        self.range(&snapshot)
            .map_err(|_| HlsHistoryError::Unavailable)?;
        Ok(snapshot)
    }
    fn range(&self, view: &HlsHistorySnapshot) -> Result<Vec<&Segment>, HlsHistoryError> {
        if view.last_seq < view.first_seq {
            return Err(HlsHistoryError::Gone);
        }
        let count = view
            .last_seq
            .checked_sub(view.first_seq)
            .and_then(|n| n.checked_add(1))
            .ok_or(HlsHistoryError::Gone)?;
        if count > MAX_SEGMENTS as u64 {
            return Err(HlsHistoryError::Gone);
        }
        let selected: Vec<_> = self
            .segments
            .iter()
            .filter(|s| s.seq >= view.first_seq && s.seq <= view.last_seq)
            .collect();
        if selected.len() != count as usize
            || selected
                .first()
                .is_none_or(|s| !s.independent || s.start_ms != view.start_ms)
            || selected.last().is_none_or(|s| s.end_ms != view.end_ms)
            || selected
                .iter()
                .enumerate()
                .any(|(i, s)| s.seq != view.first_seq + i as u64)
        {
            return Err(HlsHistoryError::Gone);
        }
        Ok(selected)
    }
    pub(crate) fn playlist(
        &mut self,
        view: &HlsHistorySnapshot,
        prefix: &str,
        now: Instant,
    ) -> Result<String, HlsHistoryError> {
        self.prune(now);
        let segments = self.range(view)?;
        let target = segments
            .iter()
            .map(|s| s.duration.ceil() as u64)
            .max()
            .unwrap_or(1)
            .max(1);
        let mut out = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:{}\n#EXT-X-START:TIME-OFFSET=0.000,PRECISE=YES\n", view.first_seq);
        for s in segments {
            if s.discontinuity {
                out.push_str("#EXT-X-DISCONTINUITY\n");
            }
            out.push_str(&format!(
                "#EXTINF:{:.3},\n{prefix}/{}.ts\n",
                s.duration, s.seq
            ));
        }
        out.push_str("#EXT-X-ENDLIST\n");
        Ok(out)
    }
    pub(crate) fn segment(
        &mut self,
        view: &HlsHistorySnapshot,
        seq: u64,
        now: Instant,
    ) -> Result<Bytes, HlsHistoryError> {
        self.prune(now);
        if seq < view.first_seq || seq > view.last_seq {
            return Err(HlsHistoryError::Gone);
        }
        // A player may already have consumed an aged-out prefix of this immutable VOD.
        // Only the requested fragment must still exist; requiring the whole old manifest
        // here would terminate long replays while their unread fragments remain available.
        self.segments
            .iter()
            .find(|s| s.seq == seq && s.start_ms >= view.start_ms && s.end_ms <= view.end_ms)
            .map(|s| s.bytes.clone())
            .ok_or(HlsHistoryError::Gone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> HlsHistoryConfig {
        HlsHistoryConfig {
            max_seconds: 60,
            max_bytes: 1024,
            global_max_bytes: 2048,
        }
    }
    fn push(store: &mut HistoryStore, seq: u64, independent: bool, now: Instant) {
        store.push(
            seq,
            Bytes::from(vec![seq as u8; 100]),
            100,
            5.0,
            false,
            independent,
            now,
        );
    }
    #[test]
    fn finite_replay_starts_at_earlier_access_point_and_is_immutable() {
        let now = Instant::now();
        let mut store = HistoryStore::new(config(), HistoryBudget::new(2048));
        push(&mut store, 0, true, now);
        push(&mut store, 1, false, now);
        push(&mut store, 2, true, now);
        let view = store.prepare(Duration::from_secs(8), now).unwrap();
        assert_eq!(view.first_seq, 0);
        assert_eq!(view.last_seq, 2);
        let before = store.playlist(&view, "/synthetic", now).unwrap();
        push(&mut store, 3, true, now);
        assert_eq!(store.playlist(&view, "/synthetic", now).unwrap(), before);
        assert!(before.ends_with("#EXT-X-ENDLIST\n"));
        assert!(before.contains("#EXT-X-START:TIME-OFFSET=0.000"));
        assert_eq!(store.segment(&view, 3, now), Err(HlsHistoryError::Gone));
    }

    #[test]
    fn absolute_position_stays_stable_as_the_completed_head_advances() {
        let now = Instant::now();
        let mut store = HistoryStore::new(config(), HistoryBudget::new(2048));
        push(&mut store, 0, true, now);
        push(&mut store, 1, false, now);
        push(&mut store, 2, true, now);
        let view = store.prepare_at(7500, now).unwrap();
        assert_eq!(
            (view.first_seq, view.last_seq, view.start_ms, view.end_ms),
            (0, 2, 0, 15000)
        );
        let playlist = store.playlist(&view, "/synthetic", now).unwrap();

        push(&mut store, 3, true, now);
        let later = store.prepare_at(7500, now).unwrap();
        assert_eq!(
            (
                later.first_seq,
                later.last_seq,
                later.start_ms,
                later.end_ms
            ),
            (0, 3, 0, 20000)
        );
        assert_eq!(
            store
                .prepare(Duration::from_millis(7500), now)
                .unwrap()
                .first_seq,
            2
        );
        assert_eq!(store.playlist(&view, "/synthetic", now).unwrap(), playlist);
        assert_eq!(store.prepare_at(0, now).unwrap().first_seq, 0);
        assert_eq!(store.prepare_at(19_999, now).unwrap().first_seq, 3);
        for position in [20_000, 20_001, u64::MAX] {
            assert_eq!(
                store.prepare_at(position, now),
                Err(HlsHistoryError::Unavailable)
            );
        }
    }

    #[test]
    fn absolute_position_rejects_expiry_holes_and_unretained_head() {
        let now = Instant::now();
        let mut store = HistoryStore::new(config(), HistoryBudget::new(2048));
        assert_eq!(store.prepare_at(0, now), Err(HlsHistoryError::NotReady));
        push(&mut store, 0, true, now);
        push(&mut store, 2, true, now);
        assert_eq!(
            store.prepare_at(1000, now),
            Err(HlsHistoryError::Unavailable)
        );
        let view = store.prepare_at(5000, now).unwrap();
        assert_eq!(view.first_seq, 2);
        store.push(3, Bytes::from_static(b"tiny"), 1025, 5.0, false, true, now);
        assert_eq!(
            store.prepare_at(5000, now),
            Err(HlsHistoryError::Unavailable)
        );
        assert!(store.playlist(&view, "/synthetic", now).is_ok());
        assert_eq!(
            store.prepare_at(5000, now + Duration::from_secs(61)),
            Err(HlsHistoryError::NotReady)
        );

        let mut disabled = HistoryStore::new(HlsHistoryConfig::default(), HistoryBudget::new(2048));
        assert_eq!(disabled.prepare_at(0, now), Err(HlsHistoryError::Disabled));
    }
    #[test]
    fn holes_unavailable_and_expired_views_are_never_truncated() {
        let now = Instant::now();
        let mut store = HistoryStore::new(config(), HistoryBudget::new(2048));
        push(&mut store, 0, true, now);
        push(&mut store, 2, true, now);
        assert_eq!(
            store.prepare(Duration::from_secs(8), now),
            Err(HlsHistoryError::Unavailable)
        );
        let view = store.prepare(Duration::from_secs(3), now).unwrap();
        assert_eq!(
            store.playlist(&view, "/x", now + Duration::from_secs(61)),
            Err(HlsHistoryError::Gone)
        );
    }

    #[test]
    fn consumed_prefix_can_expire_without_invalidating_unread_replay_segments() {
        let now = Instant::now();
        let mut store = HistoryStore::new(config(), HistoryBudget::new(2048));
        push(&mut store, 0, true, now);
        push(&mut store, 1, true, now + Duration::from_secs(5));
        push(&mut store, 2, true, now + Duration::from_secs(10));
        let view = store
            .prepare(Duration::from_secs(12), now + Duration::from_secs(10))
            .unwrap();
        let later = now + Duration::from_secs(61);
        assert_eq!(store.segment(&view, 0, later), Err(HlsHistoryError::Gone));
        assert_eq!(store.segment(&view, 1, later).unwrap()[0], 1);
        assert_eq!(store.segment(&view, 2, later).unwrap()[0], 2);
        assert_eq!(
            store.playlist(&view, "/x", later),
            Err(HlsHistoryError::Gone)
        );
        assert_eq!(store.segment(&view, 3, later), Err(HlsHistoryError::Gone));
    }
    #[test]
    fn global_budget_bounds_multiple_sources_and_is_released_on_drop() {
        let now = Instant::now();
        let budget = HistoryBudget::new(300);
        let mut a = HistoryStore::new(config(), budget.clone());
        let mut b = HistoryStore::new(config(), budget.clone());
        push(&mut a, 0, true, now);
        push(&mut a, 1, true, now);
        push(&mut b, 0, true, now);
        push(&mut b, 1, true, now);
        assert_eq!(*budget.used.lock().unwrap(), 300);
        assert_eq!(b.status(now).first_seq, Some(1));
        drop(a);
        assert_eq!(*budget.used.lock().unwrap(), 100);
        drop(b);
        assert_eq!(*budget.used.lock().unwrap(), 0);
    }
    #[test]
    fn backing_capacity_is_charged_and_oversized_tail_is_not_called_live() {
        let now = Instant::now();
        let budget = HistoryBudget::new(2048);
        let mut store = HistoryStore::new(config(), budget.clone());
        store.push(0, Bytes::from_static(b"tiny"), 900, 5.0, false, true, now);
        assert_eq!(store.status(now).retained_bytes, 900);
        store.push(1, Bytes::from_static(b"tiny"), 1025, 5.0, false, true, now);
        assert_eq!(
            store.prepare(Duration::from_secs(1), now),
            Err(HlsHistoryError::Unavailable)
        );
    }
    #[test]
    fn duration_bound_discontinuity_and_disabled_configuration() {
        let now = Instant::now();
        let mut cfg = config();
        cfg.max_seconds = 10;
        let mut store = HistoryStore::new(cfg, HistoryBudget::new(2048));
        push(&mut store, 0, true, now);
        push(&mut store, 1, true, now);
        store.push(2, Bytes::from(vec![2; 100]), 100, 5.0, true, true, now);
        let state = store.status(now);
        assert_eq!(state.first_seq, Some(1));
        assert_eq!(state.intervals.len(), 2);
        let view = store.prepare(Duration::from_secs(9), now).unwrap();
        assert!(store
            .playlist(&view, "/x", now)
            .unwrap()
            .contains("#EXT-X-DISCONTINUITY"));
        let mut disabled = HistoryStore::new(HlsHistoryConfig::default(), HistoryBudget::new(2048));
        push(&mut disabled, 0, true, now);
        assert_eq!(disabled.status(now).retained_bytes, 0);
        assert_eq!(
            disabled.prepare(Duration::from_secs(1), now),
            Err(HlsHistoryError::Disabled)
        );
        cfg.max_seconds = 3601;
        assert!(cfg.validate().is_err());
    }
}
