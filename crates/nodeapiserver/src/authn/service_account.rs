//! ServiceAccount bearer-token authentication and TokenRequest issuance.
//!
//! The bootstrapper generates an EC P-256 PKCS#8 keypair for this purpose,
//! matching the ES256 JWT format accepted by Kubernetes clients. Tokens are
//! deliberately stateless here: the signed claims carry the ServiceAccount
//! identity and the apiserver verifies the signature, issuer, audience, and
//! lifetime on every request. The TokenRequest handler remains responsible
//! for looking up the ServiceAccount and its current UID before minting one.

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{Duration, Utc};
use p256::ecdsa::{
    signature::{Signer, Verifier},
    Signature, SigningKey,
};
use p256::pkcs8::DecodePrivateKey;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

const JWT_HEADER: &str = "eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9";
const DEFAULT_EXPIRATION_SECONDS: i64 = 3600;
const MIN_EXPIRATION_SECONDS: i64 = 600;

#[derive(Clone)]
pub struct Authenticator {
    signing_key: Arc<SigningKey>,
    issuer: String,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedToken {
    pub identity: crate::authn::x509::Identity,
    pub service_account_uid: String,
}

#[derive(Debug, Clone)]
pub struct IssuedToken {
    pub token: String,
    pub expiration_timestamp: String,
}

#[derive(Debug, Clone)]
pub struct TokenRequestSpec {
    pub audiences: Vec<String>,
    pub expiration_seconds: Option<i64>,
    pub bound_pod: Option<(String, String)>,
}

impl Authenticator {
    pub fn from_pem(path: &Path, issuer: impl Into<String>) -> Result<Self> {
        let issuer = issuer.into();
        anyhow::ensure!(
            !issuer.trim().is_empty(),
            "ServiceAccount token issuer must not be empty"
        );
        let pem = std::fs::read_to_string(path)
            .with_context(|| format!("reading ServiceAccount signing key {}", path.display()))?;
        let signing_key = SigningKey::from_pkcs8_pem(&pem)
            .with_context(|| format!("parsing ServiceAccount signing key {}", path.display()))?;
        Ok(Self {
            signing_key: Arc::new(signing_key),
            issuer,
        })
    }

    /// Mint a bound or unbound ServiceAccount token. The caller has already
    /// checked RBAC and fetched the ServiceAccount UID, so this function has
    /// no storage side effects and is safe to unit-test in isolation.
    pub fn issue_token(
        &self,
        namespace: &str,
        service_account: &str,
        service_account_uid: &str,
        request: &TokenRequestSpec,
    ) -> Result<IssuedToken> {
        anyhow::ensure!(
            !namespace.is_empty(),
            "TokenRequest namespace must not be empty"
        );
        anyhow::ensure!(
            !service_account.is_empty(),
            "TokenRequest ServiceAccount must not be empty"
        );
        anyhow::ensure!(
            !service_account_uid.is_empty(),
            "TokenRequest ServiceAccount has no UID"
        );
        let expiration_seconds = request
            .expiration_seconds
            .unwrap_or(DEFAULT_EXPIRATION_SECONDS);
        anyhow::ensure!(
            expiration_seconds >= MIN_EXPIRATION_SECONDS,
            "TokenRequest expirationSeconds must be at least {MIN_EXPIRATION_SECONDS}"
        );

        let now = Utc::now();
        let expiration = now + Duration::seconds(expiration_seconds);
        let audiences = if request.audiences.is_empty() {
            vec![self.issuer.clone()]
        } else {
            request.audiences.clone()
        };
        let subject = format!("system:serviceaccount:{namespace}:{service_account}");
        let mut kubernetes_claims = json!({
            "namespace": namespace,
            "serviceaccount": {
                "name": service_account,
                "uid": service_account_uid,
            },
        });
        if let Some((pod_name, pod_uid)) = &request.bound_pod {
            kubernetes_claims["pod"] = json!({"name": pod_name, "uid": pod_uid});
        }
        kubernetes_claims["warnafter"] = json!(expiration.timestamp() - 60);
        let claims = json!({
            "aud": audiences,
            "exp": expiration.timestamp(),
            "iat": now.timestamp(),
            "iss": &self.issuer,
            "jti": uuid::Uuid::new_v4().to_string(),
            "kubernetes.io": kubernetes_claims,
            "nbf": now.timestamp(),
            "sub": subject,
        });

        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
        let signing_input = format!("{JWT_HEADER}.{payload}");
        let signature: Signature = self.signing_key.sign(signing_input.as_bytes());
        let signature =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes());
        Ok(IssuedToken {
            token: format!("{signing_input}.{signature}"),
            expiration_timestamp: expiration.to_rfc3339(),
        })
    }

