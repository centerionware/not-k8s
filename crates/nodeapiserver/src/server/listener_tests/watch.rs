#[tokio::test]
async fn watch_response_body_streams_the_replay_then_live_events() {
    use http_body_util::BodyExt;

    // An unrelated event at revision 2 first, purely so `watch_from`'s
    // own "not older than the oldest retained history entry" check
    // has something at or before the requested start_revision (same
    // pre-existing `watch_from` quirk `cacher::store`'s own tests hit
    // — untouched by, and unrelated to, what this test is proving).
    // The event actually under test needs a real encoded envelope —
    // `to_watch_event_json` decodes it for real, same as
    // `server::watch_event`'s own tests do.
    let schema = crate::codec::protobuf::schema_for_gvk("", "v1", "Namespace").unwrap();
    let object_bytes = crate::codec::protobuf::encode_message(
        schema,
        &serde_json::json!({"metadata": {"name": "default"}}),
    )
    .unwrap();
    let envelope = crate::codec::protobuf::wrap_unknown("v1", "Namespace", &object_bytes);

    let cache = crate::cacher::store::WatchCache::new(vec![], 1, 16, 16);
    let shared = crate::cacher::store::SharedCache::new(cache);
    shared.apply(
        crate::cacher::store::EventKind::Added,
        b"seed".to_vec(),
        b"unrelated".to_vec(),
        2,
    );
    shared.apply(
        crate::cacher::store::EventKind::Added,
        b"a".to_vec(),
        envelope,
        3,
    );
    let (replay, rx) = shared.watch_from(2).unwrap();
    assert_eq!(
        replay.len(),
        1,
        "only the revision-3 event should be in the replay"
    );
    // Drop the cache (and its own broadcast::Sender) before consuming
    // the stream to completion below — otherwise the live half of
    // `watch_response_body` never ends (a real watch stream is
    // meant to run forever; only exercised for the replay half here,
    // the live half is real end-to-end behavior, not something a
    // `.collect()`-to-completion unit test can observe without
    // artificially closing the channel first).
    drop(shared);

    let body = watch_response_body(
        replay,
        rx,
        "Namespace".to_string(),
        "v1".to_string(),
        Vec::new(),
        Vec::new(),
        None,
        String::new(),
        "namespaces".to_string(),
        "v1".to_string(),
        false,
        true,
        None,
        None,
    );
    let collected = body.collect().await.unwrap().to_bytes();
    let text = String::from_utf8(collected.to_vec()).unwrap();
    assert_eq!(text.lines().count(), 1);
    let parsed: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(parsed["type"], "ADDED");
}

#[tokio::test]
async fn watch_response_body_honors_bookmark_negotiation_and_timeout() {
    use http_body_util::BodyExt;

    let bookmark = crate::cacher::store::WatchEvent {
        kind: crate::cacher::store::EventKind::Bookmark,
        key: Vec::new(),
        value: Vec::new(),
        revision: 9,
    };
    let (_, rx) = {
        let cache = crate::cacher::store::WatchCache::new(vec![], 0, 16, 16);
        cache.watch_from(0).unwrap()
    };
    let body = watch_response_body(
        vec![bookmark.clone()],
        rx,
        "Namespace".to_string(),
        "v1".to_string(),
        Vec::new(),
        Vec::new(),
        None,
        String::new(),
        "namespaces".to_string(),
        "v1".to_string(),
        false,
        false,
        None,
        None,
    );
    let bytes = body.collect().await.unwrap().to_bytes();
    assert!(bytes.is_empty(), "bookmarks must be opt-in");

    let (_, rx) = {
        let cache = crate::cacher::store::WatchCache::new(vec![], 0, 16, 16);
        cache.watch_from(0).unwrap()
    };
    let body = watch_response_body(
        Vec::new(),
        rx,
        "Namespace".to_string(),
        "v1".to_string(),
        Vec::new(),
        Vec::new(),
        None,
        String::new(),
        "namespaces".to_string(),
        "v1".to_string(),
        false,
        false,
        Some(std::time::Duration::from_millis(10)),
        None,
    );
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(1), body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    assert!(
        bytes.is_empty(),
        "an idle watch must terminate at timeoutSeconds"
    );
}

#[tokio::test]
async fn watch_response_body_sends_streaming_list_initial_events_end_bookmark() {
    use http_body_util::BodyExt;

    let initial = crate::cacher::store::WatchEvent {
        kind: crate::cacher::store::EventKind::Added,
        key: b"/registry/namespaces/default".to_vec(),
        value: envelope_for("default", serde_json::json!({})),
        revision: 5,
    };
    let cache = crate::cacher::store::WatchCache::new(vec![], 5, 16, 16);
    let (_, rx) = cache.watch_from(5).unwrap();
    drop(cache);

    let body = watch_response_body_with_initial_events(
        Vec::new(),
        rx,
        "Namespace".to_string(),
        "v1".to_string(),
        Vec::new(),
        Vec::new(),
        None,
        String::new(),
        "namespaces".to_string(),
        "v1".to_string(),
        false,
        true,
        None,
        None,
        Some((vec![initial], 5)),
    );
    let bytes = body.collect().await.unwrap().to_bytes();
    let lines: Vec<serde_json::Value> = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["type"], "ADDED");
    assert_eq!(lines[1]["type"], "BOOKMARK");
    assert_eq!(lines[1]["object"]["metadata"]["resourceVersion"], "5");
    assert_eq!(
        lines[1]["object"]["metadata"]["annotations"]["k8s.io/initial-events-end"],
        "true"
    );
}

