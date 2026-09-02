
fn type_mismatch(message: &str, field: &ProtoField, expected: &'static str, value: &Value) -> Error {
    Error::TypeMismatch {
        message: message.to_string(),
        field: field.json_name.to_string(),
        proto_type: field.proto_type.to_string(),
        expected,
        value: value.clone(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalarKind {
    Bool,
    Int32,
    Int64,
    Double,
    String,
    Bytes,
}

impl ScalarKind {
    /// `None` means "not a scalar — a message reference". `map<...>` is
    /// handled separately by callers before this is ever consulted.
    fn of(proto_type: &str) -> Option<ScalarKind> {
        match proto_type {
            "bool" => Some(ScalarKind::Bool),
            "int32" => Some(ScalarKind::Int32),
            "int64" => Some(ScalarKind::Int64),
            "double" => Some(ScalarKind::Double),
            "string" => Some(ScalarKind::String),
            "bytes" => Some(ScalarKind::Bytes),
            _ => None,
        }
    }
}