    /// Authenticate a bearer token and return the Kubernetes ServiceAccount
    /// identity it represents. Any malformed, expired, wrong-audience, or
    /// incorrectly signed token is rejected without exposing parser detail
    /// to the HTTP layer.
    pub fn authenticate(&self, token: &str) -> Option<AuthenticatedToken> {
        let mut parts = token.split('.');
        let header = parts.next()?;
        let payload = parts.next()?;
        let encoded_signature = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let header_json: Value = serde_json::from_slice(&decode_segment(header)?).ok()?;
        if header_json.get("alg").and_then(Value::as_str) != Some("ES256") {
            return None;
        }
        let payload_bytes = decode_segment(payload)?;
        let signature_bytes = decode_segment(encoded_signature)?;
        let signature = Signature::from_slice(&signature_bytes).ok()?;
        self.signing_key
            .verifying_key()
            .verify(format!("{header}.{payload}").as_bytes(), &signature)
            .ok()?;

        let claims: Value = serde_json::from_slice(&payload_bytes).ok()?;
        if claims.get("iss").and_then(Value::as_str) != Some(self.issuer.as_str()) {
            return None;
        }
        let now = Utc::now().timestamp();
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
        if !audience_contains(&claims, &self.issuer) {
            return None;
        }

        let subject = claims.get("sub").and_then(Value::as_str)?;
        let subject = subject.strip_prefix("system:serviceaccount:")?;
        let (namespace, service_account) = subject.split_once(':')?;
        if namespace.is_empty() || service_account.is_empty() || service_account.contains(':') {
            return None;
        }
        let service_account_claims = claims
            .pointer("/kubernetes.io/serviceaccount")
            .and_then(Value::as_object)?;
        if service_account_claims.get("name").and_then(Value::as_str) != Some(service_account)
            || claims
                .pointer("/kubernetes.io/namespace")
                .and_then(Value::as_str)
                != Some(namespace)
        {
            return None;
        }
        let uid = service_account_claims
            .get("uid")
            .and_then(Value::as_str)?
            .to_string();
        Some(AuthenticatedToken {
            identity: crate::authn::x509::Identity {
                name: format!("system:serviceaccount:{namespace}:{service_account}"),
                groups: vec![
                    "system:serviceaccounts".to_string(),
                    format!("system:serviceaccounts:{namespace}"),
                ],
                credential_id: (String::new(), Vec::new()),
            },
            service_account_uid: uid,
        })
    }
}

fn decode_segment(segment: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(segment)
        .ok()
}

fn audience_contains(claims: &Value, expected: &str) -> bool {
    match claims.get("aud") {
        Some(Value::String(audience)) => audience == expected,
        Some(Value::Array(audiences)) => audiences
            .iter()
            .any(|audience| audience.as_str() == Some(expected)),
        _ => false,
    }
}

pub fn parse_token_request(body: &Value) -> std::result::Result<TokenRequestSpec, String> {
    let spec = body
        .get("spec")
        .and_then(Value::as_object)
        .ok_or_else(|| "TokenRequest.spec is required".to_string())?;
    let audiences = match spec.get("audiences") {
        None => Vec::new(),
        Some(Value::Array(values)) => values
            .iter()
            .map(|audience| {
                audience
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        "TokenRequest.spec.audiences must contain non-empty strings".to_string()
                    })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?,
        Some(_) => return Err("TokenRequest.spec.audiences must be an array".to_string()),
    };
    let expiration_seconds =
        match spec.get("expirationSeconds") {
            None => None,
            Some(value) => Some(value.as_i64().ok_or_else(|| {
                "TokenRequest.spec.expirationSeconds must be an integer".to_string()
            })?),
        };
    let bound_pod = match spec.get("boundObjectRef") {
        None | Some(Value::Null) => None,
        Some(Value::Object(reference)) => {
            if reference.get("kind").and_then(Value::as_str) != Some("Pod") {
                return Err("TokenRequest.spec.boundObjectRef.kind must be Pod".to_string());
            }
            let name = reference
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| "TokenRequest.spec.boundObjectRef.name is required".to_string())?;
            let uid = reference
                .get("uid")
                .and_then(Value::as_str)
                .filter(|uid| !uid.is_empty())
                .ok_or_else(|| "TokenRequest.spec.boundObjectRef.uid is required".to_string())?;
            Some((name.to_string(), uid.to_string()))
        }
        Some(_) => {
            return Err("TokenRequest.spec.boundObjectRef must be an object".to_string());
        }
    };
    Ok(TokenRequestSpec {
        audiences,
        expiration_seconds,
        bound_pod,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::pkcs8::EncodePrivateKey;

    fn authenticator() -> Authenticator {
        let key = p256::SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let pem = key.to_pkcs8_pem(Default::default()).unwrap();
        let path = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(path.path(), pem.as_bytes()).unwrap();
        Authenticator::from_pem(path.path(), "https://kubernetes.default.svc.cluster.local")
            .unwrap()
    }

    #[test]
    fn issued_tokens_round_trip_to_a_service_account_identity() {
        let authenticator = authenticator();
        let issued = authenticator
            .issue_token(
                "kube-system",
                "coredns",
                "sa-uid",
                &TokenRequestSpec {
                    audiences: Vec::new(),
                    expiration_seconds: Some(600),
                    bound_pod: Some(("coredns-0".to_string(), "pod-uid".to_string())),
                },
            )
            .unwrap();
        let authenticated = authenticator.authenticate(&issued.token).unwrap();
        assert_eq!(
            authenticated.identity.name,
            "system:serviceaccount:kube-system:coredns"
        );
        assert_eq!(authenticated.service_account_uid, "sa-uid");
        assert!(issued.expiration_timestamp.contains('T'));
    }

    #[test]
    fn altered_tokens_and_wrong_audiences_are_rejected() {
        let authenticator = authenticator();
        let issued = authenticator
            .issue_token(
                "default",
                "default",
                "sa-uid",
                &TokenRequestSpec {
                    audiences: vec!["https://other.example".to_string()],
                    expiration_seconds: Some(600),
                    bound_pod: None,
                },
            )
            .unwrap();
        assert!(authenticator.authenticate(&issued.token).is_none());
        let mut altered = issued.token.into_bytes();
        let last = altered.len() - 1;
        altered[last] = if altered[last] == b'A' { b'B' } else { b'A' };
        assert!(authenticator
            .authenticate(std::str::from_utf8(&altered).unwrap())
            .is_none());
    }

    #[test]
    fn token_request_requires_a_pod_kind_for_bound_tokens() {
        let body = json!({
            "spec": {
                "boundObjectRef": {
                    "kind": "Secret",
                    "name": "x",
                    "uid": "y"
                }
            }
        });
        assert!(parse_token_request(&body).is_err());
    }
}
