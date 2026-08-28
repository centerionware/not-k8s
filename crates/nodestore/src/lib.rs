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
use tracing::{error, info};

/// Install rustls' default CryptoProvider, unless something already did.
///
/// rustls 0.23 stopped silently picking one. The combined `notk8s` binary
/// links several components that all use rustls, so feature unification can
/// leave more than one provider available and make the first TLS builder
/// panic. Split binaries do not expose that because each component links a
/// smaller dependency graph. Select the ring provider at the component
/// boundary so both layouts have identical startup behaviour.
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("installing default rustls CryptoProvider (no other provider was installed a moment ago)");
    }
}

/// Run the datastore until it stops. Only returns on a fatal condition.
pub async fn run() -> Result<()> {
    install_crypto_provider();
    let cfg = config::Config::from_env().context("loading configuration")?;
    serve(cfg).await
}

/// Run with an explicit configuration — the seam the e2e tests and the
/// integration tests use to run a store on a scratch path and port.
pub async fn serve(cfg: config::Config) -> Result<()> {
    install_fatal_panic_hook();

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
        cfg.has_other_members(),
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

    // A peer-server failure has to bring the whole member down, not just its
    // own task. A member that keeps serving the client API with a dead raft
    // link answers reads it can no longer keep current and can never be
    // replicated to again — it diverges from the cluster silently, which is
    // strictly worse than being unreachable.
    let (peer_failed_tx, peer_failed_rx) = tokio::sync::oneshot::channel::<anyhow::Error>();
    // Kept alive for the whole function: with no raft the sender is never
    // dropped, so the receiver simply never fires.
    let mut peer_failed_tx = Some(peer_failed_tx);

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
            cfg.has_other_members(),
        )
        .context("preparing raft peer TLS material")?;
        info!(ca = %peer_material.ca.display(), "raft peer link requires mutual TLS");

        // Bind before starting the raft driver. The driver can campaign as
        // soon as its task is spawned; if the listener is only bound after
        // that point, every member can drop its first pre-vote connection
        // while the peer server is still coming up and never form a quorum.
        let peer_addr: std::net::SocketAddr = cfg
            .peer_listen
            .parse()
            .with_context(|| format!("NODESTORE_PEER_LISTEN={:?} is not a valid address", cfg.peer_listen))?;
        let incoming = tonic::transport::server::TcpIncoming::bind(peer_addr)
            .with_context(|| format!("binding raft peer listener at {peer_addr}"))?;
        info!(%peer_addr, "raft peer listener bound before raft startup");

        let transport =
            replication::transport::Transport::new(cfg.member_id, Some(peer_material.clone()));
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

        // Asked before raft is built, because the answer decides whether an
        // empty data directory means "new cluster" or "a member that has been
        // wiped out from under a cluster that is still running" — and only the
        // other members still know which. Short and best-effort: a whole
        // cluster starting at once has nothing to answer yet, and that is the
        // ordinary case, not a failure.
        let probe = replication::transport::probe_cluster(
            cfg.member_id,
            &members,
            Some(&peer_material),
            std::time::Duration::from_secs(2),
        )
        .await;

        let handle = replication::driver::start(
            cfg.member_id,
            members,
            log,
            Arc::clone(&node),
            Arc::clone(&transport),
            cfg.election_ticks,
            cfg.heartbeat_ticks,
            probe,
        )
        .context("starting raft")?;
        consensus.attach(handle.clone());

        // Start accepting peer traffic as soon as the driver exists. The
        // listener was bound above to reserve the address, but waiting until
        // after the whole clustered branch (including the pre-start probe)
        // leaves the first election messages racing a multi-second startup
        // gap. The transport is bounded and lossy by design, so the server
        // must be live before the first tick can campaign.
        let peer_service = replication::peer_service::PeerService::new(handle.clone(), Arc::clone(&node));
        let tls = tls::server_tls_config(&peer_material).context("building peer TLS config")?;
        let peer_server = tonic::transport::Server::builder()
            .tls_config(tls)
            .context("applying peer TLS config")?
            .add_service(pb::peer::peer_server::PeerServer::new(peer_service));
        let tx = peer_failed_tx.take().expect("the peer sender is taken exactly once");
        info!(%peer_addr, member = cfg.member_id, "serving raft peer listener");
        tokio::spawn(async move {
            let result = peer_server
                .serve_with_incoming(incoming)
                .await
                .context("the raft peer server stopped");
            let error = match result {
                Ok(()) => anyhow::anyhow!("the raft peer server exited unexpectedly"),
                Err(e) => e,
            };
            error!(error = %error, %peer_addr, "raft peer listener exited; stopping this member");
            let _ = tx.send(error);
        });

        tokio::spawn(replication::bootstrap::publish_address_book(
            handle.clone(),
            Arc::clone(&node),
            cfg.clone(),
        ));
        tokio::spawn(replication::bootstrap::campaign_if_alone(handle.clone(), cfg.clone()));

        (node, Some(handle))
    } else {
        // Resume the index from what the store has already applied, rather
        // than from zero. Store::apply_at() writes whatever index it is given,
        // so a restarted single member counting from zero again would move the
        // persisted applied index *backwards* — and that value is what a later
        // clustered start reads to decide where in the raft log it stands.
        let applied = store.applied_index().context("reading the applied index")?;
        let consensus = Arc::new(consensus::SingleNode::resuming(cfg.member_id, cfg.cluster_id, applied));
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
    let client_server = tonic::transport::Server::builder()
        .tls_config(tls::server_tls_config(&client_tls).context("building client API TLS")?)
        .context("applying client API TLS")?
        .add_service(KvServer::new(api.clone()))
        .add_service(WatchServer::new(api.clone()))
        .add_service(LeaseServer::new(api.clone()))
        .add_service(MaintenanceServer::new(api.clone()))
        .add_service(ClusterServer::new(api))
        .serve(addr);
    tokio::pin!(client_server);

    // Whichever of the two servers stops first ends the process. See the
    // channel's own comment above for why a dead peer link must not be
    // survivable.
    tokio::select! {
        r = &mut client_server => r.context("the gRPC server stopped")?,
        recv = peer_failed_rx => match recv {
            Ok(e) => return Err(e.context(
                "the raft peer server stopped, so this member can no longer be replicated to",
            )),
            // The sender was dropped without sending: there is no peer server
            // on this member at all. Nothing to watch for — keep serving.
            Err(_) => client_server.await.context("the gRPC server stopped")?,
        },
    }
    Ok(())
}

