//! Configuration, from the environment — the same convention every other
//! component in this workspace uses, so the combined `notk8s` binary needs
//! no per-component config mechanism. Minimal today (Group A has no
//! listener and no storage client yet); grows with each group that needs a
//! new setting rather than being designed up front.

use anyhow::{anyhow, Result};

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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind_addr: "0.0.0.0:6443".to_string(),
            nodestore_endpoint: "https://127.0.0.1:2379".to_string(),
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
        Ok(cfg)
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
}
