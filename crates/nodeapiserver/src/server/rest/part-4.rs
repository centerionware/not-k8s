
/// What [`resolve_resource`] found `(group, version, resource)` to be —
/// either a built-in with a compiled proto schema (`resolve_kind`/
/// `schema_for_gvk`, unchanged from before Group K existed), or a
/// CRD-defined resource (`apiextensions::registry`), which has no
/// compiled schema at all: its body is stored/read as plain JSON, and
/// defaulting (when `open_api_schema` is present) walks that schema at
/// runtime instead of a compiled `FIELD_META` table
/// (`apiextensions::schema_defaults`).
struct ResolvedResource {
    include!("body-6-1.rs");
    include!("body-6-2.rs");
}
