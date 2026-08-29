//! Kubernetes' `kubernetes.net.ip` CEL extension library.
//!
//! This is the runtime portion of upstream's IP library: it parses strict
//! IPv4/IPv6 strings into an opaque `net.IP` value and exposes the same
//! classification helpers (`family`, `isUnspecified`, `isLoopback`,
//! `isLinkLocalMulticast`, `isLinkLocalUnicast`, `isGlobalUnicast`) plus
//! `ip.isCanonical`, `isIP`, and `string`.

use cel::extractors::This;
use cel::objects::Opaque;
use cel::{ExecutionError, FunctionContext, Value};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

const IP_TYPE: &str = "net.IP";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IpValue(IpAddr);

impl Opaque for IpValue {
    fn runtime_type_name(&self) -> &str {
        IP_TYPE
    }
}

fn opaque(address: IpAddr) -> Value {
    Value::Opaque(Arc::new(IpValue(address)))
}

fn address_ref(value: &Value) -> Option<IpAddr> {
    match value {
        Value::Opaque(value) => value.downcast_ref::<IpValue>().map(|value| value.0),
        _ => None,
    }
}

fn invalid_receiver(ftx: &FunctionContext, operation: &str) -> ExecutionError {
    ftx.error(format!("{operation}() requires a Kubernetes IP"))
}

fn parse_address(raw: &str) -> Result<IpAddr, String> {
    let address = raw.parse::<IpAddr>().map_err(|error| {
        format!("IP Address {raw:?} parse error during conversion from string: {error}")
    })?;
    if let IpAddr::V6(address) = address {
        let segments = address.segments();
        if segments[..5] == [0, 0, 0, 0, 0] && segments[5] == 0xffff {
            return Err(format!("IPv4-mapped IPv6 address {raw:?} is not allowed"));
        }
    }
    Ok(address)
}

pub fn ip_binding(ftx: &FunctionContext, raw: Arc<String>) -> Result<Value, ExecutionError> {
    parse_address(&raw)
        .map(opaque)
        .map_err(|error| ftx.error(error))
}

pub fn is_ip_binding(raw: Arc<String>) -> bool {
    parse_address(&raw).is_ok()
}

pub fn is_canonical_binding(
    ftx: &FunctionContext,
    raw: Arc<String>,
) -> Result<bool, ExecutionError> {
    Ok(parse_address(&raw)
        .map(|address| address.to_string() == *raw)
        .map_err(|error| ftx.error(error))?)
}

pub fn family_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<i64, ExecutionError> {
    match address_ref(&value) {
        Some(IpAddr::V4(_)) => Ok(4),
        Some(IpAddr::V6(_)) => Ok(6),
        None => Err(invalid_receiver(ftx, "family")),
    }
}

pub fn is_unspecified_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<bool, ExecutionError> {
    address_ref(&value)
        .map(|address| address.is_unspecified())
        .ok_or_else(|| invalid_receiver(ftx, "isUnspecified"))
}

pub fn is_loopback_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<bool, ExecutionError> {
    address_ref(&value)
        .map(|address| address.is_loopback())
        .ok_or_else(|| invalid_receiver(ftx, "isLoopback"))
}

pub fn is_link_local_multicast_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<bool, ExecutionError> {
    address_ref(&value)
        .map(is_link_local_multicast)
        .ok_or_else(|| invalid_receiver(ftx, "isLinkLocalMulticast"))
}

pub fn is_link_local_unicast_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<bool, ExecutionError> {
    address_ref(&value)
        .map(is_link_local_unicast)
        .ok_or_else(|| invalid_receiver(ftx, "isLinkLocalUnicast"))
}

pub fn is_global_unicast_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<bool, ExecutionError> {
    address_ref(&value)
        .map(is_global_unicast)
        .ok_or_else(|| invalid_receiver(ftx, "isGlobalUnicast"))
}

fn is_link_local_multicast(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            octets[0] == 224 && octets[1] == 0 && octets[2] == 0
        }
        IpAddr::V6(address) => {
            let first = address.segments()[0];
            first & 0xff00 == 0xff00 && first & 0x000f == 0x0002
        }
    }
}

fn is_link_local_unicast(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_link_local(),
        IpAddr::V6(address) => {
            let first = address.segments()[0];
            first & 0xffc0 == 0xfe80
        }
    }
}

fn is_global_unicast(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address != Ipv4Addr::UNSPECIFIED && address != Ipv4Addr::BROADCAST,
        IpAddr::V6(address) => {
            !address.is_unspecified()
                && !address.is_loopback()
                && !address.is_multicast()
                && !is_link_local_unicast(IpAddr::V6(address))
        }
    }
}

pub fn string_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<Value, ExecutionError> {
    match value {
        Value::Opaque(_) => address_ref(&value)
            .map(|address| Value::String(Arc::new(address.to_string())))
            .ok_or_else(|| invalid_receiver(ftx, "string")),
        Value::String(value) => Ok(Value::String(value)),
        Value::Int(value) => Ok(Value::String(Arc::new(value.to_string()))),
        Value::UInt(value) => Ok(Value::String(Arc::new(value.to_string()))),
        Value::Float(value) => Ok(Value::String(Arc::new(value.to_string()))),
        Value::Bytes(value) => Ok(Value::String(Arc::new(
            String::from_utf8_lossy(value.as_slice()).into_owned(),
        ))),
        other => Err(ftx.error(format!("cannot convert {other:?} to string"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_parser_accepts_both_address_families() {
        assert_eq!(parse_address("127.0.0.1"), Ok("127.0.0.1".parse().unwrap()));
        assert_eq!(
            parse_address("2001:db8::abcd"),
            Ok("2001:db8::abcd".parse().unwrap())
        );
    }

    #[test]
    fn strict_parser_rejects_zones_and_mapped_addresses() {
        assert!(parse_address("fe80::1%eth0").is_err());
        assert!(parse_address("::ffff:192.0.2.1").is_err());
    }

    #[test]
    fn canonical_form_matches_the_address_display() {
        assert_eq!(
            parse_address("2001:db8::abcd").unwrap().to_string(),
            "2001:db8::abcd"
        );
        assert_ne!(
            parse_address("2001:DB8::ABCD").unwrap().to_string(),
            "2001:DB8::ABCD"
        );
    }

    #[test]
    fn address_classification_matches_upstream_examples() {
        assert!(is_link_local_multicast("224.0.0.1".parse().unwrap()));
        assert!(!is_link_local_multicast("224.0.1.1".parse().unwrap()));
        assert!(is_link_local_multicast("ff02::1".parse().unwrap()));
        assert!(is_link_local_unicast("169.254.169.254".parse().unwrap()));
        assert!(is_link_local_unicast("fe80::1".parse().unwrap()));
        assert!(is_global_unicast("192.168.0.1".parse().unwrap()));
        assert!(!is_global_unicast("255.255.255.255".parse().unwrap()));
        assert!(!is_global_unicast("ff00::1".parse().unwrap()));
    }
}
