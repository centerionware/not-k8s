//! GVK registry, conversion between multi-version API groups, and defaulting.
//!
//! Status: defaulting landed (`defaulting::apply_defaults`, driven by
//! `codegen::openapi_meta::FIELD_META`'s `default_json`/`ref_schema`) and
//! structural required-field validation landed
//! (`validation::validate_required`, driven by
//! `codegen::openapi_meta::REQUIRED_FIELDS`) — see each module's own doc
//! for what it does and doesn't cover. Conversion and the rest of
//! validation (formats, enums, cross-field, numeric ranges) not started
//! (see docs/APISERVER.md).

pub mod defaulting;
pub mod validation;
