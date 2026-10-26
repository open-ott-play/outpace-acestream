use crate::config::StartupBufferConfig;
use crate::provider::{SourceStats, StatsHandle, StatsReader, TsSource};
use ace_media::mpegts::{ts_timing, ClockObservation, SelectedPcrClock, TS_PACKET_LEN};
use ace_swarm::types::StreamMetadata;
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseReason {
    TargetDuration,
    ByteLimit,
    Deadline,
    EndOfStream,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Collecting,
    Draining(ReleaseReason),
    Released,
}

fn release_event(reason: ReleaseReason, duration: Duration, queued_bytes: usize) -> String {
    format!(
        "[prebuffer] startup buffer released: reason={reason:?} duration_ms={} queued_bytes={queued_bytes}",
        duration.as_millis()
    )
}

struct StartupReservoir {
    config: StartupBufferConfig,
    /// Raw transport descriptor rate in bytes/s; measured source stats remain bits/s.
    bitrate: Option<u64>,
    queue: VecDeque<BufferedChunk>,
    queued_bytes: usize,
    clock: SelectedPcrClock,
    partial: Vec<u8>,
    /// Absolute startup origin. Media resets discard queued data, not elapsed waiting time.
    first_byte_at: Option<Instant>,
    release_clock_duration: Option<Duration>,
    release_bytes: usize,
    phase: Phase,
}

struct BufferedChunk {
    bytes: Bytes,
    startup_complete: bool,
}

impl StartupReservoir {
    fn new(config: StartupBufferConfig, bitrate: Option<u64>) -> Self {
        Self {
            config,
            bitrate: bitrate.filter(|rate| *rate > 0),
            queue: VecDeque::new(),
            queued_bytes: 0,
            clock: SelectedPcrClock::new(),
            partial: Vec::with_capacity(TS_PACKET_LEN),
            first_byte_at: None,
            release_clock_duration: None,
            release_bytes: 0,
            phase: if config.target_ms == 0 {
                Phase::Draining(ReleaseReason::Disabled)
            } else {
                Phase::Collecting
            },
        }
    }

    fn push(&mut self, mut chunk: Bytes, startup_complete: bool) -> Option<ReleaseReason> {
        if self.phase != Phase::Collecting {
            return match self.phase {
                Phase::Draining(reason) => Some(reason),
                _ => None,
            };
        }
        // Empty reads cannot grow queue metadata or start the media deadline. A source
        // gap has already been handled by collect_next, and an existing deadline still runs.
        if chunk.is_empty() {
            let reason = self.ready_reason(Instant::now());
            if let Some(reason) = reason {
                self.release(reason);
            }
            return reason;
        }
        self.first_byte_at.get_or_insert_with(Instant::now);

        if let Some(retain_from) = self.observe_bytes(&chunk) {
            self.discard_prefix(retain_from, &mut chunk);
        }
        let crosses_cap = self.queued_bytes.saturating_add(chunk.len()) > self.config.max_bytes;
        self.queued_bytes += chunk.len();
        self.queue.push_back(BufferedChunk {
            bytes: chunk,
            startup_complete,
        });

        let reason = if crosses_cap {
            Some(ReleaseReason::ByteLimit)
        } else {
            self.ready_reason(Instant::now())
        };
        if let Some(reason) = reason {
            self.release(reason);
        }
        reason
    }

    /// Observe complete packets and return the combined queued/current byte offset of the last
    /// reset packet. The caller uses that boundary to retain the reset packet and its suffix.
    fn observe_bytes(&mut self, bytes: &[u8]) -> Option<usize> {
        let prior_bytes = self.queued_bytes;
        let mut retain_from = None;
        let mut consumed = 0;
        if !self.partial.is_empty() {
            let take = (TS_PACKET_LEN - self.partial.len()).min(bytes.len());
            self.partial.extend_from_slice(&bytes[..take]);
            consumed = take;
            if self.partial.len() == TS_PACKET_LEN {
                // Only a packet straddling two chunks needs a scratch copy. Whole packets below
                // borrow the input directly instead of pushing/copying every media byte.
                let packet: [u8; TS_PACKET_LEN] = self.partial.as_slice().try_into().unwrap();
                self.partial.clear();
                if self.observe_packet(&packet) {
                    retain_from = Some(prior_bytes + consumed - TS_PACKET_LEN);
                }
            }
        }
        let (packets, remainder) = bytes[consumed..].as_chunks::<TS_PACKET_LEN>();
        for packet in packets {
            if self.observe_packet(packet) {
                retain_from = Some(prior_bytes + consumed);
            }
            consumed += TS_PACKET_LEN;
        }
        self.partial.extend_from_slice(remainder);
        retain_from
    }

    fn observe_packet(&mut self, packet: &[u8; TS_PACKET_LEN]) -> bool {
        let timing = ts_timing(packet);
        if self.clock.observe(timing) != ClockObservation::Reset {
            return false;
        }
        self.release_clock_duration = None;
        self.release_bytes = 0;
        if !timing.discontinuity {
            let _ = self.clock.observe(timing);
        }
        true
    }

