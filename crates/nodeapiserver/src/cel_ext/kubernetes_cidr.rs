//! Kubernetes' `kubernetes.net.cidr` CEL extension library.
//!
//! CIDRs are represented as opaque `net.CIDR` values. Parsing intentionally
//! keeps host bits in the original address, matching upstream's `netip.Prefix`
//! behavior; containment and `masked()` use the network address.

use super::kubernetes_ip;
use cel::extractors::This;
use cel::objects::Opaque;
use cel::{ExecutionError, FunctionContext, Value};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

const CIDR_TYPE: &str = "net.CIDR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CidrValue {
    address: IpAddr,
    prefix_length: u8,
}

impl Opaque for CidrValue {
    fn runtime_type_name(&self) -> &str {
        CIDR_TYPE
    }
}

fn opaque(cidr: CidrValue) -> Value {
    Value::Opaque(Arc::new(cidr))
}

fn cidr_ref(value: &Value) -> Option<CidrValue> {
    match value {
        Value::Opaque(value) => value.downcast_ref::<CidrValue>().copied(),
        _ => None,
    }
}

fn invalid_receiver(ftx: &FunctionContext, operation: &str) -> ExecutionError {
    ftx.error(format!("{operation}() requires a Kubernetes CIDR"))
}

fn parse_cidr(raw: &str) -> Result<CidrValue, String> {
    let (address, prefix) = raw.rsplit_once('/').ok_or_else(|| {
        format!("network address parse error during conversion from string: {raw:?}")
    })?;
    let address = kubernetes_ip::parse_address(address).map_err(|error| {
        format!("network address parse error during conversion from string: {error}")
    })?;
    let prefix_length = prefix.parse::<u8>().map_err(|error| {
        format!("network address parse error during conversion from string: {error}")
    })?;
    let max_prefix_length = match address {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    if prefix_length > max_prefix_length {
        return Err(format!(
            "network address parse error during conversion from string: prefix length {prefix_length} is invalid for this address"
        ));
    }
    Ok(CidrValue {
        address,
        prefix_length,
    })
}

fn masked_address(address: IpAddr, prefix_length: u8) -> IpAddr {
    match address {
        IpAddr::V4(address) => {
            let value = u32::from(address);
            let mask = if prefix_length == 0 {
                0
            } else {
                u32::MAX << (32 - prefix_length)
            };
            IpAddr::V4(Ipv4Addr::from(value & mask))
        }
        IpAddr::V6(address) => {
            let value = u128::from(address);
            let mask = if prefix_length == 0 {
                0
            } else {
                u128::MAX << (128 - prefix_length)
            };
            IpAddr::V6(Ipv6Addr::from(value & mask))
        }
    }
}

fn contains_address(cidr: CidrValue, address: IpAddr) -> bool {
    match (cidr.address, address) {
        (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_)) => {
            masked_address(cidr.address, cidr.prefix_length)
                == masked_address(address, cidr.prefix_length)
        }
        _ => false,
    }
}

pub(crate) fn string_value(value: &Value) -> Option<String> {
    cidr_ref(value).map(|cidr| format!("{}/{}", cidr.address, cidr.prefix_length))
}

pub fn cidr_binding(ftx: &FunctionContext, raw: Arc<String>) -> Result<Value, ExecutionError> {
    parse_cidr(&raw)
        .map(opaque)
        .map_err(|error| ftx.error(error))
}

pub fn is_cidr_binding(raw: Arc<String>) -> bool {
    parse_cidr(&raw).is_ok()
}

pub fn contains_ip_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
    other: Value,
) -> Result<bool, ExecutionError> {
    let cidr = cidr_ref(&value).ok_or_else(|| invalid_receiver(ftx, "containsIP"))?;
    let address = match other {
        Value::String(raw) => {
            kubernetes_ip::parse_address(&raw).map_err(|error| ftx.error(error))?
        }
        other => kubernetes_ip::address_from_value(&other).ok_or_else(|| {
            ftx.error(format!(
                "containsIP() requires an IP or string, got {other:?}"
            ))
        })?,
    };
    Ok(contains_address(cidr, address))
}

pub fn contains_cidr_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
    other: Value,
) -> Result<bool, ExecutionError> {
    let cidr = cidr_ref(&value).ok_or_else(|| invalid_receiver(ftx, "containsCIDR"))?;
    let other = match other {
        Value::String(raw) => parse_cidr(&raw).map_err(|error| ftx.error(error))?,
        other => cidr_ref(&other).ok_or_else(|| {
            ftx.error(format!(
                "containsCIDR() requires a CIDR or string, got {other:?}"
            ))
        })?,
    };
    Ok(cidr.prefix_length <= other.prefix_length && contains_address(cidr, other.address))
}

pub fn ip_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<Value, ExecutionError> {
    if let Some(cidr) = cidr_ref(&value) {
        return Ok(kubernetes_ip::opaque_address(cidr.address));
    }
    kubernetes_ip::from_value(ftx, value)
}

pub fn prefix_length_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<i64, ExecutionError> {
    cidr_ref(&value)
        .map(|cidr| i64::from(cidr.prefix_length))
        .ok_or_else(|| invalid_receiver(ftx, "prefixLength"))
}

pub fn masked_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<Value, ExecutionError> {
    let cidr = cidr_ref(&value).ok_or_else(|| invalid_receiver(ftx, "masked"))?;
    Ok(opaque(CidrValue {
        address: masked_address(cidr.address, cidr.prefix_length),
        ..cidr
    }))
}

pub fn string_binding(
    ftx: &FunctionContext,
    This(value): This<Value>,
) -> Result<Value, ExecutionError> {
    if let Some(cidr) = cidr_ref(&value) {
        return Ok(Value::String(Arc::new(format!(
            "{}/{}",
            cidr.address, cidr.prefix_length
        ))));
    }
    kubernetes_ip::string_binding(ftx, This(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_preserves_host_bits_until_masked() {
        let cidr = parse_cidr("192.168.0.1/24").unwrap();
        assert_eq!(cidr.address, "192.168.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(
            masked_address(cidr.address, cidr.prefix_length),
            "192.168.0.0".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn containment_requires_the_same_address_family() {
        let cidr = parse_cidr("192.168.0.0/24").unwrap();
        assert!(contains_address(
            cidr,
            "192.168.0.10".parse::<IpAddr>().unwrap()
        ));
        assert!(!contains_address(
            cidr,
            "2001:db8::1".parse::<IpAddr>().unwrap()
        ));
    }
}
