//! Control-plane membership operations for nodebootstrap.
//!
//! Worker bootstrap deliberately does not come through this module: a worker
//! only needs a Kubernetes kubeconfig and works with any Kubernetes
//! distribution. Control-plane joining is narrower because it adds a member
//! to nodestore, so it is explicit and uses nodestore's etcd-compatible
//! Cluster RPCs to add a learner before the local service starts.

use anyhow::{bail, Context, Result};
use prost::Message;
use std::path::PathBuf;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};
use tonic::Request;

use crate::config::Config;

#[derive(Clone, Debug)]
struct TlsPaths {
    ca: PathBuf,
    cert: PathBuf,
    key: PathBuf,
}

#[derive(Clone, PartialEq, Message)]
struct Member {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(string, repeated, tag = "3")]
    peer_urls: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ResponseHeader {
    #[prost(uint64, tag = "1")]
    cluster_id: u64,
}

#[derive(Clone, PartialEq, Message)]
struct MemberListRequest {
    #[prost(bool, tag = "1")]
    linearizable: bool,
}

#[derive(Clone, PartialEq, Message)]
struct MemberListResponse {
    #[prost(message, optional, tag = "1")]
    header: Option<ResponseHeader>,
    #[prost(message, repeated, tag = "2")]
    members: Vec<Member>,
}

#[derive(Clone, PartialEq, Message)]
struct MemberAddRequest {
    #[prost(string, repeated, tag = "1")]
    peer_urls: Vec<String>,
    #[prost(bool, tag = "2")]
    is_learner: bool,
}

#[derive(Clone, PartialEq, Message)]
struct MemberAddResponse {
    #[prost(message, optional, tag = "1")]
    header: Option<ResponseHeader>,
    #[prost(message, optional, tag = "2")]
    member: Option<Member>,
    #[prost(message, repeated, tag = "3")]
    members: Vec<Member>,
}

#[derive(Clone, PartialEq, Message)]
struct MemberRemoveRequest {
    #[prost(uint64, tag = "1")]
    id: u64,
}

#[derive(Clone, PartialEq, Message)]
struct MemberRemoveResponse {
    #[prost(message, optional, tag = "1")]
    header: Option<ResponseHeader>,
    #[prost(message, repeated, tag = "2")]
    members: Vec<Member>,
}

struct ClusterClient {
    grpc: tonic::client::Grpc<Channel>,
}

impl ClusterClient {
    fn new(channel: Channel) -> Self {
        Self {
            grpc: tonic::client::Grpc::new(channel),
        }
    }

    async fn member_list(&mut self) -> Result<MemberListResponse> {
        self.unary(
            MemberListRequest { linearizable: true },
            "/etcdserverpb.Cluster/MemberList",
        )
        .await
    }

    async fn member_add(&mut self, request: MemberAddRequest) -> Result<MemberAddResponse> {
        self.unary(request, "/etcdserverpb.Cluster/MemberAdd").await
    }

    async fn member_remove(&mut self, request: MemberRemoveRequest) -> Result<()> {
        let _: MemberRemoveResponse = self
            .unary(request, "/etcdserverpb.Cluster/MemberRemove")
            .await?;
        Ok(())
    }

