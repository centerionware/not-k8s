//! `EncryptionConfiguration` YAML parsing — the config-loader half of
//! `storage::encryption` (that module builds the transform primitives;
//! this one turns a real `apiserver.config.k8s.io/v1`
//! `EncryptionConfiguration` document, fetched and read directly from
//! `staging/src/k8s.io/apiserver/pkg/apis/apiserver/v1/types_encryption.go`,
//! into a resolvable set of them). **Wired end to end now**:
//! `storage::client::StorageClient::with_encryption` attaches the parsed
//! config, and `server::rest::decrypt_and_decode`/`encrypt_for_storage`
//! are the shared wiring surface every real read/write verb — `range`/
//! `put`/`txn`/`watch` all included — funnels through (see
//! `docs/APISERVER.md`'s own Group C section for the full account of
//! why this was deferred until it could be done for all of them at
//! once, not gap-by-gap).
//!
//! Parses into `serde_json::Value` rather than a derived typed struct —
//! same "round-trips through `Value`, no separate YAML-shaped type"
//! posture `codec::yaml` already established for this crate, since this
//! crate has no direct `serde` dependency of its own (only `serde_json`/
//! `serde_yaml`), and every other structured-data reader in this crate
//! already walks a `Value` by hand rather than deriving.
//!
//! # Providers ported
//!
//! `aesgcm` (`storage::encryption::Gcm`) and `identity`
//! (`storage::encryption::Identity`) — the same two real upstream
//! providers `storage::encryption`'s own doc comment already names as
//! built. `aescbc`/`secretbox`/`kms` entries parse structurally (so a
//! config file that names one doesn't fail to parse at all) but
//! [`build`] returns a real, named [`Error::UnsupportedProvider`] rather
//! than silently dropping or misapplying them — the same "fail loud on
//! what isn't ported" posture as `is_extended_resource_name`'s own
//! `IsQualifiedName` gap.
//!
//! # Resource matching
//!
//! Real upstream's own `resources` field per entry: a bare name
//! (`secrets`) matches only the core group; `<resource>.<group>` matches
//! a specific non-core group; `*.` matches every core-group resource;
//! `*.<group>` matches every resource in a specific non-core group;
//! `*.*` matches everything. Real upstream's own doc comment: "Resource
//! lists are processed in order, with earlier lists taking precedence"
//! — [`transformers_for`] returns the *first* entry whose `resources`
//! list matches, same as [`crate::storage::encryption::PrefixTransformers`]
//! itself already does one level down for providers/keys. **Not
//! ported**: real upstream's own build-time validation that overlapping
//! wildcards within/across entries aren't used inconsistently — this
//! port just takes first-match-wins at face value, same as it takes the
//! document's own field values at face value elsewhere.

