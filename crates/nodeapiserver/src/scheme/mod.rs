//! GVK registry, conversion between multi-version API groups, and defaulting.
//!
//! Status: defaulting landed (`defaulting::apply_defaults`, driven by
//! `codegen::openapi_meta::FIELD_META`'s `default_json`/`ref_schema`) and
//! structural validation landed — required-field presence
//! (`validation::validate_required`, driven by
//! `codegen::openapi_meta::REQUIRED_FIELDS`) and basic type-kind checking
//! (`validation::validate_types`, driven by the new
//! `codegen::openapi_meta::TYPE_INFO`) — see each module's own doc for
//! what it does and doesn't cover. Conversion and the rest of validation
//! (formats, cross-field, numeric ranges) not started (see
//! docs/APISERVER.md).

pub mod defaulting;
pub mod validation;
