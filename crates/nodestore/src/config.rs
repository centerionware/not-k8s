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

    /// Where peers reach this member for raft traffic. Empty means single
    /// member: no peer server is started at all.
    pub peer_listen: String,
    /// This member's own peer URL, as other members should dial it. Defaults
    /// to whatever `initial_cluster` says for `member_id`.
    pub advertise_peer_url: String,
    /// This member's own client URL, published into the replicated address
    /// book so followers know where to forward writes.
    pub advertise_client_url: String,
    /// The cluster's initial membership: id → peer URL.
    pub initial_cluster: Vec<(u64, String)>,

    /// Ticks (of 100ms) without hearing from a leader before campaigning.
    ///
    /// etcd's defaults are 1000ms election / 100ms heartbeat, and the ratio
    /// matters more than either number: an election timeout under a few
    /// heartbeats turns ordinary jitter into a leadership change, and a
    /// cluster that re-elects under load is a cluster that stops serving
    /// writes under load.
    pub election_ticks: usize,
    pub heartbeat_ticks: usize,

    /// TLS material for the client API and for the raft peer link, kept in
    /// two separate trust domains. See [`crate::tls`] for why they are
    /// separate and why neither is optional.
    ///
    /// All six are unset by default, in which case the material is generated
    /// into `data_dir/pki/` on first start. Setting them is how an operator
    /// brings their own PKI; the names mirror etcd's own flags so an existing
    /// etcd deployment's certificates and automation carry over unchanged.
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
    pub trusted_ca_file: Option<PathBuf>,
    pub peer_cert_file: Option<PathBuf>,
    pub peer_key_file: Option<PathBuf>,
    pub peer_trusted_ca_file: Option<PathBuf>,
}

impl Config {
    /// Whether this member is part of a multi-member cluster.
    pub fn is_clustered(&self) -> bool {
        self.initial_cluster.len() > 1
    }
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
            peer_listen: String::new(),
            advertise_peer_url: String::new(),
            advertise_client_url: String::new(),
            initial_cluster: Vec::new(),
            election_ticks: 10,
            heartbeat_ticks: 1,
            // Unset means "generate it", not "run without TLS" — there is no
            // way to ask for plaintext. See crate::tls.
            cert_file: None,
            key_file: None,
            trusted_ca_file: None,
            peer_cert_file: None,
            peer_key_file: None,
            peer_trusted_ca_file: None,
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

        // NODESTORE_PEERS is the older spelling of the same thing, kept
        // working because it was documented before this one existed.
        let cluster_spec = std::env::var("NODESTORE_INITIAL_CLUSTER")
            .or_else(|_| std::env::var("NODESTORE_PEERS"))
            .unwrap_or_default();
        cfg.initial_cluster = parse_initial_cluster(&cluster_spec)?;

        if let Ok(v) = std::env::var("NODESTORE_PEER_LISTEN") {
            if !v.is_empty() {
                cfg.peer_listen = v;
            }
        }
        if let Ok(v) = std::env::var("NODESTORE_ADVERTISE_PEER_URL") {
            if !v.is_empty() {
                cfg.advertise_peer_url = v;
            }
        }
        if let Ok(v) = std::env::var("NODESTORE_ADVERTISE_CLIENT_URL") {
            if !v.is_empty() {
                cfg.advertise_client_url = v;
            }
        }
        cfg.election_ticks = parse_env("NODESTORE_ELECTION_TICKS", cfg.election_ticks)?;
        cfg.heartbeat_ticks = parse_env("NODESTORE_HEARTBEAT_TICKS", cfg.heartbeat_ticks)?;

        cfg.cert_file = path_env("NODESTORE_CERT_FILE");
        cfg.key_file = path_env("NODESTORE_KEY_FILE");
        cfg.trusted_ca_file = path_env("NODESTORE_TRUSTED_CA_FILE");
        cfg.peer_cert_file = path_env("NODESTORE_PEER_CERT_FILE");
        cfg.peer_key_file = path_env("NODESTORE_PEER_KEY_FILE");
        cfg.peer_trusted_ca_file = path_env("NODESTORE_PEER_TRUSTED_CA_FILE");

