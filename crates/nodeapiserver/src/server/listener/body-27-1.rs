    let detail = conflicts.iter().map(|c| format!("\"{}\" already owns: {}", c.manager, c.fields.to_json())).collect::<Vec<_>>().join("; ");