/// Make any panic kill the process instead of just the task it happened on.
///
/// Found live on a three-member cluster: raft-rs panicked inside the driver's
/// tokio task (`to_commit N is out of range [last_index 0]`). Tokio's default
/// is to unwind that one task and carry on, so the process stayed up, the
/// client listener kept accepting connections, `systemctl is-active` kept
/// reporting **active** — and every request returned "no leader elected within
/// 3s" forever, because the raft driver was dead. `Restart=always` never
/// fired, since from systemd's point of view nothing had failed.
///
/// A datastore is exactly the wrong place to survive a broken invariant. A
/// panic here means some assumption about the log or the state machine did not
/// hold, and continuing to answer reads on that basis is worse than being
/// down: a member that is visibly gone gets its traffic taken elsewhere, while
/// one that lies about being healthy does not. Dying loudly is also what makes
/// the service manager's restart policy mean anything.
///
/// `abort`, not `exit`: it runs no destructors, so nothing gets a chance to
/// flush half-built state over good state on the way out, and it leaves a core
/// for whatever caused it.
fn install_fatal_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // The default hook first, so the panic message and any backtrace land
        // in the log exactly as they normally would.
        previous(info);
        tracing::error!("panicked — aborting, because a datastore that survives a broken invariant is worse than one that stops");
        std::process::abort();
    }));
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
