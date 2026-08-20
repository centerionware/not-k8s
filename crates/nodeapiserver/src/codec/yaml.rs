//! `application/yaml` — accepted by `kubectl apply -f`/`create -f` and a
//! few other clients, though never the response format the apiserver
//! defaults to. Round-trips through `serde_json::Value` rather than a
//! separate YAML-shaped type, since JSON and YAML agree on the same data
//! model (maps/sequences/scalars) and every other layer in this crate
//! already speaks `Value`.

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

pub fn encode(value: &Value) -> Result<Vec<u8>, Error> {
    Ok(serde_yaml::to_string(value)?.into_bytes())
}

pub fn decode(bytes: &[u8]) -> Result<Value, Error> {
    Ok(serde_yaml::from_slice(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_an_object() {
        let value = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "x"}});
        let encoded = encode(&value).unwrap();
        assert_eq!(decode(&encoded).unwrap(), value);
    }

    #[test]
    fn accepts_hand_written_yaml() {
        let yaml = b"apiVersion: v1\nkind: Pod\nmetadata:\n  name: my-pod\n";
        let decoded = decode(yaml).unwrap();
        assert_eq!(decoded.get("kind").unwrap(), "Pod");
        assert_eq!(decoded.get("metadata").unwrap().get("name").unwrap(), "my-pod");
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        assert!(decode(b": : :\n\tbad").is_err());
    }
}