        // All three of a set, or none. A half-configured set is far more
        // likely to be an oversight than a request, and the failure it would
        // otherwise produce — quietly generating our own material while the
        // operator believes their PKI is in use — is exactly the kind of
        // security misconfiguration that goes unnoticed.
        check_tls_triple(
            "NODESTORE",
            &cfg.cert_file,
            &cfg.key_file,
            &cfg.trusted_ca_file,
        )?;
        check_tls_triple(
            "NODESTORE_PEER",
            &cfg.peer_cert_file,
            &cfg.peer_key_file,
            &cfg.peer_trusted_ca_file,
        )?;

        if cfg.election_ticks <= cfg.heartbeat_ticks {
            // Raft requires election_tick > heartbeat_tick, and a ratio near 1
            // means every scheduling hiccup looks like a dead leader.
            return Err(Error::InvalidRequest(format!(
                "NODESTORE_ELECTION_TICKS ({}) must be greater than NODESTORE_HEARTBEAT_TICKS ({}) \
                 — ideally several times greater, or ordinary jitter will keep deposing a healthy \
                 leader",
                cfg.election_ticks, cfg.heartbeat_ticks
            )));
        }

        if !cfg.initial_cluster.is_empty() {
            // A member has to be in its own cluster, or it will campaign for a
            // cluster it is not a voter in and never win.
            let own = cfg.initial_cluster.iter().find(|(id, _)| *id == cfg.member_id);
            let Some((_, peer_url)) = own else {
                return Err(Error::InvalidRequest(format!(
                    "NODESTORE_MEMBER_ID={} does not appear in the initial cluster ({}). Every \
                     member must be listed in it, including this one.",
                    cfg.member_id,
                    cfg.initial_cluster
                        .iter()
                        .map(|(id, url)| format!("{id}={url}"))
                        .collect::<Vec<_>>()
                        .join(",")
                )));
            };
            if cfg.advertise_peer_url.is_empty() {
                cfg.advertise_peer_url = peer_url.clone();
            }
            if cfg.peer_listen.is_empty() {
                // Listen on the port this member advertises, on all
                // interfaces: peers are by definition not on loopback.
                cfg.peer_listen = listen_from_url(&cfg.advertise_peer_url);
            }
            if cfg.advertise_client_url.is_empty() {
                cfg.advertise_client_url = format!("http://{}", cfg.listen);
            }
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

/// Parse `1=http://a:2380,2=http://b:2380`.
fn parse_initial_cluster(spec: &str) -> Result<Vec<(u64, String)>> {
    let mut out = Vec::new();
    for entry in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let Some((id, url)) = entry.split_once('=') else {
            return Err(Error::InvalidRequest(format!(
                "cluster member {entry:?} is not in the form <id>=<peer-url>, e.g. \
                 1=http://10.0.0.1:2380"
            )));
        };
        let id: u64 = id.trim().parse().map_err(|_| {
            Error::InvalidRequest(format!("cluster member id {id:?} is not a number"))
        })?;
        if id == 0 {
            // Raft reserves 0 for "no member", so a member with that id would
            // be indistinguishable from "no leader".
            return Err(Error::InvalidRequest(
                "0 is not a usable member id — raft uses it to mean \"no member\"".to_string(),
            ));
        }
        let url = url.trim().to_string();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(Error::InvalidRequest(format!(
                "peer URL {url:?} must include a scheme, e.g. http://10.0.0.1:2380"
            )));
        }
        if out.iter().any(|(existing, _): &(u64, String)| *existing == id) {
            return Err(Error::InvalidRequest(format!("member id {id} is listed twice")));
        }
        out.push((id, url));
    }
    Ok(out)
}

/// Turn an advertised URL into a listen address, keeping only the port and
/// binding all interfaces.
fn listen_from_url(url: &str) -> String {
    let hostport = url.split("://").nth(1).unwrap_or(url);
    let port = hostport.rsplit(':').next().unwrap_or("2380");
    format!("0.0.0.0:{port}")
}

/// An optional path from the environment. Empty means unset, so a variable
/// set to "" behaves the same as one that was never exported — which is how
/// systemd units and shell wrappers tend to pass "no value".
fn path_env(name: &str) -> Option<PathBuf> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

