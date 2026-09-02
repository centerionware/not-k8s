    use base64::Engine;
    let buf = base64::engine::general_purpose::STANDARD.decode(token).ok()?;
    let separator = buf.iter().rposition(|&b| b == 0)?;
    let (key, rest) = buf.split_at(separator);
    let revision = std::str::from_utf8(&rest[1..]).ok()?.parse::<i64>().ok()?;
