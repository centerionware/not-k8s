    crate::audit::event::build_event(&crate::audit::event::EventInput {
        audit_id: &audit_id,
        request_uri: &request_uri,
        verb: &info.verb,
        user_name,
        user_uid,
        user_groups: user_groups.as_slice(),
        source_ip: Some(&source_ip),
        user_agent,
        object_ref,
        response_code: status,
        annotations: (!annotations.is_empty()).then_some(annotations),
        timestamp: &timestamp,
    })
