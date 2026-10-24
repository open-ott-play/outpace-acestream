//! Piece-request scheduling across a pool of peers (pure, no I/O).
//!
//! The async peer loop (connect → signed handshake → read/write) lives in `live_session`;
//! this is the decision core it consults: *given the pieces we still need and what each
//! peer offers, which `(peer, piece)` requests should we issue right now?* It enforces a
//! per-peer in-flight cap, never double-requests a piece, only targets pieces inside a
//! peer's advertised live window, and skips choked peers. Pairs with
//! [`ace_wire::live::LivePicker`] / [`ace_wire::reassembly::PieceReassembler`].

use ace_wire::extended::LivePosition;
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddrV4;

/// A peer as the scheduler sees it this tick.
#[derive(Debug, Clone, Copy)]
pub struct PeerView {
    /// Local handle the driver uses to address this peer.
    pub id: u64,
    /// Lowest piece index this peer advertises (live window `min_piece`).
    pub min_piece: u64,
    /// Highest piece index this peer advertises (live window `max_piece`).
    pub max_piece: u64,
    /// Whether the peer has unchoked us (we may only request when true).
    pub unchoked: bool,
    /// Requests already outstanding to this peer.
    pub in_flight: usize,
}

impl PeerView {
    fn covers(&self, piece: u64) -> bool {
        self.min_piece <= piece && piece <= self.max_piece
    }
}

