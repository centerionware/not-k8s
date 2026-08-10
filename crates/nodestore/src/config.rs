//! Configuration, from the environment — the same convention nodelet and
//! nodeproxy use, so the combined binary needs no per-component config
//! mechanism.

use crate::error::{Error, Result};
use std::path::PathBuf;
use tracing::{info, warn};

#[derive(Clone, Debug)]
pub struct Config {
    /// Where to serve the etcd v3 gRPC API. Defaults to etcd's own port on
    /// loopback: a control plane talks to its datastore over localhost, and a
    /// datastore reachable from the network with no authentication is a
    /// cluster takeover.
    pub listen: String,
    pub data_dir: PathBuf,
    /// Revisions to keep behind the current one when auto-compacting. etcd's
    /// own default retention is 1000 revisions; apiserver's watch cache
    /// tolerates far less, but a larger window is what lets a watcher that
    /// reconnects after a blip resume instead of re-listing.
    pub compact_retain_revisions: i64,
    /// How often to auto-compact. Zero disables it, leaving compaction to
    /// whoever calls the Compact RPC (apiserver does, on its own schedule).
    pub compact_interval_secs: u64,
    /// How often the leader checks for expired leases.
    pub lease_check_interval_secs: u64,
    /// Bound on the per-watcher event buffer. See watch.rs for why it is
    /// bounded at all.
    pub watch_buffer: usize,
    pub member_id: u64,
    pub cluster_id: u64,
    /// Future raft peers. Parsed and rejected today — see [`Config::from_env`].
    pub peers: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: "127.0.0.1:2379".to_string(),
            data_dir: PathBuf::from("/var/lib/nodestore"),
            compact_retain_revisions: 10_000,
            compact_interval_secs: 300,
            lease_check_interval_secs: 1,
            watch_buffer: 1024,
            member_id: 1,
            cluster_id: 1,
            peers: Vec::new(),
        }
    }
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("state.db")
    }

    pub fn from_env() -> Result<Config> {
        let mut cfg = Config::default();

        if let Ok(v) = std::env::var("NODESTORE_LISTEN") {
            if !v.is_empty() {
                cfg.listen = v;
            }
        }
        if let Ok(v) = std::env::var("NODESTORE_DATA_DIR") {
            if !v.is_empty() {
                cfg.data_dir = PathBuf::from(v);
            }
        }
        cfg.compact_retain_revisions =
            parse_env("NODESTORE_COMPACT_RETAIN_REVISIONS", cfg.compact_retain_revisions)?;
        cfg.compact_interval_secs =
            parse_env("NODESTORE_COMPACT_INTERVAL_SECS", cfg.compact_interval_secs)?;
        cfg.lease_check_interval_secs =
            parse_env("NODESTORE_LEASE_CHECK_INTERVAL_SECS", cfg.lease_check_interval_secs)?;
        cfg.watch_buffer = parse_env("NODESTORE_WATCH_BUFFER", cfg.watch_buffer)?;
        cfg.member_id = parse_env("NODESTORE_MEMBER_ID", cfg.member_id)?;
        cfg.cluster_id = parse_env("NODESTORE_CLUSTER_ID", cfg.cluster_id)?;

        if let Ok(v) = std::env::var("NODESTORE_PEERS") {
            cfg.peers = v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
        }
        // Accepted, parsed, and then refused — rather than ignored. A
        // datastore that silently ran as a single node while its operator
        // believed it was replicated is the worst possible way to discover
        // that raft isn't finished: everything works until the node dies, and
        // then the data was never anywhere else.
        if !cfg.peers.is_empty() {
            return Err(Error::InvalidRequest(format!(
                "NODESTORE_PEERS is set ({}), but multi-node raft replication is not implemented yet. \
                 This build runs as a single member only. Unset NODESTORE_PEERS to run single-node, \
                 or keep using etcd for a replicated control plane.",
                cfg.peers.join(",")
            )));
        }

        if cfg.compact_retain_revisions < 1 {
            return Err(Error::InvalidRequest(
                "NODESTORE_COMPACT_RETAIN_REVISIONS must be at least 1 — compacting up to the \
                 current revision would discard the history every live watcher is reading from"
                    .to_string(),
            ));
        }

        if cfg.listen.starts_with("0.0.0.0") || cfg.listen.starts_with("[::]") {
            // Not refused: a multi-node future needs a routable address, and
            // an operator behind a firewall may know exactly what they are
            // doing. But it is worth saying out loud.
            warn!(
                listen = %cfg.listen,
                "serving the datastore on all interfaces — there is no authentication on this API, \
                 so anything that can reach this port can read and write the entire cluster state"
            );
        }

        info!(
            listen = %cfg.listen,
            data_dir = %cfg.data_dir.display(),
            compact_interval_secs = cfg.compact_interval_secs,
            compact_retain_revisions = cfg.compact_retain_revisions,
            "nodestore configuration"
        );
        Ok(cfg)
    }
}

fn parse_env<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(v) if v.trim().is_empty() => Ok(default),
        Ok(v) => v.trim().parse::<T>().map_err(|_| {
            Error::InvalidRequest(format!("{name}={v:?} is not a valid value for this setting"))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env vars are process-global, so these run under one lock rather than
    // racing each other into flakes.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env<R>(vars: &[(&str, &str)], f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for (k, v) in vars {
            std::env::set_var(k, v);
        }
        let out = f();
        for (k, _) in vars {
            std::env::remove_var(k);
        }
        out
    }

    #[test]
    fn defaults_are_loopback_only() {
        let cfg = Config::default();
        assert!(cfg.listen.starts_with("127.0.0.1"), "must not default to a routable address");
    }

    #[test]
    fn db_path_lives_under_the_data_dir() {
        let mut cfg = Config::default();
        cfg.data_dir = PathBuf::from("/tmp/x");
        assert_eq!(cfg.db_path(), PathBuf::from("/tmp/x/state.db"));
    }

    #[test]
    fn peers_are_refused_rather_than_ignored() {
        // The failure this prevents is silent: an operator who thinks they
        // configured replication and finds out otherwise only when the node
        // dies.
        let err = with_env(&[("NODESTORE_PEERS", "10.0.0.2:2380,10.0.0.3:2380")], Config::from_env)
            .expect_err("peers must be refused while raft is unimplemented");
        let msg = err.to_string();
        assert!(msg.contains("not implemented yet"), "got: {msg}");
    }

    #[test]
    fn an_empty_peer_list_is_fine() {
        let cfg = with_env(&[("NODESTORE_PEERS", "")], Config::from_env).unwrap();
        assert!(cfg.peers.is_empty());
    }

    #[test]
    fn a_malformed_number_is_refused_with_the_variable_name() {
        let err = with_env(&[("NODESTORE_COMPACT_INTERVAL_SECS", "soon")], Config::from_env)
            .expect_err("must not fall back to the default silently");
        assert!(err.to_string().contains("NODESTORE_COMPACT_INTERVAL_SECS"));
    }

    #[test]
    fn zero_retention_is_refused() {
        let err = with_env(&[("NODESTORE_COMPACT_RETAIN_REVISIONS", "0")], Config::from_env)
            .expect_err("compacting to the current revision breaks live watchers");
        assert!(err.to_string().contains("at least 1"));
    }

    #[test]
    fn settings_come_from_the_environment() {
        let cfg = with_env(
            &[("NODESTORE_LISTEN", "127.0.0.1:12379"), ("NODESTORE_DATA_DIR", "/var/tmp/ns")],
            Config::from_env,
        )
        .unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:12379");
        assert_eq!(cfg.data_dir, PathBuf::from("/var/tmp/ns"));
    }
}
