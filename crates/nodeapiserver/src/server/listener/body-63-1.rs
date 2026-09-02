    let event = build_audit_event(method, path_str, query, user_agent, identity, peer, status, annotations);
    if let Some(sink) = audit_sink {
        if let Err(error) = sink.write(&event) {
            warn!(error = ?error, "nodeapiserver: failed to write audit event");
        }
    }
