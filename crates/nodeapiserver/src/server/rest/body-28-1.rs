    use base64::Engine;
    let mut buf = resume_key.to_vec();
    buf.push(0);
    buf.extend_from_slice(revision.to_string().as_bytes());
