    use http_body_util::{BodyExt, StreamBody};
    use tokio_stream::wrappers::BroadcastStream;
    use tokio_stream::StreamExt;

    let replay_stream = tokio_stream::iter(replay);
    let live_stream = BroadcastStream::new(rx).map_while(|res| res.ok());
    let events = replay_stream
        .chain(live_stream)
        .filter(move |event| allow_watch_bookmarks || event.kind != crate::cacher::store::EventKind::Bookmark);
    let events: WatchEventStream = if let Some(timeout) = timeout {
        Box::pin(futures::StreamExt::take_until(
            events,
            tokio::time::sleep(timeout),
        ))
    } else {
        Box::pin(events)
    };
    // Cloned once per closure (`StorageClient` wraps a cheap-to-clone
    // `tonic::transport::Channel`, same posture every other real call
    // site in this crate already takes) — `filter`/`filter_map` each need
    // their own `'static`-owned copy of the encryption-lookup context.
    let (storage_for_filter, group_for_filter, resource_for_filter) = (storage.clone(), group.clone(), resource.clone());
    let filtered = events.filter(move |event| watch_event_matches_selector(event, &label_reqs, &field_reqs, storage_for_filter.as_ref(), &group_for_filter, &resource_for_filter));
    if conversion_webhook.is_none() {
        let frames = filtered.filter_map(move |event| {
            encode_watch_event(&event, &kind, &api_version, storage.as_ref(), &group, &resource, &version, partial_metadata)
        });
        return StreamBody::new(frames).boxed();
    }

    let events: WatchEventStream = Box::pin(filtered);
    let stream = ConversionWatchStream {
        state: Arc::new(Mutex::new(ConversionWatchState {
            events,
            pending: None,
            kind,
            api_version,
            storage,
            group,
            resource,
            version,
            partial_metadata,
            conversion_webhook,
        })),
    };
