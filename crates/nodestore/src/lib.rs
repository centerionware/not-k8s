//! nodestore — the datastore for not-k8s.
//!
//! Speaks the etcd v3 gRPC API over a sqlite-backed MVCC store, so a real
//! kube-apiserver can use it unmodified (`--etcd-servers`, or k3s's
//! `--datastore-endpoint`). It replaces kine, which does the same translation
//! but drives watches by polling the database — the cost of which is visible
//! on an idle cluster as a control plane doing hundreds of syscalls a second
//! with nothing to do. Here, an applied command hands its events straight to
//! the watchers, and an idle store is idle.
//!
//! Reading order, roughly outside-in:
//!
//!   * [`server`]    — the gRPC surface. Translation only.
//!   * [`consensus`] — where ordering is decided, and where raft plugs in.
//!   * [`store`]     — MVCC semantics: revisions, transactions, compaction.
//!   * [`command`]   — the state machine's input language, and the future
//!                     raft log entry. Start here to understand the
//!                     determinism rules the rest obeys.
//!   * [`encode`]    — the log entry format, and the one place a mistake
//!                     becomes a data format problem rather than a bug.
//!   * [`replication`] — raft: the log in sqlite, and the durability argument
//!                     for why it is a separate database.
//!   * [`watch`]     — event fan-out.
//!
//! # Status
//!
//! Replicated via raft. A single member is still the default — configure a
//! cluster with `NODESTORE_INITIAL_CLUSTER` and `NODESTORE_MEMBER_ID` — and
//! the consensus seam is what keeps the two paths from diverging: everything
//! downstream proposes commands the same way either way.
//!
//! Both listeners require TLS, and both require the other end to present a
//! certificate. There is no plaintext mode; see [`tls`] for why, and for the
//! two separate trust domains.

pub mod command;
pub mod config;
pub mod consensus;
pub mod encode;
pub mod error;
pub mod pb;
pub mod replication;
pub mod server;
pub mod store;
pub mod tls;
pub mod watch;

use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::info;

/// Run the datastore until it stops. Only returns on a fatal condition.
pub async fn run() -> Result<()> {
    let cfg = config::Config::from_env().context("loading configuration")?;
    serve(cfg).await
}

