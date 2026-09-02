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