use crate::storage::encryption::{Gcm, Identity, PrefixTransformer, PrefixTransformers, AES_GCM_PREFIX_V1};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parsing EncryptionConfiguration YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("EncryptionConfiguration.resources[{index}] is missing its own \"resources\" list")]
    MissingResources { index: usize },
    #[error("EncryptionConfiguration.resources[{index}] is missing its own \"providers\" list, or it's empty")]
    NoProviders { index: usize },
    #[error("EncryptionConfiguration.resources[{index}]'s {provider} key {key_name:?} has invalid base64: {source}")]
    InvalidKeyBase64 { index: usize, provider: &'static str, key_name: String, #[source] source: base64::DecodeError },
    #[error("EncryptionConfiguration.resources[{index}]'s {provider} key {key_name:?} must decode to exactly 32 bytes for AES-256, got {actual}")]
    WrongKeyLength { index: usize, provider: &'static str, key_name: String, actual: usize },
    #[error("EncryptionConfiguration.resources[{index}] names a provider this build doesn't implement: {provider} (only aesgcm/identity are ported — see this module's own doc comment)")]
    UnsupportedProvider { index: usize, provider: &'static str },
}

/// One resolved entry: which resources it applies to (real upstream's
/// own glob forms, unparsed — matched textually by [`resource_matches`])
/// and the [`PrefixTransformers`] built from its provider list.
pub struct ResourceEntry {
    pub resources: Vec<String>,
    pub transformers: PrefixTransformers,
}

pub struct EncryptionConfig {
    pub entries: Vec<ResourceEntry>,
}

fn build_aes_gcm(index: usize, keys: &Value) -> Result<PrefixTransformers, Error> {
    use base64::Engine;
    let mut transformers = Vec::new();
    for key in keys.as_array().into_iter().flatten() {
        let name = key.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        let secret_b64 = key.get("secret").and_then(Value::as_str).unwrap_or("");
        let secret = base64::engine::general_purpose::STANDARD.decode(secret_b64).map_err(|source| Error::InvalidKeyBase64 { index, provider: "aesgcm", key_name: name.clone(), source })?;
        let len = secret.len();
        let key_bytes: [u8; 32] = secret.try_into().map_err(|_| Error::WrongKeyLength { index, provider: "aesgcm", key_name: name.clone(), actual: len })?;
        transformers.push(PrefixTransformer { prefix: format!("{name}:").into_bytes(), transformer: Box::new(Gcm::new(key_bytes)) });
    }
    Ok(PrefixTransformers::new(transformers))
}

/// Builds the real [`EncryptionConfig`] this crate can act on from the
/// parsed document's own `resources` array — the one step that can fail
/// on an unsupported provider ([`Error::UnsupportedProvider`]), kept
/// separate from [`parse`] so a caller could, in principle, inspect the
/// raw document before deciding whether an unsupported provider is fatal
/// (not done by any caller today, but the split costs nothing and
/// mirrors this crate's usual parse/decide separation).
fn build(doc: &Value) -> Result<EncryptionConfig, Error> {
    let mut entries = Vec::new();
    for (index, entry) in doc.get("resources").and_then(Value::as_array).into_iter().flatten().enumerate() {
        let Some(resources) = entry.get("resources").and_then(Value::as_array) else {
            return Err(Error::MissingResources { index });
        };
        let resources: Vec<String> = resources.iter().filter_map(Value::as_str).map(str::to_string).collect();

        let providers = entry.get("providers").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        if providers.is_empty() {
            return Err(Error::NoProviders { index });
        }
        let mut transformers = Vec::new();
        for provider in providers {
            if let Some(aesgcm) = provider.get("aesgcm") {
                let keys = aesgcm.get("keys").cloned().unwrap_or(Value::Array(vec![]));
                let inner = build_aes_gcm(index, &keys)?;
                transformers.push(PrefixTransformer { prefix: AES_GCM_PREFIX_V1.as_bytes().to_vec(), transformer: Box::new(inner) });
            } else if provider.get("identity").is_some() {
                transformers.push(PrefixTransformer { prefix: Vec::new(), transformer: Box::new(Identity) });
            } else if provider.get("aescbc").is_some() {
                return Err(Error::UnsupportedProvider { index, provider: "aescbc" });
            } else if provider.get("secretbox").is_some() {
                return Err(Error::UnsupportedProvider { index, provider: "secretbox" });
            } else if provider.get("kms").is_some() {
                return Err(Error::UnsupportedProvider { index, provider: "kms" });
            } else {
                return Err(Error::UnsupportedProvider { index, provider: "unknown" });
            }
        }
        entries.push(ResourceEntry { resources, transformers: PrefixTransformers::new(transformers) });
    }
    Ok(EncryptionConfig { entries })
}

/// Parses a real `EncryptionConfiguration` YAML document (`apiVersion`/
/// `kind` are accepted-and-ignored if present, same as every other field
/// this crate doesn't read — there's no generic `TypeMeta` decoder here
/// to strip them first).
pub fn parse(yaml: &str) -> Result<EncryptionConfig, Error> {
    let doc: Value = serde_yaml::from_str(yaml)?;
    build(&doc)
}

/// Real upstream's own resource-name matching (`resources` field doc
/// comment): a bare name matches only the core group; `<resource>.<group>`
/// matches a specific non-core group; `*.` matches every core-group
/// resource; `*.<group>` matches every resource in a specific non-core
/// group; `*.*` matches everything.
fn resource_matches(pattern: &str, group: &str, resource: &str) -> bool {
    if pattern == "*.*" {
        return true;
    }
    if pattern == "*." {
        return group.is_empty();
    }
    if let Some(wildcard_group) = pattern.strip_prefix("*.") {
        return group == wildcard_group;
    }
    if group.is_empty() {
        pattern == resource
    } else {
        pattern == format!("{resource}.{group}")
    }
}

/// The first entry (real upstream's own "earlier lists take precedence")
/// whose `resources` list matches `(group, resource)`, if any.
pub fn transformers_for<'a>(config: &'a EncryptionConfig, group: &str, resource: &str) -> Option<&'a PrefixTransformers> {
    config.entries.iter().find(|entry| entry.resources.iter().any(|pattern| resource_matches(pattern, group, resource))).map(|entry| &entry.transformers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::encryption::Transformer;

    #[test]
    fn resource_matches_bare_name_is_core_group_only() {
        assert!(resource_matches("secrets", "", "secrets"));
        assert!(!resource_matches("secrets", "apps", "secrets"));
    }

    #[test]
    fn resource_matches_dotted_form_is_a_specific_non_core_group() {
        assert!(resource_matches("deployments.apps", "apps", "deployments"));
        assert!(!resource_matches("deployments.apps", "", "deployments"));
        assert!(!resource_matches("deployments.apps", "batch", "deployments"));
    }

    #[test]
    fn resource_matches_wildcards() {
        assert!(resource_matches("*.", "", "configmaps"));
        assert!(!resource_matches("*.", "apps", "deployments"));
        assert!(resource_matches("*.apps", "apps", "deployments"));
        assert!(!resource_matches("*.apps", "batch", "jobs"));
        assert!(resource_matches("*.*", "anything", "whatever"));
        assert!(resource_matches("*.*", "", "pods"));
    }

    #[test]
    fn parses_a_real_aesgcm_and_identity_config() {
        let key_b64 = { use base64::Engine; base64::engine::general_purpose::STANDARD.encode([7u8; 32]) };
        let yaml = format!("resources:\n- resources:\n  - secrets\n  providers:\n  - aesgcm:\n      keys:\n      - name: key1\n        secret: {key_b64}\n- resources:\n  - '*.*'\n  providers:\n  - identity: {{}}\n");
        let config = parse(&yaml).expect("valid EncryptionConfiguration");
        assert_eq!(config.entries.len(), 2);
        assert_eq!(config.entries[0].resources, vec!["secrets"]);

        // Round trip through the built transformer, proving the parsed
        // key material is real and usable, not just structurally present.
        let transformers = transformers_for(&config, "", "secrets").expect("secrets should match the first entry");
        let ciphertext = transformers.transform_to_storage(b"hello", b"aad").unwrap();
        let (plaintext, _stale) = transformers.transform_from_storage(&ciphertext, b"aad").unwrap();
        assert_eq!(plaintext, b"hello");
    }

    #[test]
    fn transformers_for_picks_the_first_matching_entry() {
        let key_b64 = { use base64::Engine; base64::engine::general_purpose::STANDARD.encode([9u8; 32]) };
        let yaml = format!("resources:\n- resources:\n  - events\n  providers:\n  - identity: {{}}\n- resources:\n  - '*.*'\n  providers:\n  - aesgcm:\n      keys:\n      - name: key1\n        secret: {key_b64}\n");
        let config = parse(&yaml).unwrap();
        // `events` matches the first (identity) entry even though `*.*`
        // in the second entry would also match -- first-match-wins.
        let events_transformers = transformers_for(&config, "", "events").unwrap();
        let stored = events_transformers.transform_to_storage(b"plain", b"").unwrap();
        assert_eq!(stored, b"plain", "identity provider should not transform the bytes at all");

        let other_transformers = transformers_for(&config, "apps", "deployments").unwrap();
        let stored = other_transformers.transform_to_storage(b"plain", b"").unwrap();
        assert_ne!(stored, b"plain", "aesgcm provider should have actually encrypted the bytes");
    }

    #[test]
    fn transformers_for_returns_none_when_nothing_matches() {
        let yaml = "resources:\n- resources:\n  - secrets\n  providers:\n  - identity: {}\n";
        let config = parse(yaml).unwrap();
        assert!(transformers_for(&config, "apps", "deployments").is_none());
    }

    #[test]
    fn an_unsupported_provider_is_a_real_named_error_not_silently_dropped() {
        let yaml = "resources:\n- resources:\n  - secrets\n  providers:\n  - aescbc:\n      keys:\n      - name: key1\n        secret: c2VjcmV0IGlzIHNlY3VyZQ==\n";
        let err = match parse(yaml) {
            Err(e) => e,
            Ok(_) => panic!("aescbc isn't ported"),
        };
        assert!(matches!(err, Error::UnsupportedProvider { provider: "aescbc", .. }));
    }

    #[test]
    fn a_resource_entry_with_no_providers_is_rejected() {
        let yaml = "resources:\n- resources:\n  - secrets\n  providers: []\n";
        assert!(matches!(parse(yaml), Err(Error::NoProviders { index: 0 })));
    }

    #[test]
    fn a_wrong_length_aesgcm_key_is_rejected() {
        let short_key_b64 = { use base64::Engine; base64::engine::general_purpose::STANDARD.encode([1u8; 16]) };
        let yaml = format!("resources:\n- resources:\n  - secrets\n  providers:\n  - aesgcm:\n      keys:\n      - name: key1\n        secret: {short_key_b64}\n");
        let err = match parse(&yaml) {
            Err(e) => e,
            Ok(_) => panic!("16-byte key is not valid AES-256"),
        };
        assert!(matches!(err, Error::WrongKeyLength { actual: 16, .. }));
    }
}