/// Tracks outstanding piece requests and assigns new ones across peers.
pub struct Scheduler {
    max_in_flight: usize,
    requested: BTreeSet<u64>,
    completed: BTreeSet<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerAssignment {
    pub peer_id: u64,
    pub piece: u64,
}

#[derive(Debug, Clone)]
struct ActivePeer {
    observed_bounds: Option<(u64, u64)>,
    min_piece: u64,
    max_piece: u64,
    unchoked: bool,
    in_flight: BTreeSet<u64>,
}

/// Pure active-peer state paired with [`Scheduler`]. It tracks each peer's current
/// advertised window, choke state, and pieces assigned to that peer, while the scheduler
/// keeps global "already requested" ownership.
#[derive(Debug, Default, Clone)]
pub struct ActivePeers {
    peers: BTreeMap<u64, ActivePeer>,
}

impl ActivePeers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, id: u64, _addr: SocketAddrV4, window: LivePosition) {
        self.peers.insert(
            id,
            ActivePeer {
                observed_bounds: u64::try_from(window.min_piece)
                    .ok()
                    .zip(u64::try_from(window.max_piece).ok())
                    .filter(|(min, max)| min <= max),
                min_piece: window.min_piece.max(0) as u64,
                max_piece: window.max_piece.max(0) as u64,
                unchoked: false,
                in_flight: BTreeSet::new(),
            },
        );
    }

    pub fn set_unchoked(&mut self, id: u64, unchoked: bool) {
        if let Some(peer) = self.peers.get_mut(&id) {
            peer.unchoked = unchoked;
        }
    }

    pub fn update_window(&mut self, id: u64, min_piece: u64, max_piece: u64) {
        if let Some(peer) = self.peers.get_mut(&id) {
            peer.min_piece = min_piece;
            peer.max_piece = max_piece.max(min_piece);
        }
    }

    pub fn assign(
        &mut self,
        scheduler: &mut Scheduler,
        next_needed: u64,
        head: u64,
    ) -> Vec<PeerAssignment> {
        self.assign_if(scheduler, next_needed, head, |_| true)
    }

    /// Admit each scheduled request before recording peer ownership. A driver can reject
    /// work when its bounded command queue is full without losing the request or clearing
    /// an earlier, still-outstanding retry on this or another peer.
    pub fn assign_if(
        &mut self,
        scheduler: &mut Scheduler,
        next_needed: u64,
        head: u64,
        mut admit: impl FnMut(PeerAssignment) -> bool,
    ) -> Vec<PeerAssignment> {
        let views: Vec<PeerView> = self
            .peers
            .iter()
            .map(|(&id, peer)| PeerView {
                id,
                min_piece: peer.min_piece,
                max_piece: peer.max_piece,
                unchoked: peer.unchoked,
                in_flight: peer.in_flight.len(),
            })
            .collect();
        let assigned = scheduler.assign(next_needed, head, &views);
        let mut out = Vec::with_capacity(assigned.len());
        let mut attempted = BTreeSet::new();
        for (peer_id, piece) in assigned {
            attempted.insert(piece);
            if let Some(peer) = self.peers.get_mut(&peer_id) {
                let assignment = PeerAssignment { peer_id, piece };
                if admit(assignment) {
                    peer.in_flight.insert(piece);
                    out.push(assignment);
                } else {
                    scheduler.on_drop(piece);
                }
            } else {
                scheduler.on_drop(piece);
            }
        }
        // Normal scheduling prefers peers with spare capacity, giving slow requests a
        // chance on another peer first. If every usable peer is full, a timeout retry to
        // an existing owner needs no NEW slot. Scan only bounded ownership sets, not the
        // untrusted live head, and never dispatch the same piece twice in one pass.
        if next_needed <= head {
            for (&peer_id, peer) in &self.peers {
                if !peer.unchoked {
                    continue;
                }
                for &piece in peer.in_flight.range(next_needed..=head) {
                    if piece < peer.min_piece
                        || piece > peer.max_piece
                        || scheduler.requested.contains(&piece)
                        || scheduler.completed.contains(&piece)
                        || !attempted.insert(piece)
                    {
                        continue;
                    }
                    let assignment = PeerAssignment { peer_id, piece };
                    if admit(assignment) {
                        scheduler.requested.insert(piece);
                        out.push(assignment);
                    }
                }
            }
        }
        out
    }

    pub fn remove(&mut self, id: u64) -> Vec<u64> {
        self.peers
            .remove(&id)
            .map(|peer| peer.in_flight.into_iter().collect())
            .unwrap_or_default()
    }

    pub fn in_flight_count(&self, id: u64) -> usize {
        self.peers
            .get(&id)
            .map(|peer| peer.in_flight.len())
            .unwrap_or(0)
    }

    /// A piece finished: clear it from every peer's in-flight set (it may be tracked against
    /// more than one peer after a timeout re-request). Keeps a slow peer's slot occupied until
    /// the piece it was asked for actually lands, so [`assign`](Self::assign) keeps steering
    /// new work to faster peers.
    pub fn complete_everywhere(&mut self, piece: u64) {
        for peer in self.peers.values_mut() {
            peer.in_flight.remove(&piece);
        }
    }

    /// Drop in-flight bookkeeping for pieces below `floor` from every peer — call after the
    /// playback cursor jumps forward (a skip past an evicted gap) so stale slots don't wrongly
    /// suppress a peer's capacity forever.
    pub fn prune_below(&mut self, floor: u64) {
        for peer in self.peers.values_mut() {
            peer.in_flight.retain(|&p| p >= floor);
        }
    }

    /// Whether any unchoked peer's advertised window currently covers `piece` (can serve it).
    pub fn any_unchoked_covers(&self, piece: u64) -> bool {
        self.peers
            .values()
            .any(|p| p.unchoked && p.min_piece <= piece && piece <= p.max_piece)
    }

    /// Record window observations independently of scheduler coverage and inferred heads.
    pub fn observe_window(&mut self, id: u64, bounds: Option<(u64, u64)>) {
        if let Some(peer) = self.peers.get_mut(&id) {
            peer.observed_bounds = bounds.filter(|(min, max)| min <= max);
        }
    }

    /// Bounds advertised by currently unchoked peers; invalid signed windows stay unknown.
    pub fn coverage_bounds(&self) -> Option<(u64, u64)> {
        self.peers
            .values()
            .filter(|peer| peer.unchoked)
            .filter_map(|peer| peer.observed_bounds)
            .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
    }

    /// The lowest piece index any unchoked peer can currently serve (min of their windows), or
    /// `None` if no peer is unchoked — the skip target when the needed piece has been evicted
    /// from every window.
    pub fn lowest_covered_piece(&self) -> Option<u64> {
        self.peers
            .values()
            .filter(|p| p.unchoked)
            .map(|p| p.min_piece)
            .min()
    }
}

