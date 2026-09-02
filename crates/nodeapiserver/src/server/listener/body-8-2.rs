    for warning in warnings {
        // RFC 7234's warning-text is quoted; sanitize control characters so
        // a policy cannot inject a second header into the response.
        let escaped = warning.replace('\\', "\\\\").replace('"', "\\\"").replace('\r', " ").replace('\n', " ");
        let Ok(value) = hyper::header::HeaderValue::from_str(&format!("299 - \"{escaped}\"")) else {
            continue;
        };
        response.headers_mut().append(warning_header.clone(), value);
    }
