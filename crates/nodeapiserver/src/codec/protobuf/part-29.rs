
fn as_varint(field: &str, raw: &RawField) -> Result<u64> {
    match raw {
        RawField::Varint(v) => Ok(*v),
        _ => Err(Error::UnexpectedWireShape { field: field.to_string() }),
    }
}

fn as_fixed64(field: &str, raw: &RawField) -> Result<f64> {
    match raw {
        RawField::Fixed64(v) => Ok(*v),
        _ => Err(Error::UnexpectedWireShape { field: field.to_string() }),
    }
}

// Only the inner `'a` (the wire buffer's own lifetime) is named — the
// returned slice borrows the original bytes, independent of how long the
// `RawField` wrapper value itself is borrowed for.
fn as_bytes<'a>(field: &str, raw: &RawField<'a>) -> Result<&'a [u8]> {
    match raw {
        RawField::LengthDelimited(b) => Ok(b),
        _ => Err(Error::UnexpectedWireShape { field: field.to_string() }),
    }
}
