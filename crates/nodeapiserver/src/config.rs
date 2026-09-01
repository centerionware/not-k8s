//! Configuration, from the environment — the same convention every other
//! component in this workspace uses, so the combined `notk8s` binary needs
//! no per-component config mechanism.

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_AUTHORIZATION_WEBHOOK_AUTHORIZED_TTL: Duration = Duration::from_secs(5 * 60);
const DEFAULT_AUTHORIZATION_WEBHOOK_UNAUTHORIZED_TTL: Duration = Duration::from_secs(30);
const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 3 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Config {
    /// Where the REST/watch API is served — `server::listener::run` binds
    /// this directly.
    pub bind_addr: String,
    /// Optional PEM serving certificate and PKCS#8 private key. Bootstrap
    /// supplies the cluster-CA-signed pair so kubeconfigs and in-cluster
    /// clients trust the replacement apiserver. When unset, the standalone
    /// binary keeps its persisted self-signed development certificate.
    pub tls_cert_file: Option<PathBuf>,
    pub tls_key_file: Option<PathBuf>,
    /// The nodestore etcd v3 endpoint this apiserver is a client of (Group
    /// C). Loopback by default: a control-plane component talks to its
    /// datastore over localhost, the same posture `nodestore`'s own
    /// `NODESTORE_LISTEN` default takes.
    pub nodestore_endpoint: String,
    /// TLS material for the nodestore client connection. nodestore's client
    /// API has no plaintext mode (see `crate::config`'s own doc comment on
    /// that crate), so all three are required together, mirroring
    /// nodestore's own `NODESTORE_CERT_FILE`/`_KEY_FILE`/`_TRUSTED_CA_FILE`
    /// naming — an operator who already has nodestore's own client cert
    /// material provisioned should recognize the shape immediately.
    pub nodestore_cert_file: Option<PathBuf>,
    pub nodestore_key_file: Option<PathBuf>,
    pub nodestore_ca_file: Option<PathBuf>,
    /// A CA bundle to verify *incoming* client certificates against
    /// (Group H's x509 authenticator, `authn::x509`). `None` (the
    /// default) means the listener offers no client certificate
    /// authentication at all — `with_no_client_auth()`, same as before
    /// this setting existed. Same "optional, offered-not-required"
    /// posture `crates/nodelet/src/config.rs`'s own `NODELET_CLIENT_CA_FILE`
    /// already established: a client presenting a cert that chains to
    /// this CA gets a verified identity, a client presenting none still
    /// completes the handshake (falls back to whatever other
    /// authentication this build eventually supports), and a client
    /// presenting a cert that does *not* chain to this CA fails the TLS
    /// handshake outright.
    pub client_ca_file: Option<PathBuf>,
    /// `NODEAPISERVER_SERVICE_ACCOUNT_SIGNING_KEY_FILE` and
    /// `NODEAPISERVER_SERVICE_ACCOUNT_ISSUER` configure the ServiceAccount
    /// JWT issuer used by TokenRequest/TokenReview. The bootstrapper points
    /// this at the cluster PKI's `sa.key`; when unset, ServiceAccount JWT
    /// authentication remains unavailable for standalone development runs.
    pub service_account_signing_key_file: Option<PathBuf>,
    pub service_account_issuer: String,
    /// `NODEAPISERVER_TOKEN_AUTH_FILE` points at the standard
    /// `--token-auth-file` CSV used for static bootstrap bearer tokens.
    /// `None` leaves this authenticator disabled.
    pub bootstrap_token_file: Option<PathBuf>,
    /// `NODEAPISERVER_ANONYMOUS_AUTH` controls whether requests without a
    /// client certificate or bearer token authenticate as
    /// `system:anonymous`. It defaults to enabled, matching kube-apiserver's
    /// `--anonymous-auth=true` default.
    pub anonymous_auth: bool,
    /// OIDC bearer-token authentication. Both values must be configured to
    /// enable it; an absent pair leaves OIDC disabled.
    pub oidc_issuer_url: Option<String>,
    pub oidc_client_id: Option<String>,
    pub oidc_username_claim: String,
    pub oidc_username_prefix: Option<String>,
    pub oidc_groups_claim: Option<String>,
    pub oidc_groups_prefix: Option<String>,
    pub oidc_required_claims: Vec<(String, String)>,
    pub oidc_signing_algs: Vec<String>,
    pub oidc_ca_file: Option<PathBuf>,
    /// `NODEAPISERVER_PROXY_CLIENT_CERT_FILE`/`_KEY_FILE` are the
    /// front-proxy identity presented to aggregated API servers. The
    /// corresponding `X-Remote-User`/`X-Remote-Group` headers are generated
    /// from the authenticated request identity, never copied from the
    /// incoming request.
    pub proxy_client_cert_file: Option<PathBuf>,
    pub proxy_client_key_file: Option<PathBuf>,
    /// Optional `authorization.k8s.io/v1` SubjectAccessReview webhook.
    /// When set, requests are denied if the webhook denies them and return
    /// `503` if the webhook cannot be reached or returns an invalid review.
    pub authorization_webhook_url: Option<String>,
    /// Cache lifetimes for successful authorization-webhook decisions. A
    /// zero duration disables the corresponding cache, matching the
    /// upstream authorization configuration.
    pub authorization_webhook_authorized_ttl: Duration,
    pub authorization_webhook_unauthorized_ttl: Duration,
    /// APF's finite ordinary-request budgets. These mirror kube-apiserver's
    /// separate read and mutating request limits; the queue bound prevents
    /// unbounded memory growth while requests wait for a seat.
    pub apf_max_requests_inflight: usize,
    pub apf_max_mutating_requests_inflight: usize,
    pub apf_queue_length_limit: usize,
    /// Maximum request body accepted by the REST listener, matching
    /// kube-apiserver's default `--max-request-body-bytes` value.
    pub max_request_body_bytes: usize,
    /// `NODEAPISERVER_AUDIT_LOG_PATH` selects the append-only JSON-lines
    /// audit sink. The default remains the component's structured log
    /// output, while an explicit path mirrors kube-apiserver's
    /// `--audit-log-path` without changing the request event shape.
    pub audit_log_path: Option<PathBuf>,
    /// `NODEAPISERVER_AUDIT_LOG_MAX_SIZE_BYTES` rotates the file before a
    /// line would exceed this size. `NODEAPISERVER_AUDIT_LOG_MAX_BACKUPS`
    /// controls how many numbered backups are retained.
    pub audit_log_max_size_bytes: Option<u64>,
    pub audit_log_max_backups: usize,
    /// `NODEAPISERVER_AUDIT_WEBHOOK_URL` enables the asynchronous Kubernetes
    /// audit webhook backend. Events are sent as bounded `EventList` batches;
    /// delivery failures never block an API request.
    pub audit_webhook_url: Option<String>,
    /// `NODEAPISERVER_AUDIT_WEBHOOK_CONFIG_FILE` selects the standard
    /// kubeconfig-shaped audit webhook configuration. It supplies the
    /// endpoint and optional CA/client credentials.
    pub audit_webhook_config_file: Option<PathBuf>,
    /// `NODEAPISERVER_AUDIT_POLICY_FILE` selects an upstream-shaped
    /// `audit.k8s.io/v1` policy. When unset, every request keeps the existing
    /// metadata audit behavior.
    pub audit_policy_file: Option<PathBuf>,
    /// `NODEAPISERVER_ENFORCE_RBAC` — `false` by default, deliberately:
    /// enabling this makes `server::rest::get`/`list` deny-by-default
    /// against real `authz::resolve::rules_for` output (Group I), but
    /// Group O's cluster-bootstrap `system:` `ClusterRole`/
    /// `ClusterRoleBinding` set is supplied by nodebootstrap when the
    /// nodeapiserver target is selected. A standalone nodeapiserver still
    /// requires its operator to provision an administrative binding before
    /// enabling this, or every request can be denied with no path to grant
    /// access back.
    pub enforce_rbac: bool,
    /// `NODEAPISERVER_ENCRYPTION_CONFIG_FILE` — a real
    /// `apiserver.config.k8s.io/v1` `EncryptionConfiguration` YAML
    /// document (`storage::encryption_config::parse`, Group C). `None`
    /// (the default) means no encryption-at-rest. Loaded and validated
    /// at startup, then attached to `StorageClient`
    /// (`with_encryption`) — genuinely wired into every real
    /// `range`/`put`/`txn`/`watch` path now (`server::rest::
    /// decrypt_and_decode`/`encrypt_for_storage`), verified against a
    /// real live nodestore (`tests/encryption_roundtrip.rs`).
    pub encryption_config_file: Option<PathBuf>,
    /// `NODEAPISERVER_KUBELET_CLIENT_CERT_FILE`/`_KEY_FILE` — the
    /// client identity `proxy::client_tls` presents when dialing
    /// nodelet's own kubelet-style server for `pods/log` (Group N). The
    /// files may contain PEM (the bootstrapper's format) or raw DER (the
    /// nodelet runtime's persisted format). `None` (either unset, or only
    /// one of the pair set — same "both or neither" discipline the
    /// nodestore TLS triple already enforces) connects with no client
    /// identity at all, which only works against a nodelet that itself has
    /// no `NODELET_CLIENT_CA_FILE` configured (a real, named limitation:
    /// this build has no bearer-token credential nodelet's own
    /// `TokenReview` fallback path would accept).
    pub kubelet_client_cert_file: Option<PathBuf>,
    pub kubelet_client_key_file: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind_addr: "0.0.0.0:6443".to_string(),
            tls_cert_file: None,
            tls_key_file: None,
            nodestore_endpoint: "https://127.0.0.1:2379".to_string(),
            nodestore_cert_file: None,
            nodestore_key_file: None,
            nodestore_ca_file: None,
            client_ca_file: None,
            service_account_signing_key_file: None,
            service_account_issuer: "https://kubernetes.default.svc".to_string(),
            bootstrap_token_file: None,
            anonymous_auth: true,
            oidc_issuer_url: None,
            oidc_client_id: None,
            oidc_username_claim: "sub".to_string(),
            oidc_username_prefix: None,
            oidc_groups_claim: None,
            oidc_groups_prefix: None,
            oidc_required_claims: Vec::new(),
            oidc_signing_algs: vec!["RS256".to_string(), "PS256".to_string(), "ES256".to_string()],
            oidc_ca_file: None,
            proxy_client_cert_file: None,
            proxy_client_key_file: None,
            authorization_webhook_url: None,
            authorization_webhook_authorized_ttl: DEFAULT_AUTHORIZATION_WEBHOOK_AUTHORIZED_TTL,
            authorization_webhook_unauthorized_ttl: DEFAULT_AUTHORIZATION_WEBHOOK_UNAUTHORIZED_TTL,
            apf_max_requests_inflight: 400,
            apf_max_mutating_requests_inflight: 200,
            apf_queue_length_limit: 1000,
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
            audit_log_path: None,
            audit_log_max_size_bytes: None,
            audit_log_max_backups: 5,
            audit_webhook_url: None,
            audit_webhook_config_file: None,
            audit_policy_file: None,
            enforce_rbac: false,
            encryption_config_file: None,
            kubelet_client_cert_file: None,
            kubelet_client_key_file: None,
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Config> {
        let mut cfg = Config::default();
        if let Ok(v) = std::env::var("NODEAPISERVER_BIND_ADDR") {
            if !v.trim().is_empty() {
                cfg.bind_addr = v;
            }
        }
        cfg.tls_cert_file = path_env("NODEAPISERVER_TLS_CERT_FILE");
        cfg.tls_key_file = path_env("NODEAPISERVER_TLS_KEY_FILE");
        let tls_set = [cfg.tls_cert_file.is_some(), cfg.tls_key_file.is_some()];
        if tls_set[0] != tls_set[1] {
            return Err(anyhow!(
                "NODEAPISERVER_TLS_CERT_FILE and NODEAPISERVER_TLS_KEY_FILE must be set together or not at all"
            ));
        }
        if let Ok(v) = std::env::var("NODEAPISERVER_NODESTORE_ENDPOINT") {
            if !v.trim().is_empty() {
                cfg.nodestore_endpoint = v;
            }
        }
        if !cfg.nodestore_endpoint.starts_with("https://") {
            return Err(anyhow!(
                "NODEAPISERVER_NODESTORE_ENDPOINT={:?} must use https:// — nodestore's client API \
                 is a mutual-TLS listener with no plaintext mode",
                cfg.nodestore_endpoint
            ));
        }

        cfg.nodestore_cert_file = path_env("NODEAPISERVER_NODESTORE_CERT_FILE");
        cfg.nodestore_key_file = path_env("NODEAPISERVER_NODESTORE_KEY_FILE");
        cfg.nodestore_ca_file = path_env("NODEAPISERVER_NODESTORE_CA_FILE");
        let set = [cfg.nodestore_cert_file.is_some(), cfg.nodestore_key_file.is_some(), cfg.nodestore_ca_file.is_some()];
        if !(set.iter().all(|s| *s) || set.iter().all(|s| !*s)) {
            // Same "all three or none" discipline nodestore's own
            // check_tls_triple() enforces — a half-configured set is far
            // more likely an oversight than a request, and failing loudly
            // here beats a mysterious TLS handshake error at connect time.
            return Err(anyhow!(
                "NODEAPISERVER_NODESTORE_CERT_FILE, _KEY_FILE and _CA_FILE must be set together or \
                 not at all (set: cert={}, key={}, ca={})",
                set[0],
                set[1],
                set[2]
            ));
        }

        cfg.client_ca_file = path_env("NODEAPISERVER_CLIENT_CA_FILE");
        cfg.service_account_signing_key_file = path_env("NODEAPISERVER_SERVICE_ACCOUNT_SIGNING_KEY_FILE");
        cfg.bootstrap_token_file = path_env("NODEAPISERVER_TOKEN_AUTH_FILE");
        if let Ok(v) = std::env::var("NODEAPISERVER_SERVICE_ACCOUNT_ISSUER") {
            if !v.trim().is_empty() {
                cfg.service_account_issuer = v;
            }
        }
        anyhow::ensure!(
            !cfg.service_account_issuer.trim().is_empty(),
            "NODEAPISERVER_SERVICE_ACCOUNT_ISSUER must not be empty"
        );
        if let Ok(value) = std::env::var("NODEAPISERVER_ANONYMOUS_AUTH") {
            cfg.anonymous_auth = parse_bool("NODEAPISERVER_ANONYMOUS_AUTH", &value)?;
        }
        cfg.oidc_issuer_url = string_env("NODEAPISERVER_OIDC_ISSUER_URL");
        cfg.oidc_client_id = string_env("NODEAPISERVER_OIDC_CLIENT_ID");
        anyhow::ensure!(
            cfg.oidc_issuer_url.is_none() == cfg.oidc_client_id.is_none(),
            "NODEAPISERVER_OIDC_ISSUER_URL and NODEAPISERVER_OIDC_CLIENT_ID must be set together or not at all"
        );
        if let Some(value) = string_env("NODEAPISERVER_OIDC_USERNAME_CLAIM") {
            cfg.oidc_username_claim = value;
        }
        cfg.oidc_username_prefix = string_env("NODEAPISERVER_OIDC_USERNAME_PREFIX");
        cfg.oidc_groups_claim = string_env("NODEAPISERVER_OIDC_GROUPS_CLAIM");
        cfg.oidc_groups_prefix = string_env("NODEAPISERVER_OIDC_GROUPS_PREFIX");
        if let Some(value) = string_env("NODEAPISERVER_OIDC_REQUIRED_CLAIMS") {
            cfg.oidc_required_claims = parse_required_claims(&value)?;
        }
        if let Some(value) = string_env("NODEAPISERVER_OIDC_SIGNING_ALGS") {
            cfg.oidc_signing_algs = value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect();
            anyhow::ensure!(
                !cfg.oidc_signing_algs.is_empty(),
                "NODEAPISERVER_OIDC_SIGNING_ALGS must contain at least one algorithm"
            );
        }
        cfg.oidc_ca_file = path_env("NODEAPISERVER_OIDC_CA_FILE");
        cfg.proxy_client_cert_file = path_env("NODEAPISERVER_PROXY_CLIENT_CERT_FILE");
        cfg.proxy_client_key_file = path_env("NODEAPISERVER_PROXY_CLIENT_KEY_FILE");
        let proxy_client_set = [cfg.proxy_client_cert_file.is_some(), cfg.proxy_client_key_file.is_some()];
        if proxy_client_set[0] != proxy_client_set[1] {
            return Err(anyhow!(
                "NODEAPISERVER_PROXY_CLIENT_CERT_FILE and NODEAPISERVER_PROXY_CLIENT_KEY_FILE must be set together or not at all"
            ));
        }
        cfg.authorization_webhook_url = string_env("NODEAPISERVER_AUTHORIZATION_WEBHOOK_URL");
        cfg.authorization_webhook_authorized_ttl = duration_env(
            "NODEAPISERVER_AUTHORIZATION_WEBHOOK_CACHE_AUTHORIZED_TTL",
            cfg.authorization_webhook_authorized_ttl,
        )?;
        cfg.authorization_webhook_unauthorized_ttl = duration_env(
            "NODEAPISERVER_AUTHORIZATION_WEBHOOK_CACHE_UNAUTHORIZED_TTL",
            cfg.authorization_webhook_unauthorized_ttl,
        )?;
        cfg.apf_max_requests_inflight = usize_env(
            "NODEAPISERVER_APF_MAX_REQUESTS_INFLIGHT",
            cfg.apf_max_requests_inflight,
        )?;
        cfg.apf_max_mutating_requests_inflight = usize_env(
            "NODEAPISERVER_APF_MAX_MUTATING_REQUESTS_INFLIGHT",
            cfg.apf_max_mutating_requests_inflight,
        )?;
        cfg.apf_queue_length_limit = usize_env(
            "NODEAPISERVER_APF_QUEUE_LENGTH_LIMIT",
            cfg.apf_queue_length_limit,
        )?;
        cfg.max_request_body_bytes = usize_env(
            "NODEAPISERVER_MAX_REQUEST_BODY_BYTES",
            cfg.max_request_body_bytes,
        )?;
        cfg.audit_log_path = path_env("NODEAPISERVER_AUDIT_LOG_PATH");
        cfg.audit_log_max_size_bytes = optional_u64_env("NODEAPISERVER_AUDIT_LOG_MAX_SIZE_BYTES")?;
        cfg.audit_log_max_backups = usize_env(
            "NODEAPISERVER_AUDIT_LOG_MAX_BACKUPS",
            cfg.audit_log_max_backups,
        )?;
        cfg.audit_webhook_url = string_env("NODEAPISERVER_AUDIT_WEBHOOK_URL");
        cfg.audit_webhook_config_file = path_env("NODEAPISERVER_AUDIT_WEBHOOK_CONFIG_FILE");
        anyhow::ensure!(
            cfg.audit_webhook_url.is_none() || cfg.audit_webhook_config_file.is_none(),
            "NODEAPISERVER_AUDIT_WEBHOOK_URL and NODEAPISERVER_AUDIT_WEBHOOK_CONFIG_FILE are mutually exclusive"
        );
        cfg.audit_policy_file = path_env("NODEAPISERVER_AUDIT_POLICY_FILE");
        cfg.enforce_rbac = matches!(std::env::var("NODEAPISERVER_ENFORCE_RBAC").as_deref(), Ok("1") | Ok("true"));
        cfg.encryption_config_file = path_env("NODEAPISERVER_ENCRYPTION_CONFIG_FILE");
        cfg.kubelet_client_cert_file = path_env("NODEAPISERVER_KUBELET_CLIENT_CERT_FILE");
        cfg.kubelet_client_key_file = path_env("NODEAPISERVER_KUBELET_CLIENT_KEY_FILE");
        let kubelet_set = [cfg.kubelet_client_cert_file.is_some(), cfg.kubelet_client_key_file.is_some()];
        if kubelet_set[0] != kubelet_set[1] {
            return Err(anyhow!(
                "NODEAPISERVER_KUBELET_CLIENT_CERT_FILE and NODEAPISERVER_KUBELET_CLIENT_KEY_FILE must be set together or not at all"
            ));
        }
        Ok(cfg)
    }
}

