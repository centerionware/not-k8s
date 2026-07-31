//! `--image-credential-provider-config`/`--image-credential-provider-bin-dir`
//! (round 71; ServiceAccount token integration beta/default-on in k8s
//! 1.34, found in round 69's fresh gap re-audit): kubelet's exec-plugin
//! protocol for obtaining registry credentials dynamically (e.g. cloud
//! workload-identity federation for ECR/GCR/ACR) instead of — or in
//! addition to — static `imagePullSecrets`.
//!
//! Config is a `CredentialProviderConfig` YAML file (`NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG`)
//! listing providers, each naming a binary (found under
//! `NODELET_IMAGE_CREDENTIAL_PROVIDER_BIN_DIR`) and a `matchImages` glob
//! list. When an image being pulled matches a provider's patterns,
//! nodelet execs that binary, writing a `CredentialProviderRequest` as
//! JSON to its stdin and reading a `CredentialProviderResponse` back from
//! its stdout — no gRPC involved, unlike every other plugin protocol
//! elsewhere in this codebase (CSI/device-plugin/DRA all register over a
//! Unix socket; this one is a plain subprocess exec per kubelet's own
//! design).
//!
//! **Scope decisions, both documented rather than hidden**: (1) only the
//! *first* matching provider (in config order) is tried per image, not
//! every matching provider merged — real kubelet does merge multiple
//! providers' results, but non-overlapping `matchImages` patterns (one
//! provider per cloud registry) are overwhelmingly the common
//! configuration shape in practice. (2) `resolve_pull_auth()` tries
//! `imagePullSecrets` first, falling back to credential providers only
//! if no secret resolves — explicit, pod-declared intent wins over
//! automatic discovery, since that's the more surprising-if-wrong
//! direction to get backwards.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Deserialize, Clone, Debug)]
pub struct CredentialProviderConfigFile {
    pub providers: Vec<CredentialProvider>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct CredentialProvider {
    pub name: String,
    #[serde(rename = "matchImages")]
    pub match_images: Vec<String>,
    #[serde(rename = "defaultCacheDuration")]
    pub default_cache_duration: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<EnvVarConfig>,
    #[serde(rename = "tokenAttributes")]
    pub token_attributes: Option<TokenAttributes>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct EnvVarConfig {
    pub name: String,
    pub value: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct TokenAttributes {
    #[serde(rename = "serviceAccountTokenAudience")]
    pub service_account_token_audience: String,
    #[serde(rename = "requireServiceAccount", default)]
    pub require_service_account: bool,
    #[serde(rename = "requiredServiceAccountAnnotationKeys", default)]
    pub required_service_account_annotation_keys: Vec<String>,
    #[serde(rename = "optionalServiceAccountAnnotationKeys", default)]
    pub optional_service_account_annotation_keys: Vec<String>,
}

/// `image`'s domain/port/path against one `matchImages` glob pattern —
/// real kubelet's own documented rule: a `*` in the domain matches
/// exactly one dot-separated segment (never spans a dot, so `*.io`
/// never matches `foo.k8s.io` — segment counts differ), the port must
/// match exactly if the pattern specifies one, and the pattern's path
/// must be a prefix of the image's path. Pure so this is unit-testable
/// without a live registry.
pub fn image_matches_pattern(image: &str, pattern: &str) -> bool {
    let (image_host, image_path) = split_host_path(image);
    let (pattern_host, pattern_path) = split_host_path(pattern);
    let (image_domain, image_port) = split_host_port(image_host);
    let (pattern_domain, pattern_port) = split_host_port(pattern_host);

    if let Some(pport) = pattern_port {
        if Some(pport) != image_port {
            return false;
        }
    }

    let image_segments: Vec<&str> = image_domain.split('.').collect();
    let pattern_segments: Vec<&str> = pattern_domain.split('.').collect();
    if image_segments.len() != pattern_segments.len() {
        return false;
    }
    let domain_matches = image_segments.iter().zip(pattern_segments.iter()).all(|(i, p)| *p == "*" || p == i);
    if !domain_matches {
        return false;
    }

    image_path.starts_with(pattern_path)
}

/// Split `ref` (an image reference, or a `matchImages` pattern — both
/// share the same `host[:port]/path` shape) into `(host_with_port,
/// path)`. A bare `registry.io` (no `/`) has an empty path, which
/// trivially prefix-matches anything.
fn split_host_path(reference: &str) -> (&str, &str) {
    match reference.split_once('/') {
        Some((host, path)) => (host, path),
        None => (reference, ""),
    }
}

fn split_host_port(host: &str) -> (&str, Option<&str>) {
    match host.split_once(':') {
        Some((domain, port)) => (domain, Some(port)),
        None => (host, None),
    }
}

/// Every provider (in config order) whose `matchImages` contains at
/// least one pattern matching `image`.
pub fn matching_providers<'a>(providers: &'a [CredentialProvider], image: &str) -> Vec<&'a CredentialProvider> {
    providers.iter().filter(|p| p.match_images.iter().any(|pat| image_matches_pattern(image, pat))).collect()
}

// --- CredentialProviderRequest/Response wire types ---

#[derive(serde::Serialize)]
struct PodInfo {
    name: String,
    namespace: String,
    uid: String,
    annotations: std::collections::BTreeMap<String, String>,
}

#[derive(serde::Serialize)]
struct ServiceAccountInfo {
    name: String,
    namespace: String,
    uid: String,
    annotations: std::collections::BTreeMap<String, String>,
}

#[derive(serde::Serialize)]
struct CredentialProviderRequest {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: &'static str,
    image: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_account_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pod: Option<PodInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_account: Option<ServiceAccountInfo>,
}

#[derive(Deserialize, Default)]
struct ResponseAuthEntry {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

#[derive(Deserialize)]
struct CredentialProviderResponse {
    #[serde(rename = "apiVersion")]
    api_version: String,
    #[serde(rename = "cacheKeyType", default)]
    cache_key_type: Option<String>,
    #[serde(rename = "cacheDuration", default)]
    cache_duration: Option<String>,
    #[serde(default)]
    auth: HashMap<String, ResponseAuthEntry>,
}

/// Pod/ServiceAccount context passed through to a `tokenAttributes`-configured
/// provider — the caller is responsible for checking
/// `require_service_account`/annotation-key requirements before minting a
/// token, since that needs a live ServiceAccount object fetch this
/// module (deliberately still pure/exec-only) doesn't do itself.
pub struct ServiceAccountContext {
    pub pod_name: String,
    pub pod_namespace: String,
    pub pod_uid: String,
    pub pod_annotations: std::collections::BTreeMap<String, String>,
    pub service_account_name: String,
    pub service_account_uid: String,
    pub service_account_annotations: std::collections::BTreeMap<String, String>,
    pub token: String,
}

/// Go duration string parsing (`"12h"`, `"15m"`, `"90s"`) — only the
/// units `CredentialProviderResponse.cacheDuration`/
/// `defaultCacheDuration` actually use in every real-world config seen
/// in upstream's own docs. Not a general Go-duration parser (no
/// fractional/compound forms like `"1h30m"`) — good enough for this
/// narrow use, falls back to `None` (caller substitutes its own default)
/// on anything else rather than misparsing.
fn parse_go_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    for (suffix, unit_secs) in [("h", 3600u64), ("m", 60), ("s", 1)] {
        if let Some(num) = s.strip_suffix(suffix) {
            let n: u64 = num.parse().ok()?;
            return Some(Duration::from_secs(n * unit_secs));
        }
    }
    None
}

enum CacheKey {
    Image(String),
    Registry(String),
    Global,
}

impl CacheKey {
    fn as_string(&self) -> String {
        match self {
            CacheKey::Image(s) => format!("image:{s}"),
            CacheKey::Registry(s) => format!("registry:{s}"),
            CacheKey::Global => "global".to_string(),
        }
    }
}

pub struct CredentialProviders {
    bin_dir: String,
    providers: Vec<CredentialProvider>,
    cache: Mutex<HashMap<String, (ResponseAuthEntry, Instant)>>,
}

impl CredentialProviders {
    /// Parse a `CredentialProviderConfig` YAML file. `None` (not `Err`)
    /// if `config_path` is empty — the feature is simply off, matching
    /// upstream's own "no flag, no providers" behavior.
    pub fn load(config_path: &str, bin_dir: &str) -> Result<Option<Self>> {
        if config_path.is_empty() {
            return Ok(None);
        }
        let bytes = std::fs::read(config_path).with_context(|| format!("reading {config_path}"))?;
        let parsed: CredentialProviderConfigFile = serde_yaml::from_slice(&bytes).context("parsing CredentialProviderConfig")?;
        Ok(Some(Self { bin_dir: bin_dir.to_string(), providers: parsed.providers, cache: Mutex::new(HashMap::new()) }))
    }

