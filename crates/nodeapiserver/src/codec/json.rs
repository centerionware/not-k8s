//! `application/json` — the default wire format. A thin, named wrapper
//! around `serde_json` rather than callers reaching for it directly, so
//! `codec::negotiation`'s dispatch has one function per format to call
//! regardless of which crate actually implements it.

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
#[error("JSON: {0}")]
pub struct Error(#[from] serde_json::Error);

pub fn encode(value: &Value) -> Result<Vec<u8>, Error> {
    Ok(serde_json::to_vec(value)?)
}

pub fn decode(bytes: &[u8]) -> Result<Value, Error> {
    Ok(serde_json::from_slice(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_an_object() {
        let value = json!({"apiVersion": "v1", "kind": "Pod"});
        let encoded = encode(&value).unwrap();
        assert_eq!(decode(&encoded).unwrap(), value);
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        assert!(decode(b"{not json").is_err());
    }
}