/// An optional path from the environment. Empty means unset, matching
/// `nodestore`'s own `path_env()` convention (a variable set to `""`
/// behaves the same as one never exported, which is how systemd units and
/// shell wrappers tend to pass "no value").
fn path_env(name: &str) -> Option<PathBuf> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

fn string_env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

fn usize_env(name: &str, default: usize) -> Result<usize> {
    match std::env::var(name) {
        Ok(value) => {
            let parsed = value
                .parse::<usize>()
                .map_err(|error| anyhow!("{name} must be a positive integer: {error}"))?;
            anyhow::ensure!(parsed > 0, "{name} must be greater than zero");
            Ok(parsed)
        }
        Err(_) => Ok(default),
    }
}

fn optional_u64_env(name: &str) -> Result<Option<u64>> {
    let Ok(value) = std::env::var(name) else {
        return Ok(None);
    };
    let parsed = value
        .parse::<u64>()
        .map_err(|error| anyhow!("{name} must be a positive integer: {error}"))?;
    anyhow::ensure!(parsed > 0, "{name} must be greater than zero");
    Ok(Some(parsed))
}

fn duration_env(name: &str, default: Duration) -> Result<Duration> {
    let Ok(value) = std::env::var(name) else {
        return Ok(default);
    };
    parse_duration(name, &value)
}