    fn discard_prefix(&mut self, retain_from: usize, chunk: &mut Bytes) {
        let prior_bytes = self.queued_bytes;
        if retain_from >= prior_bytes {
            self.queue.clear();
            self.queued_bytes = 0;
            *chunk = chunk.slice(retain_from - prior_bytes..);
            return;
        }

        let mut discard = retain_from;
        while discard > 0 {
            let front_len = self.queue.front().map_or(0, |chunk| chunk.bytes.len());
            if discard < front_len {
                let mut retained = self
                    .queue
                    .pop_front()
                    .expect("front length came from queue");
                retained.bytes = retained.bytes.slice(discard..);
                self.queue.push_front(retained);
                self.queued_bytes -= discard;
                break;
            }
            self.queue.pop_front();
            self.queued_bytes -= front_len;
            discard -= front_len;
        }
    }

    fn discontinuity(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
        self.partial.clear();
        self.clock.reset();
    }

    fn duration(&self) -> Duration {
        let clock_duration = match (self.release_clock_duration, self.release_bytes) {
            (Some(duration), bytes) if bytes > 0 => {
                duration.mul_f64(self.queued_bytes as f64 / bytes as f64)
            }
            _ => self.clock.elapsed().unwrap_or_default(),
        };
        (clock_duration > Duration::ZERO)
            .then_some(clock_duration)
            .or_else(|| {
                self.bitrate
                    .map(|rate| Duration::from_secs_f64(self.queued_bytes as f64 / rate as f64))
            })
            .unwrap_or_default()
    }

    fn ready_reason(&self, now: Instant) -> Option<ReleaseReason> {
        match self.phase {
            Phase::Draining(reason) => Some(reason),
            Phase::Collecting if self.duration() >= self.config.target() => {
                Some(ReleaseReason::TargetDuration)
            }
            Phase::Collecting
                if self
                    .first_byte_at
                    .is_some_and(|start| now.duration_since(start) >= self.config.timeout()) =>
            {
                Some(ReleaseReason::Deadline)
            }
            _ => None,
        }
    }

    fn deadline(&self) -> Option<Instant> {
        (self.phase == Phase::Collecting)
            .then(|| {
                self.first_byte_at
                    .map(|start| start + self.config.timeout())
            })
            .flatten()
    }

    fn release(&mut self, reason: ReleaseReason) {
        if self.phase != Phase::Collecting {
            return;
        }
        if matches!(
            reason,
            ReleaseReason::ByteLimit | ReleaseReason::Deadline | ReleaseReason::EndOfStream
        ) {
            crate::alog!("[prebuffer] startup buffer released early: {:?}", reason);
        }
        crate::alog!(
            "{}",
            release_event(reason, self.duration(), self.queued_bytes)
        );
        self.release_clock_duration = self.clock.elapsed();
        self.release_bytes = self.queued_bytes;
        self.phase = Phase::Draining(reason);
    }

    fn pop_front(&mut self) -> Option<BufferedChunk> {
        let chunk = self.queue.pop_front()?;
        self.queued_bytes -= chunk.bytes.len();
        Some(chunk)
    }

    fn finish_drain(&mut self) {
        if matches!(self.phase, Phase::Draining(_)) && self.queue.is_empty() {
            self.phase = Phase::Released;
        }
    }

    #[cfg(test)]
    fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }
}

struct StartupObservation {
    cached: SourceStats,
    reservoir_ms: Option<u64>,
}
struct StartupStatsReader {
    inner: Option<StatsHandle>,
    observed: Mutex<StartupObservation>,
}
impl StatsReader for StartupStatsReader {
    fn snapshot(&self) -> SourceStats {
        let (cached, reservoir_ms) = {
            let observed = self.observed.lock().unwrap();
            (observed.cached.clone(), observed.reservoir_ms)
        };
        let mut stats = self.inner.as_ref().map_or(cached, |inner| inner.snapshot());
        if let Some(duration) = reservoir_ms {
            stats.buffer_ms = duration;
        }
        stats
    }
}

pub struct StartupBufferedSource {
    inner: Box<dyn TsSource>,
    stats: Arc<StartupStatsReader>,
    reservoir: StartupReservoir,
    pending_discontinuity: bool,
    released_chunk_discontinuity: bool,
    released_chunk_startup_complete: bool,
}

impl StartupBufferedSource {
    #[allow(clippy::new_ret_no_self)] // The public decorator interface intentionally returns a trait object.
    pub fn new(
        inner: Box<dyn TsSource>,
        config: StartupBufferConfig,
        bitrate: Option<u64>,
    ) -> Box<dyn TsSource> {
        let released_chunk_startup_complete = inner.startup_complete();
        let stats = Arc::new(StartupStatsReader {
            inner: inner.stats_handle(),
            observed: Mutex::new(StartupObservation {
                cached: inner.stats(),
                reservoir_ms: Some(0),
            }),
        });
        Box::new(Self {
            inner,
            stats,
            reservoir: StartupReservoir::new(config, bitrate),
            pending_discontinuity: false,
            released_chunk_discontinuity: false,
            released_chunk_startup_complete,
        })
    }

    fn publish_stats(&self) {
        let cached = self.inner.stats();
        let reservoir_ms = (self.reservoir.phase != Phase::Released)
            .then(|| self.reservoir.duration().as_millis().min(u64::MAX as u128) as u64);
        *self.stats.observed.lock().unwrap() = StartupObservation {
            cached,
            reservoir_ms,
        };
    }

