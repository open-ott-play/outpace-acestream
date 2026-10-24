use super::*;

#[tokio::test]
async fn queued_live_outputs_keep_each_discontinuity_with_its_chunk() {
    let (tx, rx) = mpsc::channel(8);
    for (bytes, discontinuity) in [
        (b"pre".as_slice(), false),
        (b"post-one".as_slice(), true),
        (b"between".as_slice(), false),
        (b"post-two".as_slice(), true),
    ] {
        tx.send(LiveOutput {
            bytes: Bytes::copy_from_slice(bytes),
            discontinuity,
            startup_complete: true,
        })
        .await
        .unwrap();
    }
    drop(tx);
    let mut source = AceSource {
        metrics: None,
        _sampler: None,
        rx,
        worker: None,
        discontinuity: false,
        startup_complete: true,
        peers: Arc::new(AtomicU32::new(0)),
        downloaded: Arc::new(AtomicU64::new(0)),
        uploaded: Arc::new(AtomicU64::new(0)),
        peers_served: Arc::new(AtomicU32::new(0)),
        metadata: StreamMetadata::default(),
    };

    for (expected, gap) in [
        (b"pre".as_slice(), false),
        (b"post-one".as_slice(), true),
        (b"between".as_slice(), false),
        (b"post-two".as_slice(), true),
    ] {
        assert_eq!(source.next().await.unwrap().as_ref(), expected);
        assert_eq!(source.take_discontinuity(), gap);
    }
}

#[tokio::test]
async fn dropping_live_source_cancels_an_unresponsive_owned_worker() {
    let (_tx, rx) = mpsc::channel(1);
    let worker = tokio::spawn(std::future::pending());
    let abort = worker.abort_handle();
    let source = AceSource {
        metrics: None,
        _sampler: None,
        rx,
        worker: Some(worker),
        discontinuity: false,
        startup_complete: true,
        peers: Arc::new(AtomicU32::new(0)),
        downloaded: Arc::new(AtomicU64::new(0)),
        uploaded: Arc::new(AtomicU64::new(0)),
        peers_served: Arc::new(AtomicU32::new(0)),
        metadata: StreamMetadata::default(),
    };
    drop(source);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !abort.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("source drop must abort worker without media or network progress");
}

#[tokio::test]
async fn startup_readiness_stays_ordered_through_producer_reservoir_and_broadcast() {
    use crate::session::{StreamEvent, StreamSession};
    let (tx, rx) = mpsc::channel(8);
    // The producer has already completed startup before the consumer reads anything.
    for (marker, startup_complete) in [(1, false), (2, false), (3, true)] {
        tx.send(LiveOutput {
            bytes: Bytes::from(vec![marker; 188]),
            discontinuity: false,
            startup_complete,
        })
        .await
        .unwrap();
    }
    let source = AceSource {
        metrics: None,
        _sampler: None,
        worker: None,
        rx,
        discontinuity: false,
        startup_complete: false,
        peers: Arc::new(AtomicU32::new(0)),
        downloaded: Arc::new(AtomicU64::new(0)),
        uploaded: Arc::new(AtomicU64::new(0)),
        peers_served: Arc::new(AtomicU32::new(0)),
        metadata: StreamMetadata::default(),
    };
    let buffered = StartupBufferedSource::new(
        Box::new(source),
        StartupBufferConfig {
            target_ms: 30_000,
            max_bytes: 400,
            timeout_ms: 1000,
        },
        None,
    );
    let session = StreamSession::start(buffered, 8);
    let mut receiver = session.raw_receiver();
    // Collection reads all three chunks to reach the byte cap, so the inner source is
    // ready while the reservoir still owns two historical chunks. Neither may inherit it.
    for marker in [1, 2] {
        assert_eq!(
            receiver.recv().await.unwrap(),
            StreamEvent::StartupData(Bytes::from(vec![marker; 188]))
        );
    }
    assert_eq!(
        receiver.recv().await.unwrap(),
        StreamEvent::Data(Bytes::from(vec![3; 188]))
    );
    let mut late = session.raw_receiver();
    tx.send(LiveOutput {
        bytes: Bytes::from_static(b"late"),
        discontinuity: false,
        startup_complete: true,
    })
    .await
    .unwrap();
    assert_eq!(
        late.recv().await.unwrap(),
        StreamEvent::Data(Bytes::from_static(b"late"))
    );
}

#[tokio::test]
async fn finite_source_closes_current_and_late_session_receivers() {
    let (tx, rx) = mpsc::channel(1);
    let source = AceSource {
        metrics: None,
        _sampler: None,
        rx,
        worker: None,
        discontinuity: false,
        startup_complete: true,
        peers: Arc::new(AtomicU32::new(0)),
        downloaded: Arc::new(AtomicU64::new(0)),
        uploaded: Arc::new(AtomicU64::new(0)),
        peers_served: Arc::new(AtomicU32::new(0)),
        metadata: StreamMetadata::default(),
    };
    let session = crate::session::StreamSession::start(Box::new(source), 2);
    let mut receiver = session.raw_receiver();
    drop(tx);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .is_err()
    );
    tokio::task::yield_now().await;
    assert!(session.is_finished());
    assert!(session.raw_receiver().recv().await.is_err());
}