fn check_tls_triple(
    prefix: &str,
    cert: &Option<PathBuf>,
    key: &Option<PathBuf>,
    ca: &Option<PathBuf>,
) -> Result<()> {
    let set = [cert.is_some(), key.is_some(), ca.is_some()];
    if set.iter().all(|s| *s) || set.iter().all(|s| !*s) {
        return Ok(());
    }
    Err(Error::InvalidRequest(format!(
        "{prefix}_CERT_FILE, {prefix}_KEY_FILE and {prefix}_TRUSTED_CA_FILE must be set together \
         or not at all (set: cert={}, key={}, ca={}). Leaving all three unset generates the \
         material instead; setting only some would silently fall back to generated certificates \
         while looking configured.",
        cert.is_some(),
        key.is_some(),
        ca.is_some()
    )))
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
    fn an_initial_cluster_is_parsed_and_this_member_is_found_in_it() {
        let cfg = with_env(
            &[
                ("NODESTORE_INITIAL_CLUSTER", "1=http://10.0.0.1:2380,2=http://10.0.0.2:2380"),
                ("NODESTORE_MEMBER_ID", "2"),
            ],
            Config::from_env,
        )
        .unwrap();
        assert!(cfg.is_clustered());
        assert_eq!(cfg.initial_cluster.len(), 2);
        assert_eq!(cfg.advertise_peer_url, "http://10.0.0.2:2380");
        assert_eq!(cfg.peer_listen, "0.0.0.0:2380", "bind all interfaces, keep the advertised port");
    }

    #[test]
    fn a_member_missing_from_its_own_cluster_is_refused() {
        // It would campaign for a cluster it is not a voter in and never win,
        // which looks like "the cluster never elects a leader" rather than
        // like a typo.
        let err = with_env(
            &[("NODESTORE_INITIAL_CLUSTER", "1=http://a:2380,2=http://b:2380"), ("NODESTORE_MEMBER_ID", "9")],
            Config::from_env,
        )
        .expect_err("a member not in its own cluster must be refused");
        assert!(err.to_string().contains("does not appear in the initial cluster"));
    }

    #[test]
    fn member_id_zero_is_refused() {
        // Raft uses 0 for "no member", so such a member is indistinguishable
        // from "there is no leader".
        let err = with_env(&[("NODESTORE_INITIAL_CLUSTER", "0=http://a:2380")], Config::from_env)
            .expect_err("id 0 must be refused");
        assert!(err.to_string().contains("not a usable member id"));
    }

    #[test]
    fn a_duplicate_member_id_is_refused() {
        let err = with_env(
            &[("NODESTORE_INITIAL_CLUSTER", "1=http://a:2380,1=http://b:2380")],
            Config::from_env,
        )
        .expect_err("duplicates must be refused");
        assert!(err.to_string().contains("listed twice"));
    }

    #[test]
    fn a_peer_url_without_a_scheme_is_refused() {
        let err = with_env(&[("NODESTORE_INITIAL_CLUSTER", "1=10.0.0.1:2380")], Config::from_env)
            .expect_err("a schemeless URL must be refused");
        assert!(err.to_string().contains("must include a scheme"));
    }

    #[test]
    fn the_older_peers_spelling_still_works() {
        let cfg = with_env(
            &[("NODESTORE_PEERS", "1=http://a:2380"), ("NODESTORE_MEMBER_ID", "1")],
            Config::from_env,
        )
        .unwrap();
        assert_eq!(cfg.initial_cluster.len(), 1);
    }

    #[test]
    fn no_cluster_configured_means_single_member() {
        let cfg = with_env(&[], Config::from_env).unwrap();
        assert!(!cfg.is_clustered());
        assert!(cfg.peer_listen.is_empty(), "no peer server without peers");
    }

    #[test]
    fn an_election_timeout_at_or_below_the_heartbeat_is_refused() {
        // Raft requires the inequality, and a ratio near 1 makes ordinary
        // jitter look like a dead leader.
        let err = with_env(
            &[("NODESTORE_ELECTION_TICKS", "1"), ("NODESTORE_HEARTBEAT_TICKS", "1")],
            Config::from_env,
        )
        .expect_err("must be refused");
        assert!(err.to_string().contains("must be greater than"));
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