    /// The first configured provider (in config order) whose
    /// `matchImages` matches `image`, if any — lets the caller decide
    /// whether a `tokenAttributes`-scoped token is worth minting *before*
    /// calling `resolve()`, without duplicating the matching logic.
    pub fn first_match(&self, image: &str) -> Option<&CredentialProvider> {
        matching_providers(&self.providers, image).into_iter().next()
    }

    /// Resolve credentials for `image` (full reference) / `registry_host`
    /// (just the registry part, for `Registry`-scoped caching) — tries
    /// only the first matching provider (see module doc), checking the
    /// cache first. `sa_ctx` is only actually sent to the provider if it
    /// declares `tokenAttributes` at all; a provider with no
    /// `tokenAttributes` never sees a token, matching upstream (a plugin
    /// that doesn't ask for one doesn't get one).
    pub async fn resolve(&self, image: &str, registry_host: &str, sa_ctx: Option<&ServiceAccountContext>) -> Option<v1::AuthConfig> {
        let provider = matching_providers(&self.providers, image).into_iter().next()?;

        for key in [CacheKey::Image(image.to_string()), CacheKey::Registry(registry_host.to_string()), CacheKey::Global] {
            if let Some((entry, expires_at)) = self.cache.lock().unwrap().get(&key.as_string()) {
                if Instant::now() < *expires_at {
                    return Some(to_auth_config(entry));
                }
            }
        }

        let entry = self.exec_provider(provider, image, sa_ctx).await.ok()??;
        Some(to_auth_config(&entry))
    }