fn parse_duration(name: &str, value: &str) -> Result<Duration> {
    let value = value.trim();
    let (number, unit) = ["ns", "us", "µs", "ms", "s", "m", "h"]
        .iter()
        .find_map(|unit| value.strip_suffix(unit).map(|number| (number, *unit)))
        .ok_or_else(|| anyhow!("{name} must be a duration such as 30s or 5m"))?;
    let number = number
        .parse::<u64>()
        .map_err(|error| anyhow!("{name} has an invalid duration {value:?}: {error}"))?;
    let nanos = match unit {
        "ns" => number,
        "us" | "µs" => number.saturating_mul(1_000),
        "ms" => number.saturating_mul(1_000_000),
        "s" => number.saturating_mul(1_000_000_000),
        "m" => number.saturating_mul(60 * 1_000_000_000),
        "h" => number.saturating_mul(60 * 60 * 1_000_000_000),
        _ => unreachable!("the suffix list above is exhaustive"),
    };
    Ok(Duration::from_nanos(nanos))
}

fn parse_bool(name: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        _ => Err(anyhow!("{name} must be one of true/false or 1/0")),
    }
}

fn parse_required_claims(value: &str) -> Result<Vec<(String, String)>> {
    value
        .split(',')
        .map(|entry| {
            let (name, expected) = entry
                .split_once('=')
                .ok_or_else(|| anyhow!("OIDC required claim {entry:?} must use name=value"))?;
            anyhow::ensure!(!name.trim().is_empty(), "OIDC required claim name must not be empty");
            anyhow::ensure!(!expected.is_empty(), "OIDC required claim value must not be empty");
            Ok((name.trim().to_string(), expected.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard<'a>(&'a [&'a str]);
    impl Drop for EnvGuard<'_> {
        fn drop(&mut self) {
            for k in self.0 {
                std::env::remove_var(k);
            }
        }
    }

    #[test]
    fn defaults_use_a_real_bind_addr_and_https_nodestore_endpoint() {
        let cfg = Config::default();
        assert!(cfg.bind_addr.contains(':'));
        assert!(cfg.nodestore_endpoint.starts_with("https://"));
        assert_eq!(cfg.apf_max_requests_inflight, 400);
        assert_eq!(cfg.apf_max_mutating_requests_inflight, 200);
        assert_eq!(cfg.apf_queue_length_limit, 1000);
        assert_eq!(cfg.max_request_body_bytes, 3 * 1024 * 1024);
        assert!(cfg.audit_log_path.is_none());
        assert_eq!(cfg.audit_log_max_size_bytes, None);
        assert_eq!(cfg.audit_log_max_backups, 5);
        assert!(cfg.audit_webhook_url.is_none());
        assert!(cfg.audit_webhook_config_file.is_none());
        assert!(cfg.audit_policy_file.is_none());
    }

    #[test]
    fn apf_limits_are_read_and_validated_from_environment() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_APF_MAX_REQUESTS_INFLIGHT", "17");
        std::env::set_var("NODEAPISERVER_APF_MAX_MUTATING_REQUESTS_INFLIGHT", "9");
        std::env::set_var("NODEAPISERVER_APF_QUEUE_LENGTH_LIMIT", "31");
        std::env::set_var("NODEAPISERVER_MAX_REQUEST_BODY_BYTES", "8192");
        std::env::set_var("NODEAPISERVER_AUDIT_LOG_PATH", "/tmp/nodeapiserver-audit.log");
        std::env::set_var("NODEAPISERVER_AUDIT_LOG_MAX_SIZE_BYTES", "4096");
        std::env::set_var("NODEAPISERVER_AUDIT_LOG_MAX_BACKUPS", "3");
        std::env::set_var("NODEAPISERVER_AUDIT_WEBHOOK_URL", "http://127.0.0.1:9000/audit");
        std::env::set_var("NODEAPISERVER_AUDIT_POLICY_FILE", "/tmp/nodeapiserver-audit-policy.yaml");
        let _cleanup = EnvGuard(&[
            "NODEAPISERVER_APF_MAX_REQUESTS_INFLIGHT",
            "NODEAPISERVER_APF_MAX_MUTATING_REQUESTS_INFLIGHT",
            "NODEAPISERVER_APF_QUEUE_LENGTH_LIMIT",
            "NODEAPISERVER_MAX_REQUEST_BODY_BYTES",
            "NODEAPISERVER_AUDIT_LOG_PATH",
            "NODEAPISERVER_AUDIT_LOG_MAX_SIZE_BYTES",
            "NODEAPISERVER_AUDIT_LOG_MAX_BACKUPS",
            "NODEAPISERVER_AUDIT_WEBHOOK_URL",
            "NODEAPISERVER_AUDIT_WEBHOOK_CONFIG_FILE",
            "NODEAPISERVER_AUDIT_POLICY_FILE",
        ]);
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.apf_max_requests_inflight, 17);
        assert_eq!(cfg.apf_max_mutating_requests_inflight, 9);
        assert_eq!(cfg.apf_queue_length_limit, 31);
        assert_eq!(cfg.max_request_body_bytes, 8192);
        assert_eq!(cfg.audit_log_path.as_deref(), Some(std::path::Path::new("/tmp/nodeapiserver-audit.log")));
        assert_eq!(cfg.audit_log_max_size_bytes, Some(4096));
        assert_eq!(cfg.audit_log_max_backups, 3);
        assert_eq!(cfg.audit_webhook_url.as_deref(), Some("http://127.0.0.1:9000/audit"));
        assert_eq!(cfg.audit_policy_file.as_deref(), Some(std::path::Path::new("/tmp/nodeapiserver-audit-policy.yaml")));
    }

    #[test]
    fn zero_apf_limit_is_refused() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_APF_MAX_REQUESTS_INFLIGHT", "0");
        let _cleanup = EnvGuard(&["NODEAPISERVER_APF_MAX_REQUESTS_INFLIGHT"]);
        let err = Config::from_env().expect_err("zero request budget must be refused");
        assert!(err.to_string().contains("greater than zero"));
    }

    #[test]
    fn a_plaintext_nodestore_endpoint_is_refused() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_NODESTORE_ENDPOINT", "http://127.0.0.1:2379");
        let _cleanup = EnvGuard(&["NODEAPISERVER_NODESTORE_ENDPOINT"]);
        let err = Config::from_env().expect_err("plaintext endpoint must be refused");
        assert!(err.to_string().contains("https://"));
    }

    #[test]
    fn no_tls_material_configured_means_all_three_are_none() {
        let cfg = Config::default();
        assert!(cfg.nodestore_cert_file.is_none());
        assert!(cfg.nodestore_key_file.is_none());
        assert!(cfg.nodestore_ca_file.is_none());
    }

    #[test]
    fn a_half_configured_tls_triple_is_refused() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_NODESTORE_CERT_FILE", "/tmp/cert.pem");
        let _cleanup = EnvGuard(&["NODEAPISERVER_NODESTORE_CERT_FILE"]);
        let err = Config::from_env().expect_err("cert alone, without key and ca, must be refused");
        assert!(err.to_string().contains("must be set together"));
    }

    #[test]
    fn a_complete_tls_triple_is_accepted() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_NODESTORE_CERT_FILE", "/tmp/cert.pem");
        std::env::set_var("NODEAPISERVER_NODESTORE_KEY_FILE", "/tmp/key.pem");
        std::env::set_var("NODEAPISERVER_NODESTORE_CA_FILE", "/tmp/ca.pem");
        let _cleanup = EnvGuard(&[
            "NODEAPISERVER_NODESTORE_CERT_FILE",
            "NODEAPISERVER_NODESTORE_KEY_FILE",
            "NODEAPISERVER_NODESTORE_CA_FILE",
        ]);
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.nodestore_cert_file, Some(PathBuf::from("/tmp/cert.pem")));
    }

    #[test]
    fn client_ca_file_defaults_to_none_and_is_independent_of_the_nodestore_tls_triple() {
        let cfg = Config::default();
        assert_eq!(cfg.client_ca_file, None);
    }

    #[test]
    fn client_ca_file_is_read_from_its_own_env_var() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_CLIENT_CA_FILE", "/tmp/client-ca.pem");
        let _cleanup = EnvGuard(&["NODEAPISERVER_CLIENT_CA_FILE"]);
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.client_ca_file, Some(PathBuf::from("/tmp/client-ca.pem")));
    }

    #[test]
    fn service_account_signing_key_and_issuer_are_read_from_their_own_env_vars() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_SERVICE_ACCOUNT_SIGNING_KEY_FILE", "/tmp/sa.key");
        std::env::set_var("NODEAPISERVER_SERVICE_ACCOUNT_ISSUER", "https://kubernetes.default.svc.example");
        let _cleanup = EnvGuard(&[
            "NODEAPISERVER_SERVICE_ACCOUNT_SIGNING_KEY_FILE",
            "NODEAPISERVER_SERVICE_ACCOUNT_ISSUER",
        ]);
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.service_account_signing_key_file, Some(PathBuf::from("/tmp/sa.key")));
        assert_eq!(cfg.service_account_issuer, "https://kubernetes.default.svc.example");
    }

    #[test]
    fn static_token_auth_file_is_read_from_its_own_env_var() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_TOKEN_AUTH_FILE", "/tmp/tokens.csv");
        let _cleanup = EnvGuard(&["NODEAPISERVER_TOKEN_AUTH_FILE"]);
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.bootstrap_token_file, Some(PathBuf::from("/tmp/tokens.csv")));
    }

    #[test]
    fn encryption_config_file_defaults_to_none_and_is_read_from_its_own_env_var() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(Config::default().encryption_config_file, None);
        std::env::set_var("NODEAPISERVER_ENCRYPTION_CONFIG_FILE", "/tmp/encryption-config.yaml");
        let _cleanup = EnvGuard(&["NODEAPISERVER_ENCRYPTION_CONFIG_FILE"]);
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.encryption_config_file, Some(PathBuf::from("/tmp/encryption-config.yaml")));
    }

    #[test]
    fn kubelet_client_cert_key_default_to_none_and_are_read_from_their_own_env_vars() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(Config::default().kubelet_client_cert_file, None);
        assert_eq!(Config::default().kubelet_client_key_file, None);
        std::env::set_var("NODEAPISERVER_KUBELET_CLIENT_CERT_FILE", "/tmp/kubelet-client.der");
        std::env::set_var("NODEAPISERVER_KUBELET_CLIENT_KEY_FILE", "/tmp/kubelet-client-key.der");
        let _cleanup = EnvGuard(&["NODEAPISERVER_KUBELET_CLIENT_CERT_FILE", "NODEAPISERVER_KUBELET_CLIENT_KEY_FILE"]);
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.kubelet_client_cert_file, Some(PathBuf::from("/tmp/kubelet-client.der")));
        assert_eq!(cfg.kubelet_client_key_file, Some(PathBuf::from("/tmp/kubelet-client-key.der")));
    }

    #[test]
    fn proxy_client_cert_key_are_read_as_a_complete_pair() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_PROXY_CLIENT_CERT_FILE", "/tmp/front-proxy.crt");
        std::env::set_var("NODEAPISERVER_PROXY_CLIENT_KEY_FILE", "/tmp/front-proxy.key");
        let _cleanup = EnvGuard(&[
            "NODEAPISERVER_PROXY_CLIENT_CERT_FILE",
            "NODEAPISERVER_PROXY_CLIENT_KEY_FILE",
        ]);
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.proxy_client_cert_file, Some(PathBuf::from("/tmp/front-proxy.crt")));
        assert_eq!(cfg.proxy_client_key_file, Some(PathBuf::from("/tmp/front-proxy.key")));
    }

    #[test]
    fn a_half_configured_proxy_client_pair_is_refused() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_PROXY_CLIENT_CERT_FILE", "/tmp/front-proxy.crt");
        std::env::remove_var("NODEAPISERVER_PROXY_CLIENT_KEY_FILE");
        let _cleanup = EnvGuard(&[
            "NODEAPISERVER_PROXY_CLIENT_CERT_FILE",
            "NODEAPISERVER_PROXY_CLIENT_KEY_FILE",
        ]);
        assert!(Config::from_env().is_err());
    }

    #[test]
    fn a_half_configured_kubelet_client_pair_is_refused() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_KUBELET_CLIENT_CERT_FILE", "/tmp/kubelet-client.pem");
        let _cleanup = EnvGuard(&["NODEAPISERVER_KUBELET_CLIENT_CERT_FILE"]);
        let err = Config::from_env().expect_err("a kubelet cert without its key must be refused");
        assert!(err.to_string().contains("must be set together"));
    }

    #[test]
    fn enforce_rbac_defaults_to_false() {
        assert!(!Config::default().enforce_rbac);
    }

    #[test]
    fn anonymous_auth_defaults_to_true_and_accepts_standard_boolean_values() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(Config::default().anonymous_auth);
        for value in ["0", "false", "no"] {
            std::env::set_var("NODEAPISERVER_ANONYMOUS_AUTH", value);
            let _cleanup = EnvGuard(&["NODEAPISERVER_ANONYMOUS_AUTH"]);
            assert!(!Config::from_env().unwrap().anonymous_auth, "{value:?} should disable anonymous auth");
        }
        for value in ["1", "true", "yes"] {
            std::env::set_var("NODEAPISERVER_ANONYMOUS_AUTH", value);
            let _cleanup = EnvGuard(&["NODEAPISERVER_ANONYMOUS_AUTH"]);
            assert!(Config::from_env().unwrap().anonymous_auth, "{value:?} should enable anonymous auth");
        }
    }

    #[test]
    fn anonymous_auth_rejects_invalid_values() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_ANONYMOUS_AUTH", "sometimes");
        let _cleanup = EnvGuard(&["NODEAPISERVER_ANONYMOUS_AUTH"]);
        let error = Config::from_env().expect_err("invalid anonymous-auth value must be rejected");
        assert!(error.to_string().contains("NODEAPISERVER_ANONYMOUS_AUTH"));
    }

    #[test]
    fn enforce_rbac_is_enabled_by_1_or_true() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for value in ["1", "true"] {
            std::env::set_var("NODEAPISERVER_ENFORCE_RBAC", value);
            let _cleanup = EnvGuard(&["NODEAPISERVER_ENFORCE_RBAC"]);
            assert!(Config::from_env().unwrap().enforce_rbac, "{value:?} should enable enforcement");
        }
    }

    #[test]
    fn enforce_rbac_rejects_anything_else_as_off() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_ENFORCE_RBAC", "yes");
        let _cleanup = EnvGuard(&["NODEAPISERVER_ENFORCE_RBAC"]);
        assert!(!Config::from_env().unwrap().enforce_rbac, "only the literal 1/true should enable it");
    }

    #[test]
    fn oidc_requires_issuer_and_client_id_together() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_OIDC_ISSUER_URL", "https://issuer.example");
        let _cleanup = EnvGuard(&["NODEAPISERVER_OIDC_ISSUER_URL"]);
        let err = Config::from_env().expect_err("OIDC must not accept a half-configured pair");
        assert!(err.to_string().contains("set together"));
    }

    #[test]
    fn oidc_options_are_read_and_required_claims_are_parsed() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_OIDC_ISSUER_URL", "https://issuer.example");
        std::env::set_var("NODEAPISERVER_OIDC_CLIENT_ID", "not-k8s");
        std::env::set_var("NODEAPISERVER_OIDC_USERNAME_CLAIM", "email");
        std::env::set_var("NODEAPISERVER_OIDC_GROUPS_CLAIM", "groups");
        std::env::set_var("NODEAPISERVER_OIDC_REQUIRED_CLAIMS", "tenant=edge,environment=test");
        std::env::set_var("NODEAPISERVER_OIDC_SIGNING_ALGS", "RS256, ES256");
        let _cleanup = EnvGuard(&[
            "NODEAPISERVER_OIDC_ISSUER_URL",
            "NODEAPISERVER_OIDC_CLIENT_ID",
            "NODEAPISERVER_OIDC_USERNAME_CLAIM",
            "NODEAPISERVER_OIDC_GROUPS_CLAIM",
            "NODEAPISERVER_OIDC_REQUIRED_CLAIMS",
            "NODEAPISERVER_OIDC_SIGNING_ALGS",
        ]);
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.oidc_issuer_url.as_deref(), Some("https://issuer.example"));
        assert_eq!(cfg.oidc_client_id.as_deref(), Some("not-k8s"));
        assert_eq!(cfg.oidc_username_claim, "email");
        assert_eq!(cfg.oidc_groups_claim.as_deref(), Some("groups"));
        assert_eq!(
            cfg.oidc_required_claims,
            vec![
                ("tenant".to_string(), "edge".to_string()),
                ("environment".to_string(), "test".to_string()),
            ]
        );
        assert_eq!(cfg.oidc_signing_algs, vec!["RS256", "ES256"]);
    }

    #[test]
    fn oidc_required_claims_reject_malformed_entries() {
        let err = parse_required_claims("tenant").expect_err("a claim must include its value");
        assert!(err.to_string().contains("name=value"));
    }

    #[test]
    fn authorization_webhook_url_is_read_from_its_own_env_var() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(
            "NODEAPISERVER_AUTHORIZATION_WEBHOOK_URL",
            "https://authz.example/review",
        );
        let _cleanup = EnvGuard(&["NODEAPISERVER_AUTHORIZATION_WEBHOOK_URL"]);
        let cfg = Config::from_env().unwrap();
        assert_eq!(
            cfg.authorization_webhook_url.as_deref(),
            Some("https://authz.example/review")
        );
    }

    #[test]
    fn authorization_webhook_cache_ttls_are_read_from_their_own_env_vars() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(
            "NODEAPISERVER_AUTHORIZATION_WEBHOOK_CACHE_AUTHORIZED_TTL",
            "2m",
        );
        std::env::set_var(
            "NODEAPISERVER_AUTHORIZATION_WEBHOOK_CACHE_UNAUTHORIZED_TTL",
            "250ms",
        );
        let _cleanup = EnvGuard(&[
            "NODEAPISERVER_AUTHORIZATION_WEBHOOK_CACHE_AUTHORIZED_TTL",
            "NODEAPISERVER_AUTHORIZATION_WEBHOOK_CACHE_UNAUTHORIZED_TTL",
        ]);
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.authorization_webhook_authorized_ttl, Duration::from_secs(120));
        assert_eq!(cfg.authorization_webhook_unauthorized_ttl, Duration::from_millis(250));
    }

    #[test]
    fn authorization_webhook_cache_ttl_rejects_missing_units() {
        assert!(parse_duration("CACHE_TTL", "30").is_err());
        assert!(parse_duration("CACHE_TTL", "-1s").is_err());
    }

    #[test]
    fn audit_webhook_config_file_is_read_and_cannot_be_combined_with_a_url() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NODEAPISERVER_AUDIT_WEBHOOK_CONFIG_FILE", "/etc/nodeapiserver/audit.yaml");
        let _cleanup = EnvGuard(&[
            "NODEAPISERVER_AUDIT_WEBHOOK_CONFIG_FILE",
            "NODEAPISERVER_AUDIT_WEBHOOK_URL",
        ]);
        let cfg = Config::from_env().unwrap();
        assert_eq!(cfg.audit_webhook_config_file, Some(PathBuf::from("/etc/nodeapiserver/audit.yaml")));

        std::env::set_var("NODEAPISERVER_AUDIT_WEBHOOK_URL", "https://audit.example/events");
        assert!(Config::from_env().is_err());
    }
}
