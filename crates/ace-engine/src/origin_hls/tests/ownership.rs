use super::*;
use futures::StreamExt;

async fn media_response() -> (Fixture, Arc<OriginHls>, OriginPlayback, OriginResponse) {
    let origin = Fixture::new(|path, _| match path {
        "/index.m3u8" => manifest("#EXTM3U\n#EXTINF:1,\nsegment.ts\n"),
        "/segment.ts" => (
            [(axum::http::header::CACHE_CONTROL, "no-store")],
            Bytes::from(vec![0x47; BODY_SLICE * 2 + 17]),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let relay = relay();
    let playback = relay.start(&origin.url("/index.m3u8")).await.unwrap();
    let playlist = relay.resource(token(&playback), "0", None).await.unwrap();
    let uri = uris(&playlist.body).remove(0);
    drop(playlist);
    let response = relay
        .resource(token(&playback), resource_id(&uri), None)
        .await
        .unwrap();
    (origin, relay, playback, response)
}

fn pending(relay: &OriginHls, playback: &OriginPlayback) -> usize {
    relay.sessions.lock().unwrap()[token(playback)]
        .inner
        .lock()
        .unwrap()
        .pending
}

#[tokio::test]
async fn emitted_chunks_keep_capacity_after_eof_and_early_stream_drop() {
    for drain in [false, true] {
        let (_origin, relay, playback, response) = media_response().await;
        let original = response.body.as_ptr();
        let mut stream = Box::pin(response.into_stream());
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.len(), BODY_SLICE);
        assert_eq!(chunk.as_ptr(), original, "streaming must remain zero-copy");
        let retained = chunk.slice(1..2);
        drop(chunk);
        if drain {
            while let Some(chunk) = stream.next().await {
                drop(chunk.unwrap());
            }
        }
        drop(stream);
        assert_eq!(relay.admission.available_permits(), 15);
        assert_eq!(relay.fetches.available_permits(), 3);
        assert_eq!(pending(&relay, &playback), 1);
        drop(retained);
        assert_eq!(relay.admission.available_permits(), 16);
        assert_eq!(relay.fetches.available_permits(), 4);
        assert_eq!(pending(&relay, &playback), 0);
        assert!(relay.stop(token(&playback)));
    }
}

#[tokio::test]
async fn body_clones_keep_capacity_after_response_drop() {
    let (_origin, relay, playback, response) = media_response().await;
    let retained = response.body.slice(1..2);
    drop(response);
    assert_eq!(relay.admission.available_permits(), 15);
    assert_eq!(relay.fetches.available_permits(), 3);
    assert_eq!(pending(&relay, &playback), 1);
    drop(retained);
    assert_eq!(relay.admission.available_permits(), 16);
    assert_eq!(relay.fetches.available_permits(), 4);
    assert_eq!(pending(&relay, &playback), 0);
    assert!(relay.stop(token(&playback)));
}

#[tokio::test]
async fn stop_cancels_stream_but_keeps_capacity_until_last_chunk_drops() {
    let (_origin, relay, playback, response) = media_response().await;
    let session = Arc::downgrade(&relay.sessions.lock().unwrap()[token(&playback)]);
    let mut stream = Box::pin(response.into_stream());
    let retained = stream.next().await.unwrap().unwrap();
    assert!(relay.stop(token(&playback)));
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    drop(stream);
    assert_eq!(relay.admission.available_permits(), 15);
    assert_eq!(relay.fetches.available_permits(), 3);
    assert!(session.upgrade().is_some());
    drop(retained);
    assert_eq!(relay.admission.available_permits(), 16);
    assert_eq!(relay.fetches.available_permits(), 4);
    assert!(session.upgrade().is_none());
}

#[tokio::test]
async fn replay_slice_keeps_admission_and_history_charge_without_cache_cycles() {
    let origin = Fixture::new(|path, _| match path {
        "/index.m3u8" => manifest(
            "#EXTM3U\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:2,\nsegment.ts\n",
        ),
        "/segment.ts" => (
            [(axum::http::header::CACHE_CONTROL, "max-age=30")],
            recorded_ts(),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let relay = history_relay();
    let playback = relay.start(&origin.url("/index.m3u8")).await.unwrap();
    let playlist = relay.resource(token(&playback), "0", None).await.unwrap();
    let uri = uris(&playlist.body).remove(0);
    drop(playlist);
    for _ in 0..2 {
        drop(
            relay
                .resource(token(&playback), resource_id(&uri), None)
                .await
                .unwrap(),
        );
        assert_eq!(relay.admission.available_permits(), 16);
        assert_eq!(relay.fetches.available_permits(), 4);
        assert_eq!(pending(&relay, &playback), 0);
    }
    assert_eq!(origin.count("/segment.ts"), 1, "second request was cached");
    let view = relay
        .seek(token(&playback), OriginSeekTarget::PositionMs(0), Some(0))
        .unwrap();
    let response = relay
        .replay_resource(token(&playback), view.generation, "10.ts")
        .await
        .unwrap();
    let retained = response.body.slice(1..2);
    drop(response);
    assert_eq!(relay.admission.available_permits(), 15);
    assert_eq!(relay.fetches.available_permits(), 4);
    assert_eq!(pending(&relay, &playback), 1);
    let session = Arc::downgrade(&relay.sessions.lock().unwrap()[token(&playback)]);
    assert!(relay.stop(token(&playback)));
    assert!(relay.cache.lock().unwrap().entries.is_empty());
    assert!(relay
        .history_budget
        .reserve(relay.history_config.global_max_bytes)
        .is_none());
    drop(retained);
    assert_eq!(relay.admission.available_permits(), 16);
    assert!(
        session.upgrade().is_none(),
        "cache/history do not retain a request cycle"
    );
    assert!(relay
        .history_budget
        .reserve(relay.history_config.global_max_bytes)
        .is_some());
}
