    let start = if info.verb == "proxy" {
        2
    } else {
        info.parts.iter().position(|part| part == "proxy").map_or(info.parts.len(), |index| index + 1)
    };
    let suffix = info.parts.get(start..).map(|parts| parts.join("/")).unwrap_or_default();
