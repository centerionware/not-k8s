
/// `resource.Quantity`/`resource.QuantityValue`
/// (`vendor/protos/k8s.io/apimachinery/pkg/api/resource/generated.proto`'s
/// own `message Quantity { optional string string = 1; }` — both
/// messages share the identical one-field shape, confirmed directly) —
/// the *fifth* real occurrence of the same well-known-type pattern,
/// found via the same deliberate audit pass that found `RawExtension`
/// above: real upstream's own `+protobuf.embed=string` annotation plus a
/// hand-written `MarshalJSON`/`UnmarshalJSON` (delegating to `Quantity.
/// String()`) means the JSON representation is a bare string
/// (`"100m"`, `"1.5Gi"`), never `{"string": "100m"}`. Reused directly by
/// `scheme::quantity::Quantity`'s own string grammar at the JSON/YAML
/// codec layer already — this is the same value's *protobuf* wire
/// shape, a separate layer nothing had exercised live yet.
fn is_quantity_message(message: &str) -> bool {
    matches!(message, "io.k8s.apimachinery.pkg.api.resource.Quantity" | "io.k8s.apimachinery.pkg.api.resource.QuantityValue")
}