/// Run with an explicit configuration — the seam the e2e tests and the
/// integration tests use to run a store on a scratch path and port.
pub async fn serve(cfg: config::Config) -> Result<()> {
    let db_path = cfg.db_path();
    let store = store::Store::open(&db_path)
        .with_context(|| format!("opening the datastore at {}", db_path.display()))?;
    let revision = store.revision().context("reading the current revision")?;
    info!(
        db = %db_path.display(),
        revision,
        member_id = cfg.member_id,
        "datastore opened"
    );

    // TLS, before anything binds a socket. There is no plaintext mode: the
    // datastore holds every Secret in the cluster and the etcd v3 API has no
    // authentication of its own, so an unencrypted or unauthenticated listener
    // would be strictly worse than an open apiserver — there is no
    // authorization layer above this to fail closed. See crate::tls.
    let sans = tls_sans(&cfg);
    let client_tls = tls::load_or_generate(
        &cfg.data_dir,
        tls::Domain::Client,
        configured_material(&cfg.cert_file, &cfg.key_file, &cfg.trusted_ca_file),
        &sans,
        cfg.is_clustered(),
    )
    .context("preparing client API TLS material")?;
    info!(
        ca = %client_tls.ca.display(),
        "client API requires TLS with a client certificate"
    );
    if let Some(cc) = &client_tls.client_cert {
        // The path an operator has to hand kube-apiserver. Logged once at
        // startup because otherwise the only way to find it is to know this
        // module's directory layout.
        info!(
            cert = %cc.display(),
            key = %client_tls.client_key.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            "a client certificate for kube-apiserver is available"
        );
    }

    // Set only in a clustered run, and needed again further down where the
    // peer server binds — hence declared out here rather than inside the
    // branch that builds it.
    let mut peer_tls: Option<tls::Material> = None;

    // Single member or a real cluster. The difference is confined to which
    // Consensus is installed and whether a peer server runs — everything
    // downstream proposes commands the same way either way.
    let (node, raft) = if cfg.is_clustered() {
        let raft_db = cfg.data_dir.join("raft.db");
        let log = replication::log::RaftLog::open(&raft_db)
            .with_context(|| format!("opening the raft log at {}", raft_db.display()))?;

        let consensus = Arc::new(replication::consensus::RaftConsensus::new(
            cfg.member_id,
            cfg.cluster_id,
        ));
        let node = consensus::Node::new(store, Arc::clone(&consensus) as Arc<dyn consensus::Consensus>, cfg.watch_buffer);

        // A separate trust domain from the client API, deliberately: a
        // client certificate must not also be an admission ticket to the
        // raft cluster, where a member can rewrite committed history.
        let peer_material = tls::load_or_generate(
            &cfg.data_dir,
            tls::Domain::Peer,
            configured_material(
                &cfg.peer_cert_file,
                &cfg.peer_key_file,
                &cfg.peer_trusted_ca_file,
            ),
            &sans,
            cfg.is_clustered(),
        )
        .context("preparing raft peer TLS material")?;
        info!(ca = %peer_material.ca.display(), "raft peer link requires mutual TLS");
        peer_tls = Some(peer_material.clone());

        let transport =
            replication::transport::Transport::new(cfg.member_id, Some(peer_material));
        let members: Vec<command::Member> = cfg
            .initial_cluster
            .iter()
            .map(|(id, peer_url)| command::Member {
                id: *id,
                peer_url: peer_url.clone(),
                client_url: if *id == cfg.member_id {
                    cfg.advertise_client_url.clone()
                } else {
                    String::new()
                },
                name: format!("member-{id}"),
                is_learner: false,
            })
            .collect();

        let handle = replication::driver::start(
            cfg.member_id,
            members,
            log,
            Arc::clone(&node),
            Arc::clone(&transport),
            cfg.election_ticks,
            cfg.heartbeat_ticks,
        )
        .context("starting raft")?;
        consensus.attach(handle.clone());

        tokio::spawn(replication::bootstrap::publish_address_book(
            handle.clone(),
            Arc::clone(&node),
            cfg.clone(),
        ));
        tokio::spawn(replication::bootstrap::campaign_if_alone(handle.clone(), cfg.clone()));

        (node, Some(handle))
    } else {
        let consensus = Arc::new(consensus::SingleNode::new(cfg.member_id, cfg.cluster_id));
        (consensus::Node::new(store, consensus, cfg.watch_buffer), None)
    };

    let api = match raft.clone() {
        Some(handle) => server::EtcdApi::new(Arc::clone(&node)).with_raft(handle),
        None => server::EtcdApi::new(Arc::clone(&node)),
    }
    .with_client_tls(client_tls.clone());

    // The peer server carries raft traffic and nothing else, on its own port.
    // A raft message is trusted absolutely by whoever receives it, so this is
    // never merged into the client listener.
    if let Some(handle) = raft.clone() {
        let peer_addr: std::net::SocketAddr = cfg
            .peer_listen
            .parse()
            .with_context(|| format!("NODESTORE_PEER_LISTEN={:?} is not a valid address", cfg.peer_listen))?;
        let peer_service = replication::peer_service::PeerService::new(handle, Arc::clone(&node));
        let peer_tls_for_server = peer_tls.clone();
        info!(%peer_addr, member = cfg.member_id, "serving raft peer traffic");
        tokio::spawn(async move {
            let Some(material) = peer_tls_for_server else {
                tracing::error!("a clustered member has no peer TLS material; refusing to serve raft in the clear");
                return;
            };
            let tls = match tls::server_tls_config(&material) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "could not build peer TLS config");
                    return;
                }
            };
            let mut server = match tonic::transport::Server::builder().tls_config(tls) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "could not apply peer TLS config");
                    return;
                }
            };
            if let Err(e) = server
                .add_service(pb::peer::peer_server::PeerServer::new(peer_service))
                .serve(peer_addr)
                .await
            {
                // Without a peer server this member can receive nothing, so
                // it can never be replicated to. Loud, and fatal in effect.
                tracing::error!(error = %e, "the raft peer server stopped");
            }
        });
    }

    tokio::spawn(server::compaction_loop(
        api.clone(),
        cfg.compact_interval_secs,
        cfg.compact_retain_revisions,
    ));
    tokio::spawn(server::lease_expiry_loop(api.clone(), cfg.lease_check_interval_secs));

    let addr = cfg
        .listen
        .parse()
        .with_context(|| format!("NODESTORE_LISTEN={:?} is not a valid socket address", cfg.listen))?;
    info!(%addr, "serving the etcd v3 API");

    use pb::etcdserverpb::{
        cluster_server::ClusterServer, kv_server::KvServer, lease_server::LeaseServer,
        maintenance_server::MaintenanceServer, watch_server::WatchServer,
    };
    tonic::transport::Server::builder()
        .tls_config(tls::server_tls_config(&client_tls).context("building client API TLS")?)
        .context("applying client API TLS")?
        .add_service(KvServer::new(api.clone()))
        .add_service(WatchServer::new(api.clone()))
        .add_service(LeaseServer::new(api.clone()))
        .add_service(MaintenanceServer::new(api.clone()))
        .add_service(ClusterServer::new(api))
        .serve(addr)
        .await
        .context("the gRPC server stopped")?;
    Ok(())
}

