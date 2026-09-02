    match decoded {
        Ok(object) => crate::cacher::selector::object_matches(&object, label_reqs, field_reqs),
        Err(e) => {
            warn!(error = ?e, "watch: failed to decode a cached value for selector filtering; letting the event through unfiltered");
            true
        }
    }
