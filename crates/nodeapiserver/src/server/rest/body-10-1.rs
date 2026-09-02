    let Some(conversion_webhook) = conversion_webhook else {
        return Ok(object);
    };
    if conversion_webhook.storage_version == version {
        return Ok(object);
    }
    let mut objects = apiextensions::conversion::convert(storage, group, conversion_webhook, &conversion_webhook.storage_version, vec![object]).await.map_err(|error| Error::InvalidProtobufRequest(error.to_string()))?;
