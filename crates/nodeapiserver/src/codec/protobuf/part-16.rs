
/// Real upstream's own `intstr.IntOrString`
/// (`vendor/protos/k8s.io/apimachinery/pkg/util/intstr/generated.proto`'s
/// own `message IntOrString { optional int64 type = 1; optional int32
/// intVal = 2; optional string strVal = 3; }`, confirmed directly) — the
/// *fourth* occurrence of the same real `is_time_message` pattern: a Go
/// type (`intstr.IntOrString`) with hand-written `MarshalJSON`/
/// `UnmarshalJSON` that produces/consumes a plain scalar (a bare number
/// or a bare string — `targetPort: 8080` or `targetPort: "https"` are
/// both real, valid JSON for this field) rather than its own compiled
/// struct shape. Used all over the vendored schema wherever a field can
/// name either a numeric or a named value (`Service.spec.ports[].
/// targetPort`, `NetworkPolicyPort.port`, ...). **Found live**, the same
/// way every prior occurrence was: nothing exercised a real object
/// carrying a real `IntOrString` field through this codec's own protobuf
/// round trip until `tests/aggregator_proxy_roundtrip.rs`'s live
/// `Service.spec.ports[].targetPort` did — every prior test either
/// stayed at the JSON/YAML codec layer or happened not to submit an
/// object with this field populated.
fn is_int_or_string_message(message: &str) -> bool {
    message == "io.k8s.apimachinery.pkg.util.intstr.IntOrString"
}
