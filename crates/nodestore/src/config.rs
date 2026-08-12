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
    /// Whether to run raft at all.
    ///
    /// Any configured cluster counts, including one naming only this member.
    /// That is not a pedantic case — it is the whole upgrade path: a populated
    /// single member is converted by pointing it at a cluster of itself, and
    /// only then grown with `MemberAdd`. Treating that as "not clustered" ran
    /// it as a plain single member with no raft, which made the conversion
    /// impossible to express. Found by the e2e that walks the real sequence.
    pub fn is_clustered(&self) -> bool {
        !self.initial_cluster.is_empty()
    }

    /// Whether this member shares a cluster with anyone else.
    ///
    /// Distinct from [`Self::is_clustered`] because it answers a different
    /// question: not "does raft run" but "must the PKI have been agreed with
    /// somebody". Certificates cannot be generated for a member that has peers
    /// — each would mint a CA only it trusted and the cluster could not form
    /// (see [`crate::tls`]). With no peers that reasoning does not apply,
    /// there is nobody to disagree with, so a one-member cluster keeps the
    /// material it already generated as a single member. Requiring hand-built
    /// PKI purely to convert would put a gate in front of the upgrade for no
    /// security gain.
    pub fn has_other_members(&self) -> bool {
        self.initial_cluster.iter().any(|(id, _)| *id != self.member_id)
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
                // https for the same reason the peer URLs are: the client API
                // is a mutual-TLS listener, and this URL is what a follower
                // hands back so a write can be forwarded to the leader. An
                // http:// value there produces a plaintext dial against a TLS
                // listener, so every forwarded write fails with a transport
                // error that names nothing useful.
                cfg.advertise_client_url = format!("https://{}", cfg.listen);
            }
            if cfg.advertise_client_url.starts_with("http://") {
                return Err(Error::InvalidRequest(format!(
                    "NODESTORE_ADVERTISE_CLIENT_URL={:?} uses http://, but the client API is a \
                     mutual-TLS listener — use https://. Followers forward writes to this URL, so \
                     a plaintext value makes every write through a follower fail.",
                    cfg.advertise_client_url
                )));
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
                "serving the datastore on all interfaces — this port holds every Secret in the \
                 cluster, and the only thing standing in front of it is the client CA, so anything \
                 holding a certificate signed by it can read and write all of the cluster state"
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

/// Parse `1=https://a:2380,2=https://b:2380`.
fn parse_initial_cluster(spec: &str) -> Result<Vec<(u64, String)>> {
    let mut out = Vec::new();
    for entry in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let Some((id, url)) = entry.split_once('=') else {
            return Err(Error::InvalidRequest(format!(
                "cluster member {entry:?} is not in the form <id>=<peer-url>, e.g. \
                 1=https://10.0.0.1:2380"
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
        // https, not http: the raft peer link requires mutual TLS (see
        // crate::tls), and a peer URL with an http scheme silently produces a
        // plaintext dial against a TLS listener. The members then never
        // connect, no leader is ever elected, and nothing in the logs points
        // at the scheme — it looks like a network partition. Rejecting it here
        // turns that into a startup error naming the actual problem.
        if url.starts_with("http://") {
            return Err(Error::InvalidRequest(format!(
                "peer URL {url:?} uses http://, but the raft peer link requires TLS — use \
                 https://. A plaintext peer URL cannot connect to a TLS peer listener, and the \
                 symptom is a cluster that never elects a leader."
            )));
        }
        if !url.starts_with("https://") {
            return Err(Error::InvalidRequest(format!(
                "peer URL {url:?} must include a scheme, e.g. https://10.0.0.1:2380"
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
    // Drop any path or query first: "https://10.0.0.1:2380/raft" would
    // otherwise yield the port "2380/raft".
    let hostport = hostport.split(['/', '?', '#']).next().unwrap_or(hostport);
    // Only a numeric tail is a port. The old `rsplit(':').next()` could not
    // fall back, because rsplit always yields at least one item — so
    // "https://peer.example" produced "0.0.0.0:peer.example", which failed
    // much later at parse/bind time with a message naming neither the URL nor
    // the variable it came from.
    let port = match hostport.rsplit_once(':') {
        Some((_, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => p,
        _ => DEFAULT_PEER_PORT,
    };
    format!("0.0.0.0:{port}")
}

/// The port a peer URL is assumed to use when it names none — etcd's peer
/// port, which is what an operator writing `https://host` will have meant.
const DEFAULT_PEER_PORT: &str = "2380";

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
                ("NODESTORE_INITIAL_CLUSTER", "1=https://10.0.0.1:2380,2=https://10.0.0.2:2380"),
                ("NODESTORE_MEMBER_ID", "2"),
            ],
            Config::from_env,
        )
        .unwrap();
        assert!(cfg.is_clustered());
        assert_eq!(cfg.initial_cluster.len(), 2);
        assert_eq!(cfg.advertise_peer_url, "https://10.0.0.2:2380");
        assert_eq!(cfg.peer_listen, "0.0.0.0:2380", "bind all interfaces, keep the advertised port");
    }

    #[test]
    fn a_member_missing_from_its_own_cluster_is_refused() {
        // It would campaign for a cluster it is not a voter in and never win,
        // which looks like "the cluster never elects a leader" rather than
        // like a typo.
        let err = with_env(
            &[("NODESTORE_INITIAL_CLUSTER", "1=https://a:2380,2=https://b:2380"), ("NODESTORE_MEMBER_ID", "9")],
            Config::from_env,
        )
        .expect_err("a member not in its own cluster must be refused");
        assert!(err.to_string().contains("does not appear in the initial cluster"));
    }

    #[test]
    fn member_id_zero_is_refused() {
        // Raft uses 0 for "no member", so such a member is indistinguishable
        // from "there is no leader".
        let err = with_env(&[("NODESTORE_INITIAL_CLUSTER", "0=https://a:2380")], Config::from_env)
            .expect_err("id 0 must be refused");
        assert!(err.to_string().contains("not a usable member id"));
    }

    #[test]
    fn a_duplicate_member_id_is_refused() {
        let err = with_env(
            &[("NODESTORE_INITIAL_CLUSTER", "1=https://a:2380,1=https://b:2380")],
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
            &[("NODESTORE_PEERS", "1=https://a:2380"), ("NODESTORE_MEMBER_ID", "1")],
            Config::from_env,
        )
        .unwrap();
        assert_eq!(cfg.initial_cluster.len(), 1);
    }

    #[test]
    fn no_cluster_configured_means_single_member() {
        let cfg = with_env(&[], Config::from_env).unwrap();
        assert!(!cfg.is_clustered());
        assert!(!cfg.has_other_members());
        assert!(cfg.peer_listen.is_empty(), "no peer server without peers");
    }

    /// A cluster naming only this member is a real one-member raft cluster,
    /// not a plain single member. It is the middle step of the upgrade path:
    /// a populated single member is converted by pointing it at a cluster of
    /// itself and only then grown with MemberAdd. Treating it as unclustered
    /// ran it with no raft, which made the conversion impossible to express —
    /// caught by the e2e that walks the real sequence, not by any unit test.
    #[test]
    fn a_cluster_of_one_still_runs_raft() {
        let cfg = with_env(
            &[
                ("NODESTORE_INITIAL_CLUSTER", "1=https://10.0.0.1:2380"),
                ("NODESTORE_MEMBER_ID", "1"),
            ],
            Config::from_env,
        )
        .unwrap();
        assert!(cfg.is_clustered(), "a configured cluster runs raft even with one member");
        // ...but it has nobody to agree a CA with, so it may still generate
        // its own material — which is exactly what it already has from its
        // single-member life. Requiring hand-built PKI purely to convert would
        // gate the upgrade for no security gain.
        assert!(!cfg.has_other_members());
    }

    #[test]
    fn a_cluster_with_peers_needs_material_agreed_with_them() {
        let cfg = with_env(
            &[
                ("NODESTORE_INITIAL_CLUSTER", "1=https://10.0.0.1:2380,2=https://10.0.0.2:2380"),
                ("NODESTORE_MEMBER_ID", "1"),
            ],
            Config::from_env,
        )
        .unwrap();
        assert!(cfg.is_clustered());
        assert!(cfg.has_other_members(), "member 2 is somebody to disagree with");
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

    // A plaintext peer URL against a TLS peer listener produces no error
    // anywhere: the members simply never connect, no leader is elected, and
    // it reads as a network partition. Found exactly that way — nine cluster
    // e2e tests failed with "never elected a leader" and nothing in any log
    // mentioned the scheme.
    #[test]
    fn an_http_peer_url_is_rejected_because_the_peer_link_requires_tls() {
        let err = with_env(
            &[("NODESTORE_INITIAL_CLUSTER", "1=http://a:2380"), ("NODESTORE_MEMBER_ID", "1")],
            Config::from_env,
        )
        .expect_err("http:// peer URLs cannot reach a TLS peer listener");
        let msg = err.to_string();
        assert!(msg.contains("https://"), "should say what to use instead: {msg}");
        assert!(msg.contains("never elects a leader"), "should name the symptom: {msg}");
    }

    /// The client API is a mutual-TLS listener, so the URL a follower hands
    /// back for write forwarding has to be https or every forwarded write
    /// fails on a plaintext dial.
    #[test]
    fn the_default_advertised_client_url_is_https() {
        let cfg = with_env(
            &[
                ("NODESTORE_INITIAL_CLUSTER", "1=https://10.0.0.1:2380"),
                ("NODESTORE_MEMBER_ID", "1"),
            ],
            Config::from_env,
        )
        .unwrap();
        assert!(
            cfg.advertise_client_url.starts_with("https://"),
            "got {}",
            cfg.advertise_client_url
        );
    }

    #[test]
    fn an_http_advertised_client_url_is_rejected() {
        let err = with_env(
            &[
                ("NODESTORE_INITIAL_CLUSTER", "1=https://10.0.0.1:2380"),
                ("NODESTORE_MEMBER_ID", "1"),
                ("NODESTORE_ADVERTISE_CLIENT_URL", "http://10.0.0.1:2379"),
            ],
            Config::from_env,
        )
        .expect_err("a plaintext client URL cannot reach the mutual-TLS client listener");
        assert!(err.to_string().contains("https://"), "should say what to use instead: {err}");
    }

    /// `rsplit(':').next()` can never fall back, so a URL with no port used to
    /// yield the listen address "0.0.0.0:peer.example" and a URL with a path
    /// yielded "0.0.0.0:2380/raft". Both failed far away from the cause.
    #[test]
    fn a_peer_url_port_is_only_taken_when_it_is_actually_a_port() {
        assert_eq!(listen_from_url("https://10.0.0.1:2380"), "0.0.0.0:2380");
        assert_eq!(listen_from_url("https://peer.example"), "0.0.0.0:2380", "no port — fall back");
        assert_eq!(listen_from_url("https://10.0.0.1:2381/raft"), "0.0.0.0:2381", "drop the path");
        assert_eq!(listen_from_url("https://peer.example/raft"), "0.0.0.0:2380");
        // An IPv6 literal's last colon-separated field is not a port either.
        assert_eq!(listen_from_url("https://[fd00::1]"), "0.0.0.0:2380");
        assert_eq!(listen_from_url("https://[fd00::1]:2382"), "0.0.0.0:2382");
    }
}
