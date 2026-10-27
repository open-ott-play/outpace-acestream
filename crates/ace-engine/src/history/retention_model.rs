//! A fixed transition oracle, deliberately not another implementation of pruning.
//! Expected retained sequences and charges are written out independently below.

use super::*;

fn append(store: &mut HistoryStore, seq: u64, marker: u8, allocation: usize, now: Instant) {
    let mut bytes = Vec::with_capacity(allocation);
    bytes.extend_from_slice(&[marker; 16]);
    store.push(seq, Bytes::from(bytes), allocation, 1.0, false, true, now);
}

fn assert_retained(
    store: &mut HistoryStore,
    now: Instant,
    sequences: &[u64],
    bytes: usize,
    seekable_start_ms: Option<u64>,
) {
    let status = store.status(now);
    let observed: Vec<_> = status
        .intervals
        .iter()
        .flat_map(|interval| interval.first_seq..=interval.last_seq)
        .collect();
    assert_eq!(observed, sequences);
    assert_eq!(status.first_seq, sequences.first().copied());
    assert_eq!(status.last_seq, sequences.last().copied());
    assert_eq!(status.retained_bytes, bytes);
    assert_eq!(status.seekable_start_ms, seekable_start_ms);
}

#[test]
fn independent_model_tracks_retention_and_shared_budget_lifecycle() {
    let config = HlsHistoryConfig {
        max_seconds: 10,
        max_bytes: 600,
        global_max_bytes: 800,
    };
    config.validate().unwrap();
    let budget = HistoryBudget::new(config.global_max_bytes);
    let mut a = HistoryStore::new(config, budget.clone());
    let mut b = HistoryStore::new(config, budget.clone());
    let start = Instant::now();
    let at = |seconds| start + Duration::from_secs(seconds);

    // A owns 600 of the shared 800 bytes. Visible payloads are only 16 bytes;
    // the oracle counts backing allocations, not payload length.
    append(&mut a, 0, 0xa0, 200, at(0));
    append(&mut a, 1, 0xa1, 200, at(1));
    append(&mut a, 2, 0xa2, 200, at(2));
    assert_retained(&mut a, at(2), &[0, 1, 2], 600, Some(0));
    assert_retained(&mut b, at(2), &[], 0, None);
    let original_a = a.prepare(Duration::from_secs(3), at(2)).unwrap();
    let original_a_playlist = a.playlist(&original_a, "/synthetic/a", at(2)).unwrap();
    assert_eq!(
        original_a,
        HlsHistorySnapshot {
            first_seq: 0,
            last_seq: 2,
            start_ms: 0,
            end_ms: 3000,
        }
    );

    append(&mut b, 0, 0xb0, 200, at(2));
    let original_b = b.prepare(Duration::from_secs(1), at(2)).unwrap();
    assert_retained(&mut b, at(2), &[0], 200, Some(0));

    // A remains untouched when B cannot fit even after releasing its old 200
    // bytes: 600 + 300 exceeds the global cap. B's failed head cannot be read.
    append(&mut b, 1, 0xb1, 300, at(3));
    assert_retained(&mut a, at(3), &[0, 1, 2], 600, Some(0));
    assert_retained(&mut b, at(3), &[], 0, None);
    assert_eq!(
        a.playlist(&original_a, "/synthetic/a", at(3)).unwrap(),
        original_a_playlist
    );
    assert_eq!(b.segment(&original_b, 0, at(3)), Err(HlsHistoryError::Gone));

    // A's per-source cap now evicts only sequence 0. Previously consumed
    // prefixes may disappear while unread fragments of that view remain valid.
    append(&mut a, 3, 0xa3, 200, at(4));
    append(&mut b, 2, 0xb2, 200, at(4));
    assert_retained(&mut a, at(4), &[1, 2, 3], 600, Some(1000));
    assert_retained(&mut b, at(4), &[2], 200, Some(2000));
    assert_eq!(
        a.playlist(&original_a, "/synthetic/a", at(4)),
        Err(HlsHistoryError::Gone)
    );
    assert_eq!(
        a.segment(&original_a, 1, at(4)).unwrap().as_ref(),
        &[0xa1; 16]
    );
    assert_eq!(
        a.segment(&original_a, 2, at(4)).unwrap().as_ref(),
        &[0xa2; 16]
    );
    assert_eq!(a.segment(&original_a, 3, at(4)), Err(HlsHistoryError::Gone));
    let retained_a = a.prepare(Duration::from_secs(3), at(4)).unwrap();
    let retained_a_playlist = a.playlist(&retained_a, "/synthetic/a", at(4)).unwrap();

    // An oversized new segment advances source time but does not become a
    // seekable head or mutate an already captured finite replay.
    append(&mut a, 4, 0xa4, 601, at(5));
    assert_retained(&mut a, at(5), &[1, 2, 3], 600, None);
    assert_eq!(
        a.prepare(Duration::from_secs(1), at(5)),
        Err(HlsHistoryError::Unavailable)
    );
    assert_eq!(
        a.playlist(&retained_a, "/synthetic/a", at(5)).unwrap(),
        retained_a_playlist
    );

    append(&mut b, 3, 0xb3, 200, at(6));
    append(&mut b, 4, 0xb4, 200, at(7));
    assert_retained(&mut b, at(7), &[4], 200, Some(4000));

    // At the exact age boundary, only A1 expires. Its released 200 bytes allow
    // B5 to coexist with B4, unlike B's preceding capacity-constrained pushes.
    a.prune(at(11));
    assert_retained(&mut a, at(11), &[2, 3], 400, None);
    append(&mut b, 5, 0xb5, 200, at(11));
    assert_retained(&mut b, at(11), &[4, 5], 400, Some(4000));
    let retained_b = b.prepare(Duration::from_secs(2), at(11)).unwrap();
    let retained_b_playlist = b.playlist(&retained_b, "/synthetic/b", at(11)).unwrap();

    // Observe released shared charges through admission, not private counters:
    // without dropping A, admitting B6 would have to discard B4.
    drop(a);
    append(&mut b, 6, 0xb6, 200, at(11));
    assert_retained(&mut b, at(11), &[4, 5, 6], 600, Some(4000));
    assert_eq!(
        b.playlist(&retained_b, "/synthetic/b", at(11)).unwrap(),
        retained_b_playlist
    );
    assert_eq!(
        b.segment(&retained_b, 6, at(11)),
        Err(HlsHistoryError::Gone)
    );

    b.prune(at(17));
    assert_retained(&mut b, at(17), &[5, 6], 400, Some(5000));
    assert_eq!(
        b.playlist(&retained_b, "/synthetic/b", at(17)),
        Err(HlsHistoryError::Gone)
    );
    assert_eq!(
        b.segment(&retained_b, 4, at(17)),
        Err(HlsHistoryError::Gone)
    );
    assert_eq!(
        b.segment(&retained_b, 5, at(17)).unwrap().as_ref(),
        &[0xb5; 16]
    );

    // Reuse the same shared budget after replacing A, then drop B. Successful
    // retention of both new A fragments proves B's charges were released too.
    let mut a = HistoryStore::new(config, budget.clone());
    append(&mut a, 0, 0xc0, 400, at(17));
    assert_retained(&mut a, at(17), &[0], 400, Some(0));
    drop(b);
    append(&mut a, 1, 0xc1, 200, at(18));
    assert_retained(&mut a, at(18), &[0, 1], 600, Some(0));

    // Give the replacement enough byte capacity for all three fragments, so
    // only the media-duration bound can evict the first one at this instant.
    drop(a);
    let mut a = HistoryStore::new(config, budget.clone());
    append(&mut a, 0, 0xc0, 200, at(18));
    append(&mut a, 1, 0xc1, 200, at(18));
    let mut long_segment = Vec::with_capacity(200);
    long_segment.extend_from_slice(&[0xc2; 16]);
    a.push(2, Bytes::from(long_segment), 200, 9.0, false, true, at(18));
    assert_retained(&mut a, at(18), &[1, 2], 400, Some(1000));
    let final_view = a.prepare(Duration::from_secs(10), at(18)).unwrap();
    assert_eq!((final_view.start_ms, final_view.end_ms), (1000, 11000));
    a.prune(at(28));
    assert_retained(&mut a, at(28), &[], 0, None);
    assert_eq!(
        a.segment(&final_view, 2, at(28)),
        Err(HlsHistoryError::Gone)
    );

    // A fresh full-size reservation also proves the last time-pruned charge
    // did not leak into the shared budget.
    let mut b = HistoryStore::new(config, budget);
    append(&mut b, 0, 0xd0, 600, at(28));
    assert_retained(&mut b, at(28), &[0], 600, Some(0));
}