/// The names and addresses this member is reachable as, for the certificate's
/// SANs.
///
/// A missing SAN is the most common way a correct PKI still fails to connect,
/// so this is deliberately generous: the hostname, loopback in both families,
/// and the host of every URL this member advertises. Over-including costs
/// nothing — a SAN is only ever checked against the name the client actually
/// dialled.
fn tls_sans(cfg: &config::Config) -> Vec<String> {
    let mut sans = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if let Ok(h) = hostname_of_this_machine() {
        sans.push(h);
    }
    for url in [&cfg.advertise_client_url, &cfg.advertise_peer_url] {
        if let Some(h) = host_of(url) {
            sans.push(h);
        }
    }
    // The listen address may be a concrete IP rather than a wildcard, in
    // which case that is the address peers will dial.
    if let Some(h) = cfg.listen.rsplit_once(':').map(|(h, _)| h.to_string()) {
        if !h.is_empty() && h != "0.0.0.0" && h != "[::]" {
            sans.push(h.trim_matches(|c| c == '[' || c == ']').to_string());
        }
    }
    sans.sort();
    sans.dedup();
    sans.retain(|s| !s.is_empty());
    sans
}

/// The host part of a URL, without pulling in a URL parser for two fields.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest.split('/').next()?;
    let host = match host.rsplit_once(':') {
        // Leave an IPv6 literal's colons alone; only strip a trailing port.
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    };
    let host = host.trim_matches(|c| c == '[' || c == ']');
    (!host.is_empty()).then(|| host.to_string())
}

fn hostname_of_this_machine() -> std::io::Result<String> {
    // /etc/hostname rather than the `hostname` binary: Arch does not ship
    // one, which took nodelet down on a real node once already.
    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string())
}

/// Turn a fully-specified trio of configured paths into TLS material.
///
/// `None` unless all three are set — config validation has already rejected a
/// partially-set trio, so this only has to distinguish "configured" from
/// "generate it".
fn configured_material(
    cert: &Option<std::path::PathBuf>,
    key: &Option<std::path::PathBuf>,
    ca: &Option<std::path::PathBuf>,
) -> Option<tls::Material> {
    match (cert, key, ca) {
        (Some(cert), Some(key), Some(ca)) => Some(tls::Material {
            ca: ca.clone(),
            cert: cert.clone(),
            key: key.clone(),
            client_cert: None,
            client_key: None,
        }),
        _ => None,
    }
}
