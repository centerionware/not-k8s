    let Some((_, value)) = path::parse_query(query).into_iter().find(|(key, _)| key == "dryRun") else {
        return Ok(false);
    };