impl Scheduler {
    /// `max_in_flight` is the per-peer cap on outstanding requests.
    pub fn new(max_in_flight: usize) -> Self {
        Scheduler {
            max_in_flight: max_in_flight.max(1),
            requested: BTreeSet::new(),
            completed: BTreeSet::new(),
        }
    }

    /// Decide requests to issue this tick. Considers pieces in `[next_needed, head]` (the
    /// part of the live window we still want, lowest-first for in-order playback) and
    /// assigns each not-yet-requested piece to an unchoked peer that covers it and still
    /// has spare in-flight capacity. Returns `(peer_id, piece)` pairs and records them as
    /// in-flight.
    pub fn assign(&mut self, next_needed: u64, head: u64, peers: &[PeerView]) -> Vec<(u64, u64)> {
        // Drop bookkeeping for pieces we've moved past.
        self.requested.retain(|&p| p >= next_needed);
        self.completed.retain(|&p| p >= next_needed);

        // Remaining capacity per peer for this tick (only unchoked peers can serve).
        let mut cap: Vec<(PeerView, usize)> = peers
            .iter()
            .filter(|p| p.unchoked)
            .map(|p| (*p, self.max_in_flight.saturating_sub(p.in_flight)))
            .collect();

        let mut total_spare = cap
            .iter()
            .fold(0usize, |total, (_, spare)| total.saturating_add(*spare));
        if total_spare == 0 {
            return Vec::new();
        }

        // Keep the scan horizon at the configured capacity of the eligible peers. Free
        // slots limit assignments, not the lookahead: shrinking the horizon to spare slots
        // hides new pieces behind outstanding/completed indices and leaves capacity idle
        // after an out-of-order completion. This is the same bounded horizon as the initial
        // empty pipeline; it advances with the cursor and never follows an untrusted head
        // arbitrarily far or expands with the completed-piece set.
        let horizon = (self.max_in_flight as u64).saturating_mul(cap.len() as u64);
        let ceiling = head.min(next_needed.saturating_add(horizon));

        let mut out = Vec::new();
        let mut piece = next_needed;
        while piece <= ceiling {
            if !self.requested.contains(&piece) && !self.completed.contains(&piece) {
                // Prefer the peer with the most spare capacity that covers this piece —
                // spreads load and keeps the rarest-window peers in reserve.
                if let Some(slot) = cap
                    .iter_mut()
                    .filter(|(p, c)| *c > 0 && p.covers(piece))
                    .max_by_key(|(_, c)| *c)
                {
                    out.push((slot.0.id, piece));
                    slot.1 -= 1;
                    total_spare -= 1;
                    self.requested.insert(piece);
                }
            }
            if total_spare == 0 {
                break;
            }
            let Some(next) = piece.checked_add(1) else {
                break;
            };
            piece = next;
        }
        out
    }

    /// A request completed: free its slot (the reassembler now owns the bytes).
    pub fn on_complete(&mut self, piece: u64) {
        self.requested.remove(&piece);
        self.completed.insert(piece);
    }

    /// A request failed / the peer dropped: requeue the piece for reassignment.
    pub fn on_drop(&mut self, piece: u64) {
        self.requested.remove(&piece);
    }

    /// Every outstanding request was tied to a peer that dropped. Requeue those requests,
    /// preserving completed pieces still held by the reassembler across the reconnect.
    pub fn clear_in_flight(&mut self) {
        self.requested.clear();
    }

    /// Pieces currently outstanding (for tests / metrics).
    pub fn in_flight_count(&self) -> usize {
        self.requested.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: u64, min: u64, max: u64, unchoked: bool, in_flight: usize) -> PeerView {
        PeerView {
            id,
            min_piece: min,
            max_piece: max,
            unchoked,
            in_flight,
        }
    }

    #[test]
    fn assigns_up_to_in_flight_cap_for_one_peer() {
        let mut s = Scheduler::new(3);
        let got = s.assign(0, 100, &[peer(1, 0, 100, true, 0)]);
        assert_eq!(got, vec![(1, 0), (1, 1), (1, 2)]); // capped at 3
    }

