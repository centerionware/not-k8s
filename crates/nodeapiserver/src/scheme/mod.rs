//! GVK registry, conversion between multi-version API groups, and defaulting.
//!
//! Status: defaulting landed (`defaulting::apply_defaults`, driven by
//! `codegen::openapi_meta::FIELD_META`'s `default_json`/`ref_schema`) —
//! see its own module doc for what it does and doesn't cover. Conversion
//! and validation (the rest of Group F) not started (see docs/APISERVER.md).

pub mod defaulting;
