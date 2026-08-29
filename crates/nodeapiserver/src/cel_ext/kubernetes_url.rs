//! Kubernetes' `kubernetes.urls` CEL extension library.
//!
//! URL values retain the parsed `url::Url` so the getters use the same
//! escaped-path, hostname, port, and query-pair semantics as upstream.

use cel::extractors::This;
use cel::objects::{Map, Opaque};
use cel::{ExecutionError, FunctionContext, Value};
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

const URL_TYPE: &str = "kubernetes.URL";

#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlValue(Url);

impl Opaque for UrlValue {
    fn runtime_type_name(&self) -> &str {
        URL_TYPE
    }
}

fn opaque(url: Url) -> Value {
    Value::Opaque(Arc::new(UrlValue(url)))
}

fn url_ref(value: &Value) -> Option<&Url> {
    match value {
        Value::Opaque(value) => value.downcast_ref::<UrlValue>().map(|value| &value.0),
        _ => None,
    }
}

fn invalid_receiver(ftx: &FunctionContext, operation: &str) -> ExecutionError {
    ftx.error(format!("{operation}() requires a Kubernetes URL"))
}

fn parse_url(raw: &str) -> Result<Url, String> {
    Url::parse(raw)
        .map_err(|error| format!("URL parse error during conversion from string: {error}"))
}

pub fn url_binding(ftx: &FunctionContext, raw: Arc<String>) -> Result<Value, ExecutionError> {
    parse_url(&raw)
        .map(opaque)
        .map_err(|error| ftx.error(error))
}

pub fn is_url_binding(raw: Arc<String>) -> bool {
    parse_url(&raw).is_ok()
}

fn url_from_value<'a>(
    ftx: &FunctionContext,
    value: &'a Value,
    operation: &str,
) -> Result<&'a Url, ExecutionError> {
    url_ref(value).ok_or_else(|| invalid_receiver(ftx, operation))
}

pub fn get_scheme_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<Value, ExecutionError> {
    Ok(Value::String(Arc::new(
        url_from_value(ftx, &value, "getScheme")?
            .scheme()
            .to_string(),
    )))
}

pub fn get_host_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<Value, ExecutionError> {
    let url = url_from_value(ftx, &value, "getHost")?;
    let Some(host) = url.host_str() else {
        return Ok(Value::String(Arc::new(String::new())));
    };
    let host = match url.port() {
        Some(port) if host.contains(':') => format!("[{host}]:{port}"),
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    Ok(Value::String(Arc::new(host)))
}

pub fn get_hostname_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<Value, ExecutionError> {
    Ok(Value::String(Arc::new(
        url_from_value(ftx, &value, "getHostname")?
            .host_str()
            .unwrap_or_default()
            .to_string(),
    )))
}

pub fn get_port_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<Value, ExecutionError> {
    Ok(Value::String(Arc::new(
        url_from_value(ftx, &value, "getPort")?
            .port()
            .map(|port| port.to_string())
            .unwrap_or_default(),
    )))
}

pub fn get_escaped_path_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<Value, ExecutionError> {
    Ok(Value::String(Arc::new(
        url_from_value(ftx, &value, "getEscapedPath")?
            .path()
            .to_string(),
    )))
}

pub fn get_query_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<Value, ExecutionError> {
    let url = url_from_value(ftx, &value, "getQuery")?;
    let mut query: HashMap<String, Vec<Value>> = HashMap::new();
    for (key, value) in url.query_pairs() {
        query
            .entry(key.into_owned())
            .or_default()
            .push(Value::String(Arc::new(value.into_owned())));
    }
    let query = query
        .into_iter()
        .map(|(key, values)| (key, Value::List(Arc::new(values))))
        .collect::<HashMap<_, _>>();
    Ok(Value::Map(Map::from(query)))
}

pub(crate) fn string_value(value: &Value) -> Option<String> {
    url_ref(value).map(Url::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_requires_an_absolute_url() {
        assert!(parse_url("https://example.com/path").is_ok());
        assert!(parse_url("/relative/path").is_err());
    }

    #[test]
    fn getters_preserve_the_url_components_needed_by_cel() {
        let url = parse_url("https://example.com:8443/path%20with%20spaces?k=a&k=b").unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("example.com"));
        assert_eq!(url.port(), Some(8443));
        assert_eq!(url.path(), "/path%20with%20spaces");
        assert_eq!(url.query_pairs().count(), 2);
    }
}