    #[test]
    fn skips_choked_peers() {
        let mut s = Scheduler::new(5);
        assert!(s.assign(0, 10, &[peer(1, 0, 10, false, 0)]).is_empty());
    }

    #[test]
    fn does_not_schedule_beyond_a_bounded_window_ahead_of_the_cursor() {
        // A hostile peer can advertise an implausibly large head while only actually covering
        // a far-away piece. The scheduler must bound its scan to a window ahead of the cursor
        // rather than iterating toward the advertised head, so nothing is assigned this tick.
        let mut s = Scheduler::new(4);
        let far = peer(1, 100_000, 100_000, true, 0);
        assert!(s.assign(0, 200_000, &[far]).is_empty());
    }

    #[test]
    fn a_huge_head_stays_bounded_and_returns_promptly() {
        // With an absurd head (e.g. a Have(u32::MAX)-driven jump), scheduling must still only
        // consider a bounded batch — never spin over billions of piece indices.
        let mut s = Scheduler::new(4);
        let p = peer(1, 0, u64::MAX, true, 0);
        let got = s.assign(0, u64::MAX, &[p]);
        assert_eq!(got, vec![(1, 0), (1, 1), (1, 2), (1, 3)]);
    }

    #[test]
    fn respects_peer_window() {
        let mut s = Scheduler::new(10);
        // Peer only covers [5,7]; we need from 0.
        let got = s.assign(0, 20, &[peer(1, 5, 7, true, 0)]);
        assert_eq!(got, vec![(1, 5), (1, 6), (1, 7)]);
    }

    #[test]
    fn never_double_requests_a_piece() {
        let mut s = Scheduler::new(2);
        let first = s.assign(0, 100, &[peer(1, 0, 100, true, 0)]);
        assert_eq!(first, vec![(1, 0), (1, 1)]);
        // Same pieces still in flight (peer reports in_flight=2): nothing new for piece 0/1.
        let second = s.assign(0, 100, &[peer(1, 0, 100, true, 2)]);
        assert!(second.is_empty());
    }

    #[test]
    fn distributes_across_peers_by_spare_capacity() {
        let mut s = Scheduler::new(2);
        // p1 already has 1 in flight (1 spare); p2 idle (2 spare). p2 should get the first.
        let got = s.assign(
            0,
            100,
            &[peer(1, 0, 100, true, 1), peer(2, 0, 100, true, 0)],
        );
        // 3 total spare slots → 3 assignments for pieces 0,1,2
        assert_eq!(got.len(), 3);
        let pieces: Vec<u64> = got.iter().map(|(_, pc)| *pc).collect();
        assert_eq!(pieces, vec![0, 1, 2]);
        // p2 (more spare) takes the first piece.
        assert_eq!(got[0].0, 2);
    }

    #[test]
    fn on_drop_requeues_and_on_complete_frees() {
        let mut s = Scheduler::new(1);
        assert_eq!(s.assign(0, 10, &[peer(1, 0, 10, true, 0)]), vec![(1, 0)]);
        assert_eq!(s.in_flight_count(), 1);
        // Dropped → can be reassigned (peer now idle again).
        s.on_drop(0);
        assert_eq!(s.assign(0, 10, &[peer(1, 0, 10, true, 0)]), vec![(1, 0)]);
        // Completed → advancing next_needed past it, no re-request.
        s.on_complete(0);
        let next = s.assign(1, 10, &[peer(1, 0, 10, true, 0)]);
        assert_eq!(next, vec![(1, 1)]);
    }

    #[test]
    fn clear_in_flight_requeues_every_outstanding_piece_after_peer_drop() {
        let mut s = Scheduler::new(3);
        assert_eq!(
            s.assign(0, 10, &[peer(1, 0, 10, true, 0)]),
            vec![(1, 0), (1, 1), (1, 2)]
        );
        assert_eq!(s.in_flight_count(), 3);

        s.clear_in_flight();

        assert_eq!(s.in_flight_count(), 0);
        assert_eq!(
            s.assign(0, 10, &[peer(2, 0, 10, true, 0)]),
            vec![(2, 0), (2, 1), (2, 2)]
        );
    }

