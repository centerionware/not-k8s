
/// Real upstream's own `Type` discriminator convention (`intstr.Int = 0`,
/// `intstr.String = 1`, confirmed directly against
/// `pkg/util/intstr/intstr.go`): a JSON number encodes as `{type: 0,
/// intVal: N}` (`type: 0` is proto2 `optional`'s own zero value, so
/// nothing is written for it — same "omit an explicit zero" posture
/// every other optional scalar field in this codec already takes), a
/// JSON string as `{type: 1, strVal: S}`.
fn encode_int_or_string(message: &str, field: &ProtoField, value: &Value) -> Result<Vec<u8>> {
    include!("body-20-1.rs");
    include!("body-20-2.rs");
}
