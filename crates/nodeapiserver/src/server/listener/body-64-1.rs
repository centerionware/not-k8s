    let info = path::parse(method, path_str, query);
    let (user_name, user_uid, user_groups): (&str, Option<&str>, Vec<String>) = match identity {
        Some(id) => (id.name.as_str(), id.uid.as_deref(), id.groups.clone()),
        None => (ANONYMOUS_USERNAME, None, vec![UNAUTHENTICATED_GROUP.to_string()]),
    };
    let object_ref = info.is_resource_request.then(|| crate::audit::event::ObjectRef { group: &info.api_group, resource: &info.resource, namespace: &info.namespace, name: &info.name, api_version: &info.api_version });
    let request_uri = if query.is_empty() { path_str.to_string() } else { format!("{path_str}?{query}") };
    let audit_id = uuid::Uuid::new_v4().to_string();
    let timestamp = chrono::Utc::now().to_rfc3339();
    let source_ip = peer.ip().to_string();
