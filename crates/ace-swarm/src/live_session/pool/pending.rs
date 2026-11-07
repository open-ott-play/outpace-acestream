use super::*;
use std::collections::{BTreeSet, VecDeque};

pub(super) type OutputReservation = Pin<
    Box<
        dyn Future<Output = Result<mpsc::OwnedPermit<LiveOutput>, mpsc::error::SendError<()>>>
            + Send,
    >,
>;

/// One accepted piece after TS alignment; ownership survives pool replacement.
/// No later piece is consumed until every output boundary in this batch is sent.
pub(in crate::live_session) struct PendingOutput {
    chunks: VecDeque<LiveOutput>,
    producers: BTreeSet<SocketAddrV4>,
    authenticated: bool,
}

/// At most one wire piece is processed per call, even if the recovery gate filters it.
pub(super) fn prepare_output(c: &mut Continuity, info: &StreamInfo, seed: &SeedConfig) -> bool {
    if c.pending_output.is_some() || c.startup_deadline.is_some() {
        return false;
    }
    c.skip_far_behind_live(Instant::now());
    let before = c.reasm.next_needed();
    let Some(ready) = c.reasm.take_ready_piece() else {
        return false;
    };
    let after = c.reasm.next_needed();
    c.request_floor.fetch_max(after, Ordering::Relaxed);
    if let Some(metrics) = &seed.metrics {
        metrics.received_bytes(ready.len() as u64);
    }
    let producers = c
        .piece_producers
        .range(before..after)
        .filter_map(|(_, p)| *p)
        .collect();
    c.piece_producers.retain(|&p, _| p >= after);
    let authenticated = info.sig_len > 0
        && ace_wire::live_auth::signature_len_from_pubkey_der(&info.source_pubkey)
            == Some(info.sig_len);
    if !ready.is_empty() && !c.authenticated_logged && authenticated {
        c.authenticated_logged = true;
        crate::alog!("[ace] discovery stage=authenticated-contiguous");
    }
    let mut chunks: VecDeque<_> = c.resync_output(&ready).into();
    if let Some(last) = chunks.back_mut() {
        last.startup_complete = c.output_startup_complete();
        c.pending_output = Some(PendingOutput {
            chunks,
            producers,
            authenticated,
        });
    }
    publish_live_stats(c, seed.metrics.as_ref(), info);
    true
}

/// Counters and productive peer credit advance only once capacity is actually owned.
#[allow(clippy::too_many_arguments)]
pub(super) fn finish_output(
    c: &mut Continuity,
    info: &StreamInfo,
    permit: mpsc::OwnedPermit<LiveOutput>,
    downloaded: &Arc<AtomicU64>,
    seed: &SeedConfig,
    candidates: &mut SessionCandidates,
    peers: &mut BTreeMap<u64, PeerRuntime>,
) {
    let pending = c
        .pending_output
        .as_mut()
        .expect("reservation has retained output");
    let output = pending
        .chunks
        .pop_front()
        .expect("retained batch is nonempty");
    let len = output.bytes.len() as u64;
    permit.send(output);
    if c.emitted == 0 {
        crate::alog!("[ace] discovery stage=first-source-output");
    }
    c.emitted += len;
    downloaded.fetch_add(len, Ordering::Relaxed);
    if c.emitted >= c.next_log {
        crate::alog!(
            "[ace] served {} MiB (head={}, next piece needed={})",
            c.emitted >> 20,
            c.head,
            c.reasm.next_needed()
        );
        c.next_log = c.emitted + (4 << 20);
    }
    // Apply credit exactly once, even for multiple recovery boundaries in one piece.
    for producer in std::mem::take(&mut pending.producers) {
        for peer in peers.values_mut().filter(|peer| peer.addr == producer) {
            peer.produced_output = true;
        }
        candidates.productive(producer);
        if pending.authenticated {
            seed.warm_peers
                .record_productive(info.infohash, producer, candidates.kind(producer));
        }
    }
    if pending.chunks.is_empty() {
        c.pending_output = None;
    }
    c.next_needed_since = Instant::now();
}

type ReplyJob = Pin<Box<dyn Future<Output = Option<crate::seed::protocol::PreparedReply>> + Send>>;
#[derive(Default)]
pub(super) struct Reciprocal {
    queues: BTreeMap<u64, VecDeque<(u32, u16)>>,
    last: Option<u64>,
    job: Option<(u64, ReplyJob)>,
}
impl Reciprocal {
    pub(super) fn queue(&mut self, peer: u64, payload: &[u8]) {
        if payload.len() < 10 {
            return;
        }
        let queue = self.queues.entry(peer).or_default();
        // Keep coordinates only, not attacker-sized wire payloads. Overflow can retry.
        if queue.len() < 64 {
            queue.push_back((
                u32::from_be_bytes(payload[4..8].try_into().unwrap()),
                u16::from_be_bytes(payload[8..10].try_into().unwrap()),
            ));
        }
    }
    pub(super) fn start(
        &mut self,
        peers: &BTreeMap<u64, PeerRuntime>,
        store: &Arc<tokio::sync::Mutex<PieceStore>>,
    ) {
        self.queues
            .retain(|id, q| peers.contains_key(id) && !q.is_empty());
        if self
            .job
            .as_ref()
            .is_some_and(|(id, _)| !peers.contains_key(id))
        {
            self.job = None;
        }
        if self.job.is_some() {
            return;
        }
        let next = self
            .last
            .and_then(|last| {
                self.queues
                    .range((std::ops::Bound::Excluded(last), std::ops::Bound::Unbounded))
                    .next()
                    .map(|(&id, _)| id)
            })
            .or_else(|| self.queues.keys().next().copied());
        let Some(id) = next else {
            return;
        };
        let (piece, chunk) = self.queues.get_mut(&id).unwrap().pop_front().unwrap();
        self.last = Some(id);
        let store = store.clone();
        self.job = Some((
            id,
            Box::pin(async move {
                let request = chunk_request(piece, chunk);
                prepare_reply(&request, Some(&store), ServeProfile::Reciprocal).await
            }),
        ));
    }
    pub(super) fn has_job(&self) -> bool {
        self.job.is_some()
    }
    pub(super) async fn next(&mut self) -> (u64, Option<crate::seed::protocol::PreparedReply>) {
        let (id, job) = self.job.as_mut().expect("enabled reciprocal job");
        let result = (*id, job.await);
        self.job = None;
        result
    }
}

#[cfg(test)]
mod tests;