    async fn collect_next(&mut self) {
        let next = if let Some(deadline) = self.reservoir.deadline() {
            tokio::select! { chunk = self.inner.next() => chunk, _ = tokio::time::sleep_until(deadline) => { self.reservoir.release(ReleaseReason::Deadline); self.publish_stats(); return; } }
        } else {
            self.inner.next().await
        };

        match next {
            Some(chunk) => {
                if self.inner.take_discontinuity() {
                    self.reservoir.discontinuity();
                    self.pending_discontinuity = true;
                }
                self.reservoir.push(chunk, self.inner.startup_complete());
            }
            None => self.reservoir.release(ReleaseReason::EndOfStream),
        }
        self.publish_stats();
    }
}

#[async_trait]
impl TsSource for StartupBufferedSource {
    async fn next(&mut self) -> Option<Bytes> {
        loop {
            match self.reservoir.phase {
                Phase::Collecting => self.collect_next().await,
                Phase::Draining(_) => {
                    if let Some(chunk) = self.reservoir.pop_front() {
                        self.released_chunk_discontinuity =
                            std::mem::take(&mut self.pending_discontinuity);
                        self.publish_stats();
                        self.released_chunk_startup_complete = chunk.startup_complete;
                        return Some(chunk.bytes);
                    }
                    self.reservoir.finish_drain();
                    self.publish_stats();
                }
                Phase::Released => {
                    let next = self.inner.next().await;
                    self.publish_stats();
                    if next.as_ref().is_some_and(Bytes::is_empty) {
                        self.pending_discontinuity |= self.inner.take_discontinuity();
                        continue;
                    }
                    return next;
                }
            }
        }
    }

    fn take_discontinuity(&mut self) -> bool {
        if self.reservoir.phase == Phase::Released {
            // An empty post-gap reservoir may reach its deadline before new media arrives.
            // Consume both flags even when the carried gap is already true.
            let inner = self.inner.take_discontinuity();
            std::mem::take(&mut self.pending_discontinuity) || inner
        } else {
            std::mem::take(&mut self.released_chunk_discontinuity)
        }
    }

    fn startup_complete(&self) -> bool {
        if self.reservoir.phase == Phase::Released {
            self.inner.startup_complete()
        } else {
            self.released_chunk_startup_complete
        }
    }

    fn stats(&self) -> SourceStats {
        self.stats.snapshot()
    }
    fn stats_handle(&self) -> Option<StatsHandle> {
        Some(self.stats.clone())
    }