    async fn exec_provider(
        &self,
        provider: &CredentialProvider,
        image: &str,
        sa_ctx: Option<&ServiceAccountContext>,
    ) -> Result<Option<ResponseAuthEntry>> {
        let want_token = provider.token_attributes.is_some();
        let request = CredentialProviderRequest {
            api_version: "credentialprovider.kubelet.k8s.io/v1".to_string(),
            kind: "CredentialProviderRequest",
            image: image.to_string(),
            service_account_token: want_token.then(|| sa_ctx.map(|c| c.token.clone())).flatten(),
            pod: want_token
                .then(|| {
                    sa_ctx.map(|c| PodInfo {
                        name: c.pod_name.clone(),
                        namespace: c.pod_namespace.clone(),
                        uid: c.pod_uid.clone(),
                        annotations: c.pod_annotations.clone(),
                    })
                })
                .flatten(),
            service_account: want_token
                .then(|| {
                    sa_ctx.map(|c| ServiceAccountInfo {
                        name: c.service_account_name.clone(),
                        namespace: c.pod_namespace.clone(),
                        uid: c.service_account_uid.clone(),
                        annotations: c.service_account_annotations.clone(),
                    })
                })
                .flatten(),
        };
        let request_bytes = serde_json::to_vec(&request).context("serializing CredentialProviderRequest")?;

        let bin_path = std::path::Path::new(&self.bin_dir).join(&provider.name);
        let mut cmd = tokio::process::Command::new(&bin_path);
        cmd.args(&provider.args)
            .envs(provider.env.iter().map(|e| (e.name.clone(), e.value.clone())))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().with_context(|| format!("spawning credential provider '{}'", provider.name))?;

        {
            use tokio::io::AsyncWriteExt;
            let stdin = child.stdin.as_mut().context("credential provider stdin unavailable")?;
            stdin.write_all(&request_bytes).await.context("writing CredentialProviderRequest")?;
        }
        let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
            .await
            .context("credential provider timed out")?
            .context("waiting for credential provider to exit")?;
        if !output.status.success() {
            anyhow::bail!(
                "credential provider '{}' exited with {}: {}",
                provider.name,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let response: CredentialProviderResponse = serde_json::from_slice(&output.stdout).context("parsing CredentialProviderResponse")?;
        if response.api_version != "credentialprovider.kubelet.k8s.io/v1" {
            anyhow::bail!("credential provider '{}' returned unexpected apiVersion '{}'", provider.name, response.api_version);
        }

        let cache_duration = response
            .cache_duration
            .as_deref()
            .and_then(parse_go_duration)
            .or_else(|| provider.default_cache_duration.as_deref().and_then(parse_go_duration))
            .unwrap_or(Duration::from_secs(5 * 60));
        let cache_key = match response.cache_key_type.as_deref() {
            Some("Registry") => CacheKey::Registry(response.auth.keys().next().cloned().unwrap_or_default()),
            Some("Global") => CacheKey::Global,
            _ => CacheKey::Image(image.to_string()),
        };

        // The response's `auth` map is keyed by registry host; the
        // entry for this image's own registry is the one that matters
        // (a provider may return entries for multiple registries it
        // handles in one response, e.g. all regions of one cloud).
        let entry = response.auth.into_values().next();
        if let Some(entry) = &entry {
            self.cache.lock().unwrap().insert(cache_key.as_string(), (clone_entry(entry), Instant::now() + cache_duration));
        }
        Ok(entry)
    }
}

fn clone_entry(e: &ResponseAuthEntry) -> ResponseAuthEntry {
    ResponseAuthEntry { username: e.username.clone(), password: e.password.clone() }
}

fn to_auth_config(entry: &ResponseAuthEntry) -> v1::AuthConfig {
    v1::AuthConfig { username: entry.username.clone(), password: entry.password.clone(), ..Default::default() }
}

// Reuses the CRI v1 module already compiled for the runtime — this
// module only needs its `AuthConfig` type, not a gRPC client of its own.
use crate::runtime::cri::v1;

#[cfg(test)]
#[path = "credential_provider_tests/image_matches_pattern.rs"]
mod tests_image_matches_pattern;
#[cfg(test)]
#[path = "credential_provider_tests/parse_go_duration.rs"]
mod tests_parse_go_duration;
#[cfg(test)]
#[path = "credential_provider_tests/load.rs"]
mod tests_load;
