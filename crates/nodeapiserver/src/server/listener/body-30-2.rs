    match crate::server::watch_event::to_watch_event_json_with_conversion(
        event,
        kind,
        api_version,
        storage.as_mut(),
        group,
        resource,
        conversion_webhook.as_ref(),
    )
    .await
    {
        None => None,
        Some(Ok(mut event_json)) => {
            if partial_metadata {
                if let Some(object) = event_json.get_mut("object") {
                    *object = crate::codec::partial_metadata::object(object);
                }
            }
            let mut bytes = serde_json::to_vec(&event_json).unwrap_or_default();
            bytes.push(b'\n');
            metrics::record_watch_event(group, version, resource);
            Some(Ok(hyper::body::Frame::data(hyper::body::Bytes::from(bytes))))
        }
        Some(Err(error)) => Some(Err(Box::new(error) as BoxError)),
    }
