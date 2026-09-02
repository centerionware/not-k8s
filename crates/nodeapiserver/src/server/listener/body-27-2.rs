    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{path_str}: conflict with existing field manager(s): {detail}"),
        "reason": "Conflict",
        "details": {},
        "code": 409,
    })
