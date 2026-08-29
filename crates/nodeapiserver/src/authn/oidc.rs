//! OpenID Connect bearer-token authentication.
//!
//! This is the network-backed counterpart to service-account
//! authentication: discover the issuer's JWKS once at startup, verify JWTs
//! locally, and refresh the key set once when a token names an unknown or
//! rotated key. The supported contract follows kube-apiserver's OIDC
//! authenticator: issuer, audience, expiration, required claims,
//! configurable username/groups claims, and RS256/PS256/ES256 JWS algorithms.

use anyhow::{Context, Result};
use base64::Engine;
use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
use reqwest::Client;
use ring::signature::{
    RsaPublicKeyComponents, RSA_PKCS1_2048_8192_SHA256, RSA_PSS_2048_8192_SHA256,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

const DEFAULT_USERNAME_CLAIM: &str = "sub";

#[derive(Debug, Clone)]
pub struct Config {
    pub issuer_url: String,
    pub client_id: String,
    pub username_claim: String,
    pub username_prefix: Option<String>,
    pub groups_claim: Option<String>,
    pub groups_prefix: Option<String>,
    pub required_claims: Vec<(String, String)>,
    pub signing_algs: Vec<String>,
    pub ca_certificate_pem: Option<Vec<u8>>,
}

#[derive(Clone)]
pub struct Authenticator {
    client: Client,
    config: Arc<Config>,
    jwks_uri: String,
    keys: Arc<RwLock<HashMap<String, JsonWebKey>>>,
}

#[derive(Debug, Clone, Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    jwks_uri: String,
}

#[derive(Debug, Clone, Deserialize)]
struct JsonWebKey {
    kty: String,
    kid: Option<String>,
    alg: Option<String>,
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct JwksDocument {
    keys: Vec<JsonWebKey>,
}

impl Authenticator {
    /// Performs OIDC discovery and loads the initial key set. A configured
    /// issuer that cannot be contacted is a startup configuration error; the
    /// listener keeps OIDC disabled rather than accepting unverifiable tokens.
    pub async fn from_config(config: Config) -> Result<Self> {
        anyhow::ensure!(
            !config.issuer_url.trim().is_empty(),
            "OIDC issuer URL must not be empty"
        );
        anyhow::ensure!(
            !config.client_id.trim().is_empty(),
            "OIDC client ID must not be empty"
        );
        anyhow::ensure!(
            !config.signing_algs.is_empty(),
            "OIDC signing algorithm list must not be empty"
        );

        let mut builder = Client::builder().user_agent("not-k8s-nodeapiserver/oidc");
        if let Some(ca) = &config.ca_certificate_pem {
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_pem(ca).context("parsing OIDC CA certificate")?,
            );
        }
        let client = builder.build().context("building the OIDC HTTP client")?;
        let issuer_url = config.issuer_url.trim_end_matches('/');
        let discovery_url = format!("{issuer_url}/.well-known/openid-configuration");
        let discovery: DiscoveryDocument = client
            .get(&discovery_url)
            .send()
            .await
            .with_context(|| format!("fetching OIDC discovery document from {discovery_url}"))?
            .error_for_status()
            .context("OIDC discovery endpoint returned an error")?
            .json()
            .await
            .context("decoding the OIDC discovery document")?;
        anyhow::ensure!(
            discovery.issuer.trim_end_matches('/') == issuer_url,
            "OIDC discovery issuer does not match configured issuer URL"
        );
        anyhow::ensure!(
            !discovery.jwks_uri.is_empty(),
            "OIDC discovery document has no jwks_uri"
        );

        let authenticator = Self {
            client,
            config: Arc::new(config),
            jwks_uri: discovery.jwks_uri,
            keys: Arc::new(RwLock::new(HashMap::new())),
        };
        authenticator.refresh_keys().await?;
        Ok(authenticator)
    }

    async fn refresh_keys(&self) -> Result<()> {
        let document: JwksDocument = self
            .client
            .get(&self.jwks_uri)
            .send()
            .await
            .with_context(|| format!("fetching OIDC JWKS from {}", self.jwks_uri))?
            .error_for_status()
            .context("OIDC JWKS endpoint returned an error")?
            .json()
            .await
            .context("decoding the OIDC JWKS document")?;
        let keys = document
            .keys
            .into_iter()
            .enumerate()
            .map(|(index, key)| {
                (
                    key.kid
                        .clone()
                        .unwrap_or_else(|| format!("__no_kid_{index}")),
                    key,
                )
            })
            .collect();
        *self.keys.write().await = keys;
        Ok(())
    }

