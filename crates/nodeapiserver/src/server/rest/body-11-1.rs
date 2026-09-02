    let object = if let Some(conversion_webhook) = conversion_webhook {
        let source_version = object
            .get("apiVersion")
            .and_then(Value::as_str)
            .map(|api_version| api_version.rsplit_once('/').map_or(api_version, |(_, version)| version));
        if source_version != Some(version) {
            let mut objects = apiextensions::conversion::convert(storage, group, conversion_webhook, version, vec![object]).await.map_err(|error| Error::InvalidProtobufRequest(error.to_string()))?;
            objects.pop().ok_or_else(|| Error::InvalidProtobufRequest("conversion webhook returned no object".to_string()))?
        } else {
            object
        }
    } else {
        object
    };
