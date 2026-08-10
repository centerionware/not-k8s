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
//! Single member. The consensus seam is real and every determinism rule raft
//! needs is already enforced, but replication itself is not implemented:
//! `NODESTORE_PEERS` is refused rather than ignored, and `MemberAdd` returns
//! `Unimplemented`. A datastore that quietly pretended to replicate would be
//! worse than one that says it doesn't.

pub mod command;
pub mod config;
pub mod consensus;
pub mod encode;
pub mod error;
pub mod pb;
pub mod replication;
pub mod server;
pub mod store;
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

        let transport = replication::transport::Transport::new(cfg.member_id);
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
    };

    // The peer server carries raft traffic and nothing else, on its own port.
    // A raft message is trusted absolutely by whoever receives it, so this is
    // never merged into the client listener.
    if let Some(handle) = raft.clone() {
        let peer_addr: std::net::SocketAddr = cfg
            .peer_listen
            .parse()
            .with_context(|| format!("NODESTORE_PEER_LISTEN={:?} is not a valid address", cfg.peer_listen))?;
        let peer_service = replication::peer_service::PeerService::new(handle, Arc::clone(&node));
        info!(%peer_addr, member = cfg.member_id, "serving raft peer traffic");
        tokio::spawn(async move {
            if let Err(e) = tonic::transport::Server::builder()
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