    async fn unary<Req, Res>(&mut self, request: Req, path: &'static str) -> Result<Res>
    where
        Req: Message + 'static,
        Res: Message + Default + 'static,
    {
        self.grpc
            .ready()
            .await
            .map_err(|error| anyhow::anyhow!("nodestore gRPC client is not ready: {error}"))?;
        let response = self
            .grpc
            .unary(
                Request::new(request),
                http::uri::PathAndQuery::from_static(path),
                tonic_prost::ProstCodec::<Req, Res>::default(),
            )
            .await
            .with_context(|| format!("calling nodestore membership RPC {path}"))?;
        Ok(response.into_inner())
    }
}

/// Add this host to the existing nodestore cluster and export the resulting
/// membership into the environment consumed by services::ensure_nodestore.
/// The caller must supply shared nodestore client credentials and shared
/// cluster PKI; neither can safely be invented on a joining host.
pub fn join_existing(cfg: &Config) -> Result<()> {
    let endpoint = cfg.control_plane_join_endpoint()?;
    let peer_url = cfg.control_plane_peer_url()?;
    validate_https("--join", &endpoint)?;
    validate_https("--peer-url", &peer_url)?;
    let tls = join_tls_paths()?;
    for (name, path) in [("CA", &tls.ca), ("certificate", &tls.cert), ("key", &tls.key)] {
        anyhow::ensure!(path.is_file(), "control-plane join {name} is missing: {}", path.display());
    }
    require_local_member_tls()?;

    let result = block_on(add_member(&endpoint, &peer_url, &tls))?;
    let mut members = result.members;
    if !members.iter().any(|member| member.0 == result.member_id) {
        members.push((result.member_id, peer_url.clone()));
    }
    members.sort_by_key(|(id, _)| *id);
    let initial_cluster = members
        .into_iter()
        .map(|(id, url)| format!("{id}={url}"))
        .collect::<Vec<_>>()
        .join(",");

    std::env::set_var("NODESTORE_MEMBER_ID", result.member_id.to_string());
    std::env::set_var("NODESTORE_CLUSTER_ID", result.cluster_id.to_string());
    std::env::set_var("NODESTORE_INITIAL_CLUSTER", initial_cluster);
    std::env::set_var("NODESTORE_ADVERTISE_PEER_URL", &peer_url);
    tracing::info!(member_id = result.member_id, peer_url, "nodestore learner added for the new control-plane node");
    Ok(())
}

/// Remove the requested member from nodestore first, then let the caller
/// remove the local control-plane services. Service removal is intentionally
/// separate so a failed membership operation cannot strand a still-member
/// node with its datastore stopped.
pub fn remove_existing(cfg: &Config) -> Result<()> {
    let endpoint = cfg.control_plane_join_endpoint()?;
    validate_https("--join", &endpoint)?;
    let member_id = cfg.control_plane_member_id.context(
        "--remove-control-plane requires --member-id=N (or NODEBOOTSTRAP_MEMBER_ID) to identify the member to remove",
    )?;
    let tls = join_tls_paths()?;
    for (name, path) in [("CA", &tls.ca), ("certificate", &tls.cert), ("key", &tls.key)] {
        anyhow::ensure!(path.is_file(), "control-plane removal {name} is missing: {}", path.display());
    }
    block_on(remove_member(&endpoint, member_id, &tls))?;
    tracing::info!(member_id, "removed the control-plane member from nodestore");
    Ok(())
}

struct AddResult {
    cluster_id: u64,
    member_id: u64,
    members: Vec<(u64, String)>,
}

async fn add_member(endpoint: &str, peer_url: &str, tls: &TlsPaths) -> Result<AddResult> {
    let mut client = connect(endpoint, tls).await?;
    let listed = client.member_list().await.context("listing existing nodestore members")?;
    let header_cluster_id = listed.header.as_ref().map(|header| header.cluster_id).unwrap_or_default();
    let mut members = member_urls(&listed.members);

    let member_id = if let Some(existing) = listed
        .members
        .iter()
        .find(|member| member.peer_urls.iter().any(|url| url == peer_url))
    {
        existing.id
    } else {
        let added = client
            .member_add(MemberAddRequest {
                peer_urls: vec![peer_url.to_string()],
                is_learner: true,
            })
            .await
            .with_context(|| format!("adding nodestore learner {peer_url}"))?;
        let added_member = added
            .member
            .context("nodestore member-add returned no member")?;
        if let Some(header) = added.header {
            if header.cluster_id != 0 {
                members = member_urls(&added.members);
                return Ok(AddResult {
                    cluster_id: header.cluster_id,
                    member_id: added_member.id,
                    members,
                });
            }
        }
        added_member.id
    };

    if !members.iter().any(|(id, _)| *id == member_id) {
        members.push((member_id, peer_url.to_string()));
    }
    Ok(AddResult {
        cluster_id: header_cluster_id,
        member_id,
        members,
    })
}

async fn remove_member(endpoint: &str, member_id: u64, tls: &TlsPaths) -> Result<()> {
    let mut client = connect(endpoint, tls).await?;
    client
        .member_remove(MemberRemoveRequest { id: member_id })
        .await
        .with_context(|| format!("removing nodestore member {member_id}"))?;
    Ok(())
}

async fn connect(endpoint: &str, tls: &TlsPaths) -> Result<ClusterClient> {
    let host = endpoint
        .parse::<http::Uri>()
        .with_context(|| format!("parsing nodestore endpoint {endpoint}"))?
        .host()
        .context("nodestore endpoint has no host")?
        .to_string();
    let ca = std::fs::read(&tls.ca).with_context(|| format!("reading {}", tls.ca.display()))?;
    let cert = std::fs::read(&tls.cert).with_context(|| format!("reading {}", tls.cert.display()))?;
    let key = std::fs::read(&tls.key).with_context(|| format!("reading {}", tls.key.display()))?;
    let tls_config = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca))
        .identity(Identity::from_pem(cert, key))
        .domain_name(host);
    let channel = Endpoint::from_shared(endpoint.to_string())
        .context("building nodestore endpoint")?
        .tls_config(tls_config)
        .context("configuring nodestore client TLS")?
        .connect()
        .await
        .with_context(|| format!("connecting to nodestore at {endpoint}"))?;
    Ok(ClusterClient::new(channel))
}