    fn addr() -> SocketAddrV4 {
        use std::net::Ipv4Addr;
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)
    }

    fn win(min: i64, max: i64) -> LivePosition {
        LivePosition {
            min_piece: min,
            max_piece: max,
            position: -1,
            distance_from_source: 1,
        }
    }

    #[test]
    fn timeout_rerequest_prefers_a_different_peer_via_spare_capacity() {
        // Peer 1 was asked for piece 5 but never delivered (still in-flight). On re-request,
        // the scheduler must NOT pick peer 1 again while idle peer 2 also covers the piece.
        let mut ap = ActivePeers::new();
        ap.insert(1, addr(), win(0, 100));
        ap.set_unchoked(1, true);
        let mut s = Scheduler::new(4);
        // Only peer 1 is present, so it gets piece 5.
        let first = ap.assign(&mut s, 5, 5);
        assert_eq!(
            first,
            vec![PeerAssignment {
                peer_id: 1,
                piece: 5
            }]
        );
        // A second, idle peer joins that also covers the piece.
        ap.insert(2, addr(), win(0, 100));
        ap.set_unchoked(2, true);
        // Piece 5 timed out: requeue globally but keep peer 1's slot occupied.
        s.on_drop(5);
        // Re-assign: peer 1 still shows piece 5 in-flight (less spare) -> peer 2 gets it.
        let retry = ap.assign(&mut s, 5, 5);
        assert_eq!(
            retry,
            vec![PeerAssignment {
                peer_id: 2,
                piece: 5
            }]
        );
        // When it finally lands, both peers' slots for piece 5 clear.
        ap.complete_everywhere(5);
        assert_eq!(ap.in_flight_count(1), 0);
        assert_eq!(ap.in_flight_count(2), 0);
    }

    #[test]
    fn lowest_covered_and_coverage_reflect_unchoked_windows() {
        let mut ap = ActivePeers::new();
        ap.insert(1, addr(), win(50, 100));
        ap.insert(2, addr(), win(80, 130));
        assert!(!ap.any_unchoked_covers(90)); // nobody unchoked yet
        assert_eq!(ap.lowest_covered_piece(), None);
        ap.set_unchoked(2, true);
        assert!(ap.any_unchoked_covers(90)); // peer 2 covers 90
        assert!(!ap.any_unchoked_covers(60)); // 60 only in peer 1's window, still choked
        assert_eq!(ap.lowest_covered_piece(), Some(80));
        ap.set_unchoked(1, true);
        assert_eq!(ap.lowest_covered_piece(), Some(50));
    }

    #[test]
    fn prune_below_clears_stale_slots_after_a_skip() {
        let mut ap = ActivePeers::new();
        ap.insert(1, addr(), win(0, 100));
        ap.set_unchoked(1, true);
        let mut s = Scheduler::new(4);
        ap.assign(&mut s, 0, 3); // pieces 0..=3 in flight on peer 1
        assert_eq!(ap.in_flight_count(1), 4);
        ap.prune_below(2); // cursor skipped to 2
        assert_eq!(ap.in_flight_count(1), 2); // only 2,3 remain
    }

    #[test]
    fn drops_bookkeeping_below_next_needed() {
        let mut s = Scheduler::new(5);
        s.assign(0, 4, &[peer(1, 0, 4, true, 0)]); // requests 0..=4
        assert_eq!(s.in_flight_count(), 5);
        // Window advanced to 3 (live edge moved); stale entries pruned.
        s.assign(3, 4, &[peer(1, 0, 4, true, 5)]);
        assert_eq!(s.in_flight_count(), 2); // only 3,4 remain
    }
    #[test]
    fn out_of_order_completion_refills_without_shrinking_the_horizon() {
        let mut s = Scheduler::new(4);
        assert_eq!(
            s.assign(0, u64::MAX, &[peer(1, 0, u64::MAX, true, 0)]),
            vec![(1, 0), (1, 1), (1, 2), (1, 3)]
        );
        // The cursor still needs 0, but completion of 1 frees a slot. The remaining
        // outstanding indices must not hide piece 4 from that slot.
        s.on_complete(1);
        assert_eq!(
            s.assign(0, u64::MAX, &[peer(1, 0, u64::MAX, true, 3)]),
            vec![(1, 4)]
        );
        assert_eq!(s.in_flight_count(), 4);

        // Further completions leave free slots, but all pieces inside the fixed horizon
        // are already requested or complete. Do not re-request them or expand the horizon.
        for piece in [2, 3, 4] {
            s.on_complete(piece);
        }
        assert!(s
            .assign(0, u64::MAX, &[peer(1, 0, u64::MAX, true, 1)])
            .is_empty());
        assert_eq!(s.in_flight_count(), 1);
    }

    #[test]
    fn cursor_progress_keeps_the_configured_pipeline_full() {
        let mut s = Scheduler::new(4);
        s.assign(0, 100, &[peer(1, 0, 100, true, 0)]);
        for completed in 0..8 {
            s.on_complete(completed);
            assert_eq!(
                s.assign(completed + 1, 100, &[peer(1, 0, 100, true, 3)]),
                vec![(1, completed + 4)]
            );
            assert_eq!(s.in_flight_count(), 4);
        }
    }

    #[test]
    fn bounded_horizon_terminates_at_the_largest_piece_index() {
        let mut s = Scheduler::new(4);
        let p = peer(1, u64::MAX - 1, u64::MAX, true, 0);
        assert_eq!(
            s.assign(u64::MAX - 1, u64::MAX, &[p]),
            vec![(1, u64::MAX - 1), (1, u64::MAX)]
        );
        s.on_complete(u64::MAX);
        assert!(s
            .assign(
                u64::MAX - 1,
                u64::MAX,
                &[peer(1, u64::MAX - 1, u64::MAX, true, 1)]
            )
            .is_empty());
    }

    #[test]
    fn completed_out_of_order_piece_is_not_requested_again() {
        let mut active = ActivePeers::new();
        active.insert(1, addr(), win(0, 10));
        active.set_unchoked(1, true);
        let mut scheduler = Scheduler::new(4);
        active.assign(&mut scheduler, 0, 3);
        active.complete_everywhere(1);
        scheduler.on_complete(1);

        // Piece 1 is already in the reassembler while piece 0 still holds the cursor.
        assert!(active.assign(&mut scheduler, 0, 3).is_empty());
        scheduler.on_drop(1);
        assert!(active.assign(&mut scheduler, 0, 3).is_empty());
        assert_eq!(active.in_flight_count(1), 3);

        // Reconnect resets outstanding ownership, but the reassembler keeps piece 1.
        scheduler.clear_in_flight();
        active.remove(1);
        active.insert(2, addr(), win(0, 10));
        active.set_unchoked(2, true);
        let requested = active.assign(&mut scheduler, 0, 3);
        assert_eq!(
            requested
                .iter()
                .map(|request| request.piece)
                .collect::<Vec<_>>(),
            vec![0, 2, 3]
        );

        // The completed exclusion is discarded once playback passes it.
        active.assign(&mut scheduler, 2, 3);
        assert!(scheduler.completed.is_empty());
    }

    #[test]
    fn rejected_admission_preserves_existing_retry_ownership() {
        let mut active = ActivePeers::new();
        active.insert(1, addr(), win(0, 10));
        active.set_unchoked(1, true);
        let mut scheduler = Scheduler::new(4);
        active.assign(&mut scheduler, 5, 5);
        scheduler.on_drop(5);

        // A rejected retransmit to the same peer must keep its original request slot.
        assert!(active.assign_if(&mut scheduler, 5, 5, |_| false).is_empty());
        assert_eq!(active.in_flight_count(1), 1);
        assert_eq!(scheduler.in_flight_count(), 0);

        active.insert(2, addr(), win(0, 10));
        active.set_unchoked(2, true);
        assert!(active
            .assign_if(&mut scheduler, 5, 5, |assignment| {
                assert_eq!(assignment.peer_id, 2);
                false
            })
            .is_empty());
        assert_eq!(active.in_flight_count(1), 1);
        assert_eq!(active.in_flight_count(2), 0);
        assert_eq!(scheduler.in_flight_count(), 0);

        assert_eq!(active.assign(&mut scheduler, 5, 5).len(), 1);
        assert_eq!(active.in_flight_count(2), 1);
    }
}
