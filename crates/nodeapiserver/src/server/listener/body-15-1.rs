    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
