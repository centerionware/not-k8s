
/// The real `JSONSchemaProps` message name for a `...OrArray`/`...OrBool`
/// wrapper of the same version — `"...v1.JSONSchemaPropsOrArray"` ->
/// `"...v1.JSONSchemaProps"` — both wrapper messages nest exactly that
/// type, for exactly the version they themselves belong to.
fn json_schema_props_message_for(wrapper_message: &str) -> String {
    include!("body-27-1.rs");
    include!("body-27-2.rs");
}
