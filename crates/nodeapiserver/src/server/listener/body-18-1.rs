    let params = path::parse_query(query);
    let allow_watch_bookmarks = match params
        .iter()
        .find(|(key, _)| key == "allowWatchBookmarks")
    {
        None => false,
        Some((_, value)) => match value.to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return Err("allowWatchBookmarks must be true or false"),
        },
    };
    let timeout = match params.iter().find(|(key, _)| key == "timeoutSeconds") {
        None => None,
        Some((_, value)) => {
            let seconds = value
                .parse::<u64>()
                .map_err(|_| "timeoutSeconds must be a non-negative integer")?;
            if seconds == 0 {
                None
            } else {
                Some(std::time::Duration::from_secs(seconds))
            }
        }
    };