    /// Authenticate one bearer token. It refreshes the JWKS at most once per
    /// request, and only after the locally cached set cannot verify it.
    pub async fn authenticate(&self, token: &str) -> Option<crate::authn::x509::Identity> {
        let parsed = ParsedToken::parse(token)?;
        if let Some(identity) = self.verify(&parsed).await {
            return Some(identity);
        }
        self.refresh_keys().await.ok()?;
        self.verify(&parsed).await
    }

    async fn verify(&self, token: &ParsedToken) -> Option<crate::authn::x509::Identity> {
        let algorithm = token.header.get("alg").and_then(Value::as_str)?;
        if !self
            .config
            .signing_algs
            .iter()
            .any(|allowed| allowed == algorithm)
        {
            return None;
        }
        let key = {
            let keys = self.keys.read().await;
            select_key(
                &keys,
                token.header.get("kid").and_then(Value::as_str),
                algorithm,
            )?
            .clone()
        };
        verify_signature(
            &key,
            algorithm,
            token.signing_input.as_bytes(),
            &token.signature,
        )
        .ok()?;
        validate_claims(&self.config, &token.claims).map(|(username, groups)| {
            crate::authn::x509::Identity {
                name: username,
                groups,
                uid: None,
                credential_id: (String::new(), Vec::new()),
            }
        })
    }
}

struct ParsedToken {
    header: Value,
    claims: Value,
    signing_input: String,
    signature: Vec<u8>,
}

impl ParsedToken {
    fn parse(token: &str) -> Option<Self> {
        let mut parts = token.split('.');
        let header_part = parts.next()?;
        let claims_part = parts.next()?;
        let signature_part = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            header: serde_json::from_slice(&decode_segment(header_part)?).ok()?,
            claims: serde_json::from_slice(&decode_segment(claims_part)?).ok()?,
            signing_input: format!("{header_part}.{claims_part}"),
            signature: decode_segment(signature_part)?,
        })
    }
}

fn select_key<'a>(
    keys: &'a HashMap<String, JsonWebKey>,
    kid: Option<&str>,
    algorithm: &str,
) -> Option<&'a JsonWebKey> {
    if let Some(kid) = kid {
        return keys
            .get(kid)
            .filter(|key| key.alg.as_deref().is_none_or(|alg| alg == algorithm));
    }
    let mut candidates = keys
        .values()
        .filter(|key| key.alg.as_deref().is_none_or(|alg| alg == algorithm));
    let first = candidates.next()?;
    candidates.next().is_none().then_some(first)
}

fn verify_signature(
    key: &JsonWebKey,
    algorithm: &str,
    signing_input: &[u8],
    signature: &[u8],
) -> Result<(), ()> {
    match (algorithm, key.kty.as_str()) {
        ("RS256", "RSA") => {
            let n = decode_field(key.n.as_deref().ok_or(())?)?;
            let e = decode_field(key.e.as_deref().ok_or(())?)?;
            RsaPublicKeyComponents { n: &n, e: &e }
                .verify(&RSA_PKCS1_2048_8192_SHA256, signing_input, signature)
                .map_err(|_| ())
        }
        ("PS256", "RSA") => {
            let n = decode_field(key.n.as_deref().ok_or(())?)?;
            let e = decode_field(key.e.as_deref().ok_or(())?)?;
            RsaPublicKeyComponents { n: &n, e: &e }
                .verify(&RSA_PSS_2048_8192_SHA256, signing_input, signature)
                .map_err(|_| ())
        }
        ("ES256", "EC") if key.crv.as_deref() == Some("P-256") => {
            let mut point = Vec::with_capacity(65);
            point.push(4);
            point.extend(decode_field(key.x.as_deref().ok_or(())?)?);
            point.extend(decode_field(key.y.as_deref().ok_or(())?)?);
            let verifying_key = VerifyingKey::from_sec1_bytes(&point).map_err(|_| ())?;
            let signature = Signature::from_slice(signature).map_err(|_| ())?;
            verifying_key
                .verify(signing_input, &signature)
                .map_err(|_| ())
        }
        _ => Err(()),
    }
}

