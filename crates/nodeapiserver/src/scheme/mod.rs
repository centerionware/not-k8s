//! GVK registry, conversion between multi-version API groups, and defaulting.
//!
//! Status: defaulting landed (`defaulting::apply_defaults`, driven by
//! `codegen::openapi_meta::FIELD_META`'s `default_json`/`ref_schema`) and
//! structural validation landed — required-field presence
//! (`validation::validate_required`, driven by
//! `codegen::openapi_meta::REQUIRED_FIELDS`) and basic type-kind checking
//! (`validation::validate_types`, driven by the new
//! `codegen::openapi_meta::TYPE_INFO`) — see each module's own doc for
//! what it does and doesn't cover. `name_format` lands the first format
//! checks (`is_dns1123_label`/`is_dns1123_subdomain`/`is_dns1035_label`,
//! faithful ports of real upstream's own regex-based name validators) —
//! primitives only, not yet wired to any specific resource's own name
//! rule (which validator applies to which resource is real, separate,
//! hand-maintained-per-type knowledge upstream itself keeps this way; see
//! that module's own doc comment). Conversion and the rest of validation
//! (cross-field consistency, numeric ranges) not started (see
//! docs/APISERVER.md).

pub mod defaulting;
pub mod validation;
pub mod name_format;
