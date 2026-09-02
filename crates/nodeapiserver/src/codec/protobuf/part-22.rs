
/// Real upstream's own `JSONSchemaPropsOrArray`/`JSONSchemaPropsOrBool` —
/// the *third* occurrence this codec has needed of the same real
/// pattern `is_time_message`/`is_json_message` already established: a
/// Go type with hand-written `MarshalJSON`/`UnmarshalJSON` that doesn't
/// marshal as its own struct shape at all. `JSONSchemaProps.items` in
/// real JSON is either a single schema object or an array of schemas —
/// written completely unwrapped, never as `{"schema": {...}}` or
/// `{"jSONSchemas": [...]}` — but the vendored proto wraps it as
/// `JSONSchemaPropsOrArray { schema = 1; repeated jSONSchemas = 2; }`.
/// `JSONSchemaProps.additionalProperties` is either a bare `true`/`false`
/// or a schema object — `JSONSchemaPropsOrBool { allows = 1; schema =
/// 2; }` on the wire. Found live, the same way the other two were: a
/// stored `CustomResourceDefinition`'s own `items`/`additionalProperties`
/// silently decoded to an empty object (no `properties`, no `type`,
/// nothing) on every real read, discovered only once
/// `tests/crd_roundtrip.rs`'s own strategic-merge-patch-against-a-CRD
/// test recursed into a list field's `items` schema for real.
fn is_json_schema_props_or_array(message: &str) -> bool {
    matches!(message, "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaPropsOrArray" | "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1beta1.JSONSchemaPropsOrArray")
}

fn is_json_schema_props_or_bool(message: &str) -> bool {
    matches!(message, "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1.JSONSchemaPropsOrBool" | "io.k8s.apiextensions-apiserver.pkg.apis.apiextensions.v1beta1.JSONSchemaPropsOrBool")
}
