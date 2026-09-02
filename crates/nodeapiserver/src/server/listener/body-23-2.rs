    match value.as_str() {
        "All" => Ok(true),
        _ => Err("dryRun must be All"),
    }
