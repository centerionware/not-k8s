//! Configuration, from the environment — the same convention every other
//! component in this workspace uses, so the combined `notk8s` binary needs
//! no per-component config mechanism. Minimal today (Group A has no
//! listener and no storage client yet); grows with each group that needs a
//! new setting rather than being designed up front.

use anyhow::{anyhow, Result};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    /// Where the REST/watch API will be served once Group E lands. Not yet
    /// bound to anything — `run()` only logs it today.
    pub bind_addr: String,
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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind_addr: "0.0.0.0:6443".to_string(),
            nodestore_endpoint: "https://127.0.0.1:2379".to_string(),
            nodestore_cert_file: None,
            nodestore_key_file: None,
            nodestore_ca_file: None,
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
}