    fn metadata(&self) -> StreamMetadata {
        self.inner.metadata()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StartupBufferConfig;
    use crate::provider::{SourceStats, TsSource};
    use ace_swarm::types::StreamMetadata;
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::collections::VecDeque;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    const PID: u16 = 0x100;

    fn config(target_ms: u64, max_bytes: usize, timeout_ms: u64) -> StartupBufferConfig {
        StartupBufferConfig {
            target_ms,
            max_bytes,
            timeout_ms,
        }
    }

    fn packet(pid: u16, pcr: Option<u64>, discontinuity: bool, marker: u8) -> Bytes {
        let mut p = [marker; 188];
        p[0] = 0x47;
        p[1] = (pid >> 8) as u8 & 0x1f;
        p[2] = pid as u8;
        p[3] = if pcr.is_some() || discontinuity {
            0x30
        } else {
            0x10
        };
        if let Some(pcr) = pcr {
            p[4] = 7;
            p[5] = 0x10 | if discontinuity { 0x80 } else { 0 };
            p[6] = (pcr >> 25) as u8;
            p[7] = (pcr >> 17) as u8;
            p[8] = (pcr >> 9) as u8;
            p[9] = (pcr >> 1) as u8;
            p[10] = ((pcr & 1) << 7) as u8 | 0x7e;
        } else if discontinuity {
            p[4] = 1;
            p[5] = 0x80;
        }
        Bytes::copy_from_slice(&p)
    }

    struct FixtureSource {
        chunks: VecDeque<(Bytes, bool)>,
        last_gap: bool,
        stats: SourceStats,
        live_stats: Option<Arc<std::sync::Mutex<SourceStats>>>,
        metadata: StreamMetadata,
        delay_after: Option<usize>,
        reads: usize,
        count_reads: bool,
        dropped: Option<Arc<AtomicBool>>,
    }

    impl FixtureSource {
        fn new(chunks: Vec<(Bytes, bool)>) -> Self {
            Self {
                chunks: chunks.into(),
                last_gap: false,
                stats: SourceStats::default(),
                live_stats: None,
                metadata: StreamMetadata::default(),
                delay_after: None,
                reads: 0,
                count_reads: false,
                dropped: None,
            }
        }
    }

    impl Drop for FixtureSource {
        fn drop(&mut self) {
            if let Some(flag) = &self.dropped {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }

    #[async_trait]
    impl TsSource for FixtureSource {
        async fn next(&mut self) -> Option<Bytes> {
            if self.delay_after == Some(self.reads) {
                std::future::pending().await
            };
            let (bytes, gap) = self.chunks.pop_front()?;
            self.reads += 1;
            self.last_gap = gap;
            Some(bytes)
        }
        fn take_discontinuity(&mut self) -> bool {
            std::mem::take(&mut self.last_gap)
        }
        fn stats(&self) -> SourceStats {
            let mut stats = self
                .live_stats
                .as_ref()
                .map(|stats| stats.lock().unwrap().clone())
                .unwrap_or_else(|| self.stats.clone());
            if self.count_reads {
                stats.downloaded += self.reads as u64;
            }
            stats
        }
        fn stats_handle(&self) -> Option<crate::provider::StatsHandle> {
            self.live_stats
                .as_ref()
                .map(|stats| Arc::new(MutableStats(stats.clone())) as crate::provider::StatsHandle)
        }
        fn metadata(&self) -> StreamMetadata {
            self.metadata.clone()
        }
    }

    struct MutableStats(Arc<std::sync::Mutex<SourceStats>>);
    impl crate::provider::StatsReader for MutableStats {
        fn snapshot(&self) -> SourceStats {
            self.0.lock().unwrap().clone()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn live_stats_are_visible_during_pending_startup_and_cancel_cleanly() {
        let stats = Arc::new(std::sync::Mutex::new(SourceStats {
            bitrate: 8192,
            peers: 3,
            ..SourceStats::default()
        }));
        let dropped = Arc::new(AtomicBool::new(false));
        let mut inner = FixtureSource::new(vec![
            (packet(PID, Some(0), false, 1), false),
            (packet(PID, Some(90000), false, 2), false),
        ]);
        inner.live_stats = Some(stats.clone());
        inner.delay_after = Some(2);
        inner.dropped = Some(dropped.clone());
        let buffered =
            StartupBufferedSource::new(Box::new(inner), config(10000, 4096, 30000), None);
        let session = crate::session::StreamSession::start(buffered, 16);
        let mut subscription = session.subscribe();
        tokio::task::yield_now().await;
        assert_eq!(session.stats().await.bitrate, 8192);
        assert_eq!(session.stats().await.buffer_ms, 1000);
        assert!(matches!(
            subscription.rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        stats.lock().unwrap().peers = 4;
        assert_eq!(session.stats().await.peers, 4);
        drop(session);
        tokio::task::yield_now().await;
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn released_wrapper_updates_sources_without_readers() {
        let mut inner = FixtureSource::new(vec![(Bytes::from_static(b"x"), false)]);
        inner.count_reads = true;
        let mut buffered =
            StartupBufferedSource::new(Box::new(inner), config(0, 4096, 30000), None);
        let reader = buffered.stats_handle().unwrap();
        assert!(buffered.next().await.is_some());
        assert_eq!(reader.snapshot().downloaded, 1);
    }

    #[tokio::test]
    async fn independent_reservoir_stats_decrease_while_draining_and_finish() {
        let mut inner = FixtureSource::new(vec![
            (packet(PID, Some(0), false, 1), false),
            (packet(PID, Some(90000), false, 2), false),
        ]);
        inner.stats.buffer_ms = 19;
        let mut buffered =
            StartupBufferedSource::new(Box::new(inner), config(1000, 4096, 30000), None);
        let reader = buffered
            .stats_handle()
            .expect("independent reservoir reader");
        assert!(buffered.next().await.is_some());
        assert_eq!(reader.snapshot().buffer_ms, 500);
        assert!(buffered.next().await.is_some());
        assert_eq!(reader.snapshot().buffer_ms, 0);
        assert!(buffered.next().await.is_none());
        assert_eq!(reader.snapshot().buffer_ms, 19);
    }

    #[tokio::test(start_paused = true)]
    async fn startup_stats_discontinuity_removes_the_old_epoch() {
        let mut inner = FixtureSource::new(vec![
            (packet(PID, Some(0), false, 1), false),
            (packet(PID, Some(90000), false, 2), false),
            (packet(PID, Some(900000), false, 3), true),
            (packet(PID, Some(990000), false, 4), false),
        ]);
        inner.delay_after = Some(4);
        let session = crate::session::StreamSession::start(
            StartupBufferedSource::new(Box::new(inner), config(20000, 4096, 30000), None),
            16,
        );
        let _receiver = session.raw_receiver();
        tokio::task::yield_now().await;
        assert_eq!(session.stats().await.buffer_ms, 1000);
    }

    fn boxed(chunks: Vec<(Bytes, bool)>) -> Box<dyn TsSource> {
        Box::new(FixtureSource::new(chunks))
    }

    #[tokio::test]
    async fn withholds_until_target_then_releases_exact_bytes_in_order() {
        let source = boxed(vec![
            (packet(PID, Some(0), false, b'a'), false),
            (packet(PID, Some(90_000), false, b'b'), false),
            (packet(PID, Some(180_000), false, b'c'), false),
        ]);
        let mut buffered = StartupBufferedSource::new(source, config(2_000, 4096, 10_000), None);
        assert_eq!(buffered.next().await.unwrap()[20], b'a');
        assert_eq!(buffered.next().await.unwrap()[20], b'b');
        assert_eq!(buffered.next().await.unwrap()[20], b'c');
    }

    #[tokio::test]
    async fn startup_discontinuity_discards_pre_gap_but_runtime_gap_never_regates() {
        let source = boxed(vec![
            (packet(PID, Some(0), false, b'a'), false),
            (packet(PID, Some(45_000), false, b'b'), false),
            (packet(PID, Some(0), false, b'c'), true),
            (packet(PID, Some(90_000), false, b'd'), false),
            (packet(PID, Some(180_000), false, b'e'), false),
            (packet(PID, Some(270_000), false, b'f'), true),
        ]);
        let mut buffered = StartupBufferedSource::new(source, config(1_000, 4096, 10_000), None);
        assert_eq!(buffered.next().await.unwrap()[20], b'c');
        assert!(buffered.take_discontinuity());
        assert_eq!(buffered.next().await.unwrap()[20], b'd');
        assert_eq!(buffered.next().await.unwrap()[20], b'e');
        assert_eq!(buffered.next().await.unwrap()[20], b'f');
        assert!(buffered.take_discontinuity());
    }

    #[tokio::test]
    async fn in_chunk_clock_reset_discards_every_byte_before_reset_packet() {
        let pre_gap_a = packet(PID, Some(0), false, b'a');
        let pre_gap_b = packet(PID, Some(45_000), false, b'b');
        let reset = packet(PID, Some(90_000), true, b'c');
        let post_gap_a = packet(PID, Some(0), false, b'd');
        let post_gap_b = packet(PID, Some(90_000), false, b'e');
        let chunk = [
            pre_gap_a.as_ref(),
            pre_gap_b.as_ref(),
            reset.as_ref(),
            post_gap_a.as_ref(),
            post_gap_b.as_ref(),
        ]
        .concat();
        let expected = [reset.as_ref(), post_gap_a.as_ref(), post_gap_b.as_ref()].concat();
        let mut buffered = StartupBufferedSource::new(
            boxed(vec![(Bytes::from(chunk), false)]),
            config(1_000, 4096, 10_000),
            None,
        );

        assert_eq!(buffered.next().await.unwrap().as_ref(), expected);
    }

    #[tokio::test]
    async fn reset_packet_split_across_chunks_retains_its_prefix_not_pre_gap_bytes() {
        let pre_gap = packet(PID, Some(0), false, b'a');
        let reset = packet(PID, Some(45_000), true, b'b');
        let post_gap_a = packet(PID, Some(0), false, b'c');
        let post_gap_b = packet(PID, Some(90_000), false, b'd');
        let first = [pre_gap.as_ref(), &reset[..73]].concat();
        let second = [&reset[73..], post_gap_a.as_ref(), post_gap_b.as_ref()].concat();
        let mut buffered = StartupBufferedSource::new(
            boxed(vec![
                (Bytes::from(first), false),
                (Bytes::from(second), false),
            ]),
            config(1_000, 4096, 10_000),
            None,
        );

        assert_eq!(buffered.next().await.unwrap().as_ref(), &reset[..73]);
        assert_eq!(
            buffered.next().await.unwrap().as_ref(),
            [&reset[73..], post_gap_a.as_ref(), post_gap_b.as_ref()].concat()
        );
    }

    #[tokio::test]
    async fn zero_target_is_disabled_and_reads_only_one_chunk() {
        let mut buffered = StartupBufferedSource::new(
            boxed(vec![
                (packet(PID, Some(0), false, b'a'), false),
                (packet(PID, Some(90_000), false, b'b'), false),
            ]),
            config(0, 188, 1),
            None,
        );
        assert_eq!(buffered.next().await.unwrap()[20], b'a');
    }

    #[tokio::test]
    async fn pcr_wrap_and_other_pid_still_reach_target() {
        let modulus = 1_u64 << 33;
        let source = boxed(vec![
            (packet(PID, Some(modulus - 45_000), false, b'a'), false),
            (packet(PID + 1, Some(5_000_000), false, b'x'), false),
            (packet(PID, Some(45_000), false, b'b'), false),
        ]);
        let mut buffered = StartupBufferedSource::new(source, config(1_000, 4096, 10_000), None);
        assert_eq!(buffered.next().await.unwrap()[20], b'a');
        assert_eq!(buffered.next().await.unwrap()[20], b'x');
    }

    #[tokio::test]
    async fn backward_pcr_discards_prior_startup_data() {
        let source = boxed(vec![
            (packet(PID, Some(180_000), false, b'a'), false),
            (packet(PID, Some(90_000), false, b'b'), false),
            (packet(PID, Some(180_000), false, b'c'), false),
            (packet(PID, Some(270_000), false, b'd'), false),
        ]);
        let mut buffered = StartupBufferedSource::new(source, config(1_000, 4096, 10_000), None);
        assert_eq!(buffered.next().await.unwrap()[20], b'b');
    }

    #[tokio::test]
    async fn late_pcr_and_split_packets_are_timed_without_losing_chunks() {
        let p0 = packet(PID, Some(0), false, b'b');
        let p1 = packet(PID, Some(90_000), false, b'c');
        let source = boxed(vec![
            (packet(PID, None, false, b'a'), false),
            (p0.slice(..91), false),
            (p0.slice(91..), false),
            (p1, false),
        ]);
        let mut buffered = StartupBufferedSource::new(source, config(1_000, 4096, 10_000), None);
        assert_eq!(buffered.next().await.unwrap()[20], b'a');
        assert_eq!(buffered.next().await.unwrap().len(), 91);
        assert_eq!(buffered.next().await.unwrap().len(), 97);
    }

    #[tokio::test]
    async fn bitrate_fallback_sets_queued_duration_and_reaches_target() {
        let source = boxed(vec![
            (Bytes::from(vec![b'a'; 500]), false),
            (Bytes::from(vec![b'b'; 500]), false),
        ]);
        let mut buffered =
            StartupBufferedSource::new(source, config(1_000, 4096, 10_000), Some(1_000));
        let first = buffered.next().await.unwrap();
        assert_eq!(first[0], b'a');
        assert_eq!(buffered.stats().buffer_ms, 500);
    }

    #[test]
    fn no_pcr_duration_uses_descriptor_bytes_per_second() {
        let mut reservoir = StartupReservoir::new(config(1_000, 4096, 10_000), Some(1_000));
        assert_eq!(reservoir.push(Bytes::from(vec![b'a'; 500]), true), None);
        assert_eq!(reservoir.duration(), Duration::from_millis(500));
        assert_eq!(
            reservoir.push(Bytes::from(vec![b'b'; 500]), true),
            Some(ReleaseReason::TargetDuration)
        );
        assert_eq!(reservoir.duration(), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn byte_cap_keeps_one_chunk_overhead_then_releases() {
        let source = boxed(vec![
            (Bytes::from(vec![b'a'; 150]), false),
            (Bytes::from(vec![b'b'; 100]), false),
        ]);
        let mut buffered = StartupBufferedSource::new(source, config(10_000, 200, 10_000), None);
        assert_eq!(buffered.next().await.unwrap().len(), 150);
        assert_eq!(buffered.next().await.unwrap().len(), 100);
    }

    #[tokio::test]
    async fn deadline_releases_while_inner_is_stalled() {
        let mut source = FixtureSource::new(vec![(packet(PID, None, false, b'a'), false)]);
        source.delay_after = Some(1);
        let mut buffered =
            StartupBufferedSource::new(Box::new(source), config(10_000, 4096, 10), None);
        assert_eq!(buffered.next().await.unwrap()[20], b'a');
    }

    struct ControlledSource {
        rx: tokio::sync::mpsc::UnboundedReceiver<(Bytes, bool, bool)>,
        discontinuity: bool,
        startup_complete: bool,
    }

    #[async_trait]
    impl TsSource for ControlledSource {
        async fn next(&mut self) -> Option<Bytes> {
            let (bytes, discontinuity, startup_complete) = self.rx.recv().await?;
            self.discontinuity = discontinuity;
            self.startup_complete = startup_complete;
            Some(bytes)
        }
        fn take_discontinuity(&mut self) -> bool {
            std::mem::take(&mut self.discontinuity)
        }
        fn startup_complete(&self) -> bool {
            self.startup_complete
        }
        fn stats(&self) -> SourceStats {
            SourceStats::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn source_gaps_preserve_absolute_deadline_and_ordered_startup_gate() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let source = ControlledSource {
            rx,
            discontinuity: false,
            startup_complete: false,
        };
        let mut buffered =
            StartupBufferedSource::new(Box::new(source), config(30_000, 4096, 10_000), None);
        let retained = packet(PID, Some(0), false, b'c');
        {
            let next = buffered.next();
            tokio::pin!(next);
            assert!(futures::poll!(next.as_mut()).is_pending());
            tokio::time::advance(Duration::from_secs(20)).await;
            assert!(futures::poll!(next.as_mut()).is_pending());

            let started = Instant::now();
            tx.send((packet(PID, Some(0), false, b'a'), false, false))
                .unwrap();
            assert!(futures::poll!(next.as_mut()).is_pending());
            tokio::time::advance(Duration::from_secs(4)).await;
            tx.send((packet(PID, Some(0), false, b'b'), true, false))
                .unwrap();
            assert!(futures::poll!(next.as_mut()).is_pending());
            tokio::time::advance(Duration::from_secs(4)).await;
            tx.send((retained.clone(), true, false)).unwrap();
            assert!(futures::poll!(next.as_mut()).is_pending());
            tokio::time::advance(Duration::from_millis(1999)).await;
            assert!(futures::poll!(next.as_mut()).is_pending());
            tokio::time::advance(Duration::from_millis(1)).await;
            assert_eq!(next.await, Some(retained));
            assert_eq!(Instant::now() - started, Duration::from_secs(10));
        }
        assert!(buffered.take_discontinuity());
        assert!(!buffered.take_discontinuity());
        assert!(!buffered.startup_complete());

        // The deadline cancelled a pending receive, not the source. Subsequent media
        // must pass through without regating or promoting historical data to live.
        for (marker, complete) in [(b'd', false), (b'e', true)] {
            let bytes = packet(PID, Some(90_000), false, marker);
            tx.send((bytes.clone(), false, complete)).unwrap();
            let before = Instant::now();
            assert_eq!(buffered.next().await, Some(bytes));
            assert_eq!(Instant::now(), before);
            assert_eq!(buffered.startup_complete(), complete);
            assert!(!buffered.take_discontinuity());
        }
        drop(tx);
        assert!(buffered.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn empty_startup_burst_waits_for_first_media_byte_without_queueing() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut buffered = StartupBufferedSource::new(
            Box::new(ControlledSource {
                rx,
                discontinuity: false,
                startup_complete: false,
            }),
            config(30_000, 4096, 10_000),
            None,
        );
        let bytes = packet(PID, Some(0), false, b'a');
        {
            let next = buffered.next();
            tokio::pin!(next);
            for _ in 0..16 {
                tx.send((Bytes::new(), false, true)).unwrap();
            }
            assert!(futures::poll!(next.as_mut()).is_pending());
            tokio::time::advance(Duration::from_secs(20)).await;
            assert!(futures::poll!(next.as_mut()).is_pending());
            let first_byte = Instant::now();
            tx.send((bytes.clone(), false, false)).unwrap();
            assert!(futures::poll!(next.as_mut()).is_pending());
            tokio::time::advance(Duration::from_secs(10)).await;
            assert_eq!(next.await, Some(bytes));
            assert_eq!(Instant::now() - first_byte, Duration::from_secs(10));
        }
        assert!(!buffered.startup_complete());
        assert!(!buffered.take_discontinuity());
    }

    #[tokio::test(start_paused = true)]
    async fn empty_gap_at_deadline_preserves_marker_for_next_real_payload() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut buffered = StartupBufferedSource::new(
            Box::new(ControlledSource {
                rx,
                discontinuity: false,
                startup_complete: false,
            }),
            config(30_000, 4096, 10_000),
            None,
        );
        {
            let next = buffered.next();
            tokio::pin!(next);
            tx.send((packet(PID, Some(0), false, b'a'), false, true))
                .unwrap();
            assert!(futures::poll!(next.as_mut()).is_pending());
            tokio::time::advance(Duration::from_secs(9)).await;
            tx.send((Bytes::new(), true, true)).unwrap();
            assert!(futures::poll!(next.as_mut()).is_pending());
            tokio::time::advance(Duration::from_secs(1)).await;
            // The old prefix was discarded; expiry is not an empty payload or EOF.
            assert!(futures::poll!(next.as_mut()).is_pending());
            tx.send((Bytes::new(), true, true)).unwrap();
            assert!(futures::poll!(next.as_mut()).is_pending());
            let bytes = packet(PID, Some(90_000), false, b'b');
            tx.send((bytes.clone(), true, false)).unwrap();
            let before = Instant::now();
            assert_eq!(next.await, Some(bytes));
            assert_eq!(Instant::now(), before, "no second startup gate");
        }
        assert!(!buffered.startup_complete(), "use the real payload's flag");
        assert!(buffered.take_discontinuity());
        assert!(
            !buffered.take_discontinuity(),
            "both gap flags were consumed"
        );
        drop(tx);
        assert!(buffered.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn transport_and_clock_resets_preserve_absolute_deadline() {
        for transport_marker in [false, true] {
            let mut reservoir = StartupReservoir::new(config(30_000, 4096, 10_000), None);
            let started = Instant::now();
            reservoir.push(packet(PID, Some(180_000), false, b'a'), false);
            tokio::time::advance(Duration::from_secs(4)).await;
            reservoir.push(packet(PID, Some(270_000), false, b'b'), false);
            tokio::time::advance(Duration::from_secs(4)).await;
            let reset = packet(PID, Some(90_000), transport_marker, b'c');
            reservoir.push(reset.clone(), false);
            assert_eq!(
                reservoir.deadline(),
                Some(started + Duration::from_secs(10))
            );
            assert_eq!(reservoir.queued_bytes(), reset.len());
            tokio::time::advance(Duration::from_secs(2)).await;
            assert_eq!(
                reservoir.push(packet(PID, Some(180_000), false, b'd'), true),
                Some(ReleaseReason::Deadline)
            );
            let first = reservoir.pop_front().unwrap();
            assert_eq!(first.bytes, reset);
            assert!(!first.startup_complete);
            assert!(reservoir.pop_front().unwrap().startup_complete);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn empty_queue_after_gap_exits_collection_at_original_deadline() {
        let mut reservoir = StartupReservoir::new(config(30_000, 4096, 10_000), None);
        for _ in 0..1024 {
            reservoir.push(Bytes::new(), false);
        }
        assert!(
            reservoir.queue.is_empty(),
            "no retained empty-chunk metadata"
        );
        assert_eq!(reservoir.deadline(), None, "no media byte received yet");
        reservoir.push(packet(PID, Some(0), false, b'a'), false);
        let deadline = reservoir.deadline().unwrap();
        tokio::time::advance(Duration::from_secs(9)).await;
        reservoir.discontinuity();
        assert!(reservoir.queue.is_empty());
        assert_eq!(reservoir.deadline(), Some(deadline));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            reservoir.push(Bytes::new(), true),
            Some(ReleaseReason::Deadline),
            "empty reads must still enforce an existing deadline"
        );
        assert!(reservoir.pop_front().is_none());
        reservoir.finish_drain();
        assert_eq!(reservoir.phase, Phase::Released);
        assert_eq!(reservoir.deadline(), None, "future reads must pass through");
        assert_eq!(reservoir.ready_reason(Instant::now()), None);
    }

    #[tokio::test]
    async fn eof_drains_collected_chunks() {
        let mut buffered = StartupBufferedSource::new(
            boxed(vec![(packet(PID, None, false, b'a'), false)]),
            config(10_000, 4096, 10_000),
            None,
        );
        assert_eq!(buffered.next().await.unwrap()[20], b'a');
        assert!(buffered.next().await.is_none());
    }

    #[test]
    fn reservoir_never_exceeds_cap_plus_one_chunk() {
        let mut reservoir = StartupReservoir::new(config(10_000, 200, 10_000), None);
        assert!(reservoir.push(Bytes::from(vec![0; 150]), true).is_none());
        assert_eq!(
            reservoir.push(Bytes::from(vec![0; 100]), true),
            Some(ReleaseReason::ByteLimit)
        );
        assert_eq!(reservoir.queued_bytes(), 250);
        assert!(reservoir.push(Bytes::from(vec![0; 100]), true).is_some());
        assert_eq!(reservoir.queued_bytes(), 250);
    }

    #[test]
    fn release_event_reports_reason_duration_and_queued_bytes() {
        assert_eq!(
            release_event(ReleaseReason::TargetDuration, Duration::from_millis(20_125), 42_000),
            "[prebuffer] startup buffer released: reason=TargetDuration duration_ms=20125 queued_bytes=42000"
        );
    }

    #[tokio::test]
    async fn delegates_metadata_stats_and_drop() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut source = FixtureSource::new(vec![(Bytes::from_static(b"x"), false)]);
        source.stats = SourceStats {
            peers: 3,
            bitrate: 7,
            downloaded: 11,
            ..SourceStats::default()
        };
        source.dropped = Some(dropped.clone());
        let mut buffered =
            StartupBufferedSource::new(Box::new(source), config(10_000, 4096, 10_000), Some(8));
        assert_eq!(buffered.stats().peers, 3);
        assert_eq!(buffered.metadata(), StreamMetadata::default());
        assert_eq!(buffered.next().await.unwrap(), Bytes::from_static(b"x"));
        drop(buffered);
        assert!(dropped.load(Ordering::SeqCst));
    }
    #[test]
    fn packet_batching_preserves_reset_boundaries_for_every_chunk_split() {
        let modulus = 1_u64 << 33;
        let packets = [
            packet(PID, Some(modulus - 90_000), false, b'a'),
            packet(PID, Some(modulus - 45_000), false, b'b'),
            packet(PID + 1, Some(5_000_000), false, b'x'),
            packet(PID, Some(45_000), false, b'c'), // forward wrap, not a reset
            packet(PID, Some(10_000), false, b'd'), // backwards PCR retains this packet
            packet(PID, Some(100_000), false, b'e'),
            packet(PID, Some(200_000), true, b'f'), // last reset wins, no clock seed
            packet(PID, Some(50_000), false, b'g'),
            packet(PID, Some(140_000), false, b'h'),
            packet(PID, Some(230_000), false, b'i'), // trailing partial is not timed yet
        ];
        let mut data: Vec<u8> = packets.iter().flat_map(|p| p.iter().copied()).collect();
        data.truncate(data.len() - TS_PACKET_LEN / 2);
        let expected = &data[6 * TS_PACKET_LEN..];
        let check = |chunks: Vec<&[u8]>| {
            let mut reservoir = StartupReservoir::new(config(60_000, 16_384, 60_000), None);
            for chunk in chunks {
                assert_eq!(reservoir.push(Bytes::copy_from_slice(chunk), true), None);
                assert!(reservoir.partial.len() < TS_PACKET_LEN);
            }
            let retained: Vec<_> = reservoir
                .queue
                .iter()
                .flat_map(|chunk| chunk.bytes.iter().copied())
                .collect();
            assert_eq!(retained, expected);
            assert_eq!(reservoir.queued_bytes, expected.len());
            assert_eq!(reservoir.clock.elapsed(), Some(Duration::from_secs(1)));
            assert_eq!(reservoir.partial, packets[9][..TS_PACKET_LEN / 2]);
        };
        // Includes empty chunks, every header/PCR/payload split, and both resets in one chunk.
        for split in 0..=data.len() {
            check(vec![&data[..split], &data[split..]]);
        }
        for size in [1, 2, 7, 73, 187, 188, 189, 376, 1316, 16_384] {
            check(data.chunks(size).collect());
        }
    }

    #[test]
    fn packet_batching_reports_the_last_reset_offset_in_queued_coordinates() {
        let first = packet(PID, Some(0), false, b'a');
        let reset = packet(PID, Some(90_000), true, b'b');
        let resumed = packet(PID, Some(0), false, b'c');
        let second_reset = packet(PID, Some(90_000), true, b'd');
        for split in 1..TS_PACKET_LEN {
            let mut reservoir = StartupReservoir::new(config(60_000, 16_384, 60_000), None);
            let prefix = [first.as_ref(), &reset[..split]].concat();
            reservoir.push(Bytes::from(prefix), false);
            let suffix = [&reset[split..], resumed.as_ref(), second_reset.as_ref()].concat();
            assert_eq!(reservoir.observe_bytes(&suffix), Some(3 * TS_PACKET_LEN));
            assert_eq!(
                reservoir.clock.elapsed(),
                None,
                "transport reset is not a PCR seed"
            );
            assert!(reservoir.partial.is_empty());
        }
    }
}