fn member_urls(members: &[Member]) -> Vec<(u64, String)> {
    members
        .iter()
        .filter_map(|member| member.peer_urls.first().cloned().map(|url| (member.id, url)))
        .collect()
}

fn join_tls_paths() -> Result<TlsPaths> {
    let path = |join_name: &str, nodestore_name: &str| {
        std::env::var(join_name)
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var(nodestore_name).ok().filter(|value| !value.is_empty()))
            .map(PathBuf::from)
    };
    Ok(TlsPaths {
        ca: path("NODEBOOTSTRAP_JOIN_CA_FILE", "NODESTORE_CLIENT_CA_FILE")
            .or_else(|| {
                std::env::var("NODESTORE_TRUSTED_CA_FILE")
                    .ok()
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            })
            .context("control-plane join needs NODEBOOTSTRAP_JOIN_CA_FILE (or NODESTORE_TRUSTED_CA_FILE)")?,
        cert: path("NODEBOOTSTRAP_JOIN_CERT_FILE", "NODESTORE_CLIENT_CERT_FILE")
            .or_else(|| {
                std::env::var("NODESTORE_CERT_FILE")
                    .ok()
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            })
            .context("control-plane join needs NODEBOOTSTRAP_JOIN_CERT_FILE (or NODESTORE_CERT_FILE)")?,
        key: path("NODEBOOTSTRAP_JOIN_KEY_FILE", "NODESTORE_CLIENT_KEY_FILE")
            .or_else(|| {
                std::env::var("NODESTORE_KEY_FILE")
                    .ok()
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            })
            .context("control-plane join needs NODEBOOTSTRAP_JOIN_KEY_FILE (or NODESTORE_KEY_FILE)")?,
    })
}

fn require_local_member_tls() -> Result<()> {
    for (name, path) in [
        ("NODESTORE_CERT_FILE", std::env::var("NODESTORE_CERT_FILE").ok()),
        ("NODESTORE_KEY_FILE", std::env::var("NODESTORE_KEY_FILE").ok()),
        ("NODESTORE_TRUSTED_CA_FILE", std::env::var("NODESTORE_TRUSTED_CA_FILE").ok()),
        ("NODESTORE_PEER_CERT_FILE", std::env::var("NODESTORE_PEER_CERT_FILE").ok()),
        ("NODESTORE_PEER_KEY_FILE", std::env::var("NODESTORE_PEER_KEY_FILE").ok()),
        ("NODESTORE_PEER_TRUSTED_CA_FILE", std::env::var("NODESTORE_PEER_TRUSTED_CA_FILE").ok()),
    ] {
        let path = path
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .with_context(|| format!("control-plane join requires {name} for the local clustered nodestore member"))?;
        anyhow::ensure!(path.is_file(), "{name} does not exist: {}", path.display());
    }
    Ok(())
}

fn validate_https(name: &str, value: &str) -> Result<()> {
    if !value.starts_with("https://") {
        bail!("{name} must use https:// because nodestore requires mutual TLS: {value}");
    }
    Ok(())
}

fn block_on<F, T>(future: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the nodestore membership runtime")?;
    runtime.block_on(future)
}
