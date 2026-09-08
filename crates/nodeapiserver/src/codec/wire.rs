//! Raw protobuf wire primitives: varint and the four wire types this crate
//! actually needs. Deliberately small — see `docs/APISERVER_PLAN.md`
//! finding 6 for why this crate hand-rolls the wire format instead of
//! generating one with prost.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireType {
    Varint = 0,
    Fixed64 = 1,
    LengthDelimited = 2,
    Fixed32 = 5,
}

impl WireType {
    pub fn tag_bits(self) -> u64 {
        self as u64
    }
}

pub fn encode_tag(field_number: u32, wire_type: WireType, out: &mut Vec<u8>) {
    encode_varint(((field_number as u64) << 3) | wire_type.tag_bits(), out);
}

pub fn encode_varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            out.push(byte | 0x80);
        } else {
            out.push(byte);
            break;
        }
    }
}

pub fn encode_length_delimited(payload: &[u8], out: &mut Vec<u8>) {
    encode_varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

/// A proto2 `int32` field's negative values are sign-extended to 64 bits
/// before varint encoding — the well-known "10-byte negative int32" proto2
/// quirk. This replicates it exactly so the wire bytes match every other
/// real protobuf implementation (kubectl, client-go) byte for byte, not
/// just "an" encoding that happens to decode back correctly on its own.
pub fn encode_varint_i32(v: i32, out: &mut Vec<u8>) {
    encode_varint(v as i64 as u64, out);
}

pub fn encode_varint_i64(v: i64, out: &mut Vec<u8>) {
    encode_varint(v as u64, out);
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireError {
    #[error("unexpected end of input while decoding a varint")]
    VarintEof,
    #[error("varint is more than 64 bits")]
    VarintTooLong,
    #[error("unexpected end of input while decoding a length-delimited field")]
    LengthDelimitedEof,
    #[error("unexpected end of input while decoding a fixed64 field")]
    Fixed64Eof,
    #[error("unexpected end of input while decoding a fixed32 field")]
    Fixed32Eof,
    #[error("unknown wire type {0}")]
    UnknownWireType(u64),
}

pub fn decode_varint(bytes: &[u8], pos: &mut usize) -> Result<u64, WireError> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*pos).ok_or(WireError::VarintEof)?;
        *pos += 1;
        if shift >= 64 {
            return Err(WireError::VarintTooLong);
        }
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Ok(result)
}

pub fn decode_varint_i32(bytes: &[u8], pos: &mut usize) -> Result<i32, WireError> {
    // Low 32 bits are unchanged by the encoder's sign-extension trick
    // regardless of whether the original value was negative — see
    // encode_varint_i32's own doc comment.
    Ok(decode_varint(bytes, pos)? as u32 as i32)
}

pub fn decode_varint_i64(bytes: &[u8], pos: &mut usize) -> Result<i64, WireError> {
    Ok(decode_varint(bytes, pos)? as i64)
}

pub fn decode_length_delimited<'a>(bytes: &'a [u8], pos: &mut usize) -> Result<&'a [u8], WireError> {
    let len = decode_varint(bytes, pos)? as usize;
    let end = pos.checked_add(len).ok_or(WireError::LengthDelimitedEof)?;
    let slice = bytes.get(*pos..end).ok_or(WireError::LengthDelimitedEof)?;
    *pos = end;
    Ok(slice)
}

pub fn decode_fixed64(bytes: &[u8], pos: &mut usize) -> Result<f64, WireError> {
    let end = pos.checked_add(8).ok_or(WireError::Fixed64Eof)?;
    let slice = bytes.get(*pos..end).ok_or(WireError::Fixed64Eof)?;
    *pos = end;
    Ok(f64::from_le_bytes(slice.try_into().expect("8 bytes")))
}

pub fn encode_fixed64(v: f64, out: &mut Vec<u8>) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// One `(field_number, wire_type, payload_start..payload_end-relative-info)`
/// as read straight off the wire, before any field-table-driven
/// interpretation. `Varint`/`Fixed64` carry their decoded numeric value in
/// `raw`; `LengthDelimited` carries the raw payload slice.
pub enum RawField<'a> {
    Varint(u64),
    Fixed64(f64),
    LengthDelimited(&'a [u8]),
}

pub fn wire_type_from_tag(tag: u64) -> Result<WireType, WireError> {
    match tag & 0x7 {
        0 => Ok(WireType::Varint),
        1 => Ok(WireType::Fixed64),
        2 => Ok(WireType::LengthDelimited),
        5 => Ok(WireType::Fixed32),
        other => Err(WireError::UnknownWireType(other)),
    }
}