fn validate_claims(config: &Config, claims: &Value) -> Option<(String, Vec<String>)> {
    if claims
        .get("iss")
        .and_then(Value::as_str)?
        .trim_end_matches('/')
        != config.issuer_url.trim_end_matches('/')
    {
        return None;
    }
    if !audience_contains(claims.get("aud")?, &config.client_id) {
        return None;
    }
    let now = chrono::Utc::now().timestamp();
    if claims
        .get("exp")
        .and_then(Value::as_i64)
        .is_none_or(|exp| exp <= now)
    {
        return None;
    }
    if claims
        .get("nbf")
        .and_then(Value::as_i64)
        .is_some_and(|nbf| nbf > now)
    {
        return None;
    }
    for (name, expected) in &config.required_claims {
        if claims.get(name).and_then(Value::as_str) != Some(expected.as_str()) {
            return None;
        }
    }

    let claim = if config.username_claim.is_empty() {
        DEFAULT_USERNAME_CLAIM
    } else {
        config.username_claim.as_str()
    };
    let raw_username = claims.get(claim).and_then(Value::as_str)?;
    if raw_username.is_empty() {
        return None;
    }
    let username_prefix = config
        .username_prefix
        .clone()
        .unwrap_or_else(|| format!("{}#", config.issuer_url.trim_end_matches('/')));
    let username = format!("{username_prefix}{raw_username}");
    let groups = match &config.groups_claim {
        Some(claim) => claims
            .get(claim)?
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .map(|group| format!("{}{group}", config.groups_prefix.as_deref().unwrap_or("")))
            .collect(),
        None => Vec::new(),
    };
    Some((username, groups))
}

fn audience_contains(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value == expected,
        Value::Array(values) => values.iter().any(|value| value.as_str() == Some(expected)),
        _ => false,
    }
}

fn decode_segment(segment: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(segment)
        .ok()
}

fn decode_field(value: &str) -> Result<Vec<u8>, ()> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{signature::Signer, SigningKey};
    use p256::elliptic_curve::rand_core::OsRng;

    fn config() -> Config {
        Config {
            issuer_url: "https://issuer.example".to_string(),
            client_id: "not-k8s".to_string(),
            username_claim: "sub".to_string(),
            username_prefix: Some("oidc:".to_string()),
            groups_claim: Some("groups".to_string()),
            groups_prefix: Some("oidc:".to_string()),
            required_claims: vec![("tenant".to_string(), "edge".to_string())],
            signing_algs: vec!["ES256".to_string()],
            ca_certificate_pem: None,
        }
    }

    fn token(signing_key: &SigningKey, claims: Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"alg":"ES256","kid":"test","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        let input = format!("{header}.{payload}");
        let signature: Signature = signing_key.sign(input.as_bytes());
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes());
        format!("{input}.{encoded}")
    }

    #[test]
    fn claims_apply_kubernetes_oidc_identity_rules() {
        let config = config();
        let claims = serde_json::json!({
            "iss": config.issuer_url,
            "aud": ["not-k8s"],
            "sub": "alice",
            "groups": ["developers", "ops"],
            "tenant": "edge",
            "exp": chrono::Utc::now().timestamp() + 60
        });
        let (username, groups) = validate_claims(&config, &claims).expect("claims should validate");
        assert_eq!(username, "oidc:alice");
        assert_eq!(groups, vec!["oidc:developers", "oidc:ops"]);
    }

    #[test]
    fn wrong_audience_or_required_claim_is_rejected() {
        let config = config();
        let claims = serde_json::json!({
            "iss": config.issuer_url,
            "aud": "other",
            "sub": "alice",
            "tenant": "wrong",
            "exp": chrono::Utc::now().timestamp() + 60
        });
        assert!(validate_claims(&config, &claims).is_none());
    }

    #[test]
    fn es256_jwks_key_verifies_a_real_jws_signature() {
        let signing_key = SigningKey::random(&mut OsRng);
        let token = token(
            &signing_key,
            serde_json::json!({
                "iss": "https://issuer.example",
                "aud": "not-k8s",
                "sub": "alice",
                "tenant": "edge",
                "exp": chrono::Utc::now().timestamp() + 60
            }),
        );
        let parsed = ParsedToken::parse(&token).unwrap();
        let point = signing_key.verifying_key().to_encoded_point(false);
        let key = JsonWebKey {
            kty: "EC".to_string(),
            kid: Some("test".to_string()),
            alg: Some("ES256".to_string()),
            crv: Some("P-256".to_string()),
            x: Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(point.x().unwrap())),
            y: Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(point.y().unwrap())),
            n: None,
            e: None,
        };
        verify_signature(
            &key,
            "ES256",
            parsed.signing_input.as_bytes(),
            &parsed.signature,
        )
        .unwrap();
    }
}