#[tokio::test]
async fn expired_watch_is_an_in_band_error_event_in_an_http_success_response() {
    use http_body_util::BodyExt;

    let response = watch_resource_expired_response("/api/v1/watch/namespaces");
    assert_eq!(response.status(), hyper::StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );

    let (_, body) = response.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    let event: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(event["type"], "ERROR");
    assert_eq!(event["object"]["reason"], "Gone");
    assert_eq!(event["object"]["code"], 410);
}

fn watch_event(revision: i64) -> crate::cacher::store::WatchEvent {
    crate::cacher::store::WatchEvent {
        kind: crate::cacher::store::EventKind::Added,
        key: format!("/registry/test/{revision}").into_bytes(),
        value: vec![],
        revision,
    }
}

/// A live watch stream shaped exactly like `watch_response_body` builds
/// one: a `broadcast` receiver kept open by a sender held by the test.
fn live_stream(
    rx: tokio::sync::broadcast::Receiver<crate::cacher::store::WatchEvent>,
) -> WatchEventStream {
    use tokio_stream::StreamExt as _;
    Box::pin(
        tokio_stream::wrappers::BroadcastStream::new(rx)
            .map_while(|res| res.ok())
            .map(|event| (event, false)),
    )
}

#[tokio::test]
async fn cap_idle_silence_ends_a_live_stream_that_never_yields() {
    use tokio_stream::StreamExt as _;

    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let started = std::time::Instant::now();
    let mut capped = cap_idle_silence(live_stream(rx), std::time::Duration::from_millis(100));
    // The sender stays alive (the cache it stands for is up), yet nothing
    // is ever broadcast — exactly the observed stall. The cap must end the
    // stream once the silence outlasts the limit rather than waiting for
    // the client's own timeout.
    let ended = tokio::time::timeout(std::time::Duration::from_secs(1), capped.next())
        .await
        .expect("a silent live stream must be ended by the idle cap");
    assert!(ended.is_none(), "the capped stream must end, not yield");
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(90),
        "the cap must wait out its limit, not end immediately"
    );
    drop(tx);
}

#[tokio::test]
async fn cap_idle_silence_does_not_end_a_stream_that_keeps_yielding() {
    use tokio_stream::StreamExt as _;

    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let mut capped = cap_idle_silence(live_stream(rx), std::time::Duration::from_millis(150));
    let producer = tokio::spawn(async move {
        for revision in 0..10 {
            let _ = tx.send(watch_event(revision));
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    });
    // Ten items arrive 20ms apart — 180ms of wall time, past the 150ms
    // limit. A fixed-lifetime cap would end at ~150ms having collected
    // only seven or eight; collecting all ten proves the idle timer resets
    // on every item. The 20ms gaps sit far below the 150ms limit, so a
    // briefly loaded host cannot make a healthy stream look idle.
    let collected = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        capped.take(10).collect::<Vec<_>>(),
    )
    .await
    .expect("a producing live stream must keep yielding past the idle limit");
    assert_eq!(
        collected.len(),
        10,
        "the idle cap must not fire while items keep flowing"
    );
    producer.await.unwrap();
}

#[tokio::test]
async fn cap_idle_silence_resets_its_deadline_on_every_item() {
    use tokio_stream::StreamExt as _;

    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let mut capped = cap_idle_silence(live_stream(rx), std::time::Duration::from_millis(100));
    // Let the cap age past half its limit with the sender alive and silent,
    // then deliver one item. A fixed-lifetime cap would then end only
    // ~40ms after that item (100ms from creation); a resettable idle timer
    // must wait a fresh full limit from it.
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    tx.send(watch_event(1)).unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(1), capped.next())
        .await
        .expect("the delivered item must reach the stream");
    assert!(first.is_some(), "the delivered item must reach the stream");
    let after_item = std::time::Instant::now();
    let ended = tokio::time::timeout(std::time::Duration::from_secs(1), capped.next())
        .await
        .expect("the idle cap must close the stream");
    assert!(
        ended.is_none(),
        "silence after the last item must end the stream"
    );
    assert!(
        after_item.elapsed() >= std::time::Duration::from_millis(90),
        "the deadline must reset from the last item, not run from stream creation"
    );
    drop(tx);
}