/// Reads one `(field_number, RawField)` starting at `*pos`, advancing it
/// past the field. `Fixed32` is decoded (as an `f32` widened to `f64`) but
/// no k8s API field this crate has seen actually uses it — confirmed empty
/// by grepping every vendored `.proto` for `float`/`fixed32`/`sfixed32`,
/// and `codec::protobuf`'s own test asserts that emptiness against the
/// live generated field table, not just this comment's say-so. Handling it
/// here anyway means an unexpected one is still consumed correctly rather
/// than desyncing the rest of the message.
pub fn decode_field<'a>(bytes: &'a [u8], pos: &mut usize) -> Result<(u32, RawField<'a>), WireError> {
    let tag = decode_varint(bytes, pos)?;
    let field_number = (tag >> 3) as u32;
    let wire_type = wire_type_from_tag(tag)?;
    let field = match wire_type {
        WireType::Varint => RawField::Varint(decode_varint(bytes, pos)?),
        WireType::Fixed64 => RawField::Fixed64(decode_fixed64(bytes, pos)?),
        WireType::LengthDelimited => RawField::LengthDelimited(decode_length_delimited(bytes, pos)?),
        WireType::Fixed32 => {
            let end = pos.checked_add(4).ok_or(WireError::Fixed32Eof)?;
            let slice = bytes.get(*pos..end).ok_or(WireError::Fixed32Eof)?;
            *pos = end;
            RawField::Fixed64(f32::from_le_bytes(slice.try_into().expect("4 bytes")) as f64)
        }
    };
    Ok((field_number, field))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trips_small_and_large_values() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            encode_varint(v, &mut buf);
            let mut pos = 0;
            assert_eq!(decode_varint(&buf, &mut pos).unwrap(), v);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn negative_int32_round_trips_through_the_sign_extension_quirk() {
        for v in [-1i32, -2, i32::MIN, 0, 1, i32::MAX] {
            let mut buf = Vec::new();
            encode_varint_i32(v, &mut buf);
            let mut pos = 0;
            assert_eq!(decode_varint_i32(&buf, &mut pos).unwrap(), v);
        }
        // -1 specifically produces the well-known 10-byte encoding (all
        // bits set in the sign-extended 64-bit form) — verifies this isn't
        // just "an" encoding that happens to round-trip on its own, but the
        // real proto2 wire form other implementations also produce.
        let mut buf = Vec::new();
        encode_varint_i32(-1, &mut buf);
        assert_eq!(buf.len(), 10, "negative int32 must sign-extend to the standard 10-byte form");
    }

    #[test]
    fn int64_round_trips_including_negatives() {
        for v in [-1i64, i64::MIN, 0, i64::MAX] {
            let mut buf = Vec::new();
            encode_varint_i64(v, &mut buf);
            let mut pos = 0;
            assert_eq!(decode_varint_i64(&buf, &mut pos).unwrap(), v);
        }
    }

    #[test]
    fn fixed64_round_trips_doubles() {
        for v in [0.0f64, 1.5, -3.25, f64::MAX] {
            let mut buf = Vec::new();
            encode_fixed64(v, &mut buf);
            let mut pos = 0;
            assert_eq!(decode_fixed64(&buf, &mut pos).unwrap(), v);
        }
    }

    #[test]
    fn length_delimited_round_trips_and_rejects_truncation() {
        let payload = b"hello world";
        let mut buf = Vec::new();
        encode_length_delimited(payload, &mut buf);
        let mut pos = 0;
        assert_eq!(decode_length_delimited(&buf, &mut pos).unwrap(), payload);

        let truncated = &buf[..buf.len() - 1];
        let mut pos = 0;
        assert_eq!(decode_length_delimited(truncated, &mut pos), Err(WireError::LengthDelimitedEof));
    }

    #[test]
    fn a_full_tag_plus_value_round_trips_through_decode_field() {
        let mut buf = Vec::new();
        encode_tag(5, WireType::LengthDelimited, &mut buf);
        encode_length_delimited(b"payload", &mut buf);
        let mut pos = 0;
        let (num, field) = decode_field(&buf, &mut pos).unwrap();
        assert_eq!(num, 5);
        match field {
            RawField::LengthDelimited(s) => assert_eq!(s, b"payload"),
            _ => panic!("expected LengthDelimited"),
        }
        assert_eq!(pos, buf.len());
    }

    #[test]
    fn a_truncated_varint_is_an_error_not_a_panic() {
        let mut pos = 0;
        assert_eq!(decode_varint(&[0x80], &mut pos), Err(WireError::VarintEof));
    }
}
