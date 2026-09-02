    objects.pop().ok_or_else(|| Error::InvalidProtobufRequest("conversion webhook returned no object".to_string()))
