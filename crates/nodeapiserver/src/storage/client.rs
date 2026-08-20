//! A thin etcd v3 KV client over nodestore's gRPC API. Wraps exactly the
//! four RPCs Group C's storage layer needs on top of
//! (`Range`/`Put`/`DeleteRange`/`Txn`) — `Watch`/`Lease` are Group D/C
//! follow-ups, added when the watch cache and lease-backed subresources
//! actually need them, not speculatively now.
//!
//! TLS setup mirrors `crates/nodestore/src/tls.rs`'s own
//! `client_tls_config()` almost exactly (see that function's doc comment)
//! — nodestore's client API has no plaintext mode, so this is not optional
//! machinery.

use crate::config::Config;
use crate::storage::pb::etcdserverpb::kv_client::KvClient;
use crate::storage::pb::etcdserverpb::{
    DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, RangeRequest, RangeResponse, TxnRequest, TxnResponse,
};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading nodestore TLS material at {path}: {source}")]
    ReadMaterial { path: std::path::PathBuf, source: std::io::Error },
    #[error("configuring nodestore TLS: {0}")]
    Tls(#[source] tonic::transport::Error),
    #[error("connecting to nodestore at {endpoint}: {source}")]
    Connect { endpoint: String, source: tonic::transport::Error },
    #[error("nodestore RPC failed: {0}")]
    Rpc(#[from] tonic::Status),
}

type Result<T> = std::result::Result<T, Error>;

/// A connected client. Cheap to clone (an inner `tonic::transport::Channel`
/// is itself a cheap-to-clone multiplexed connection handle) — same
/// posture `kube::Client` and every other gRPC client in this workspace
/// takes.
#[derive(Clone)]
pub struct StorageClient {
    kv: KvClient<Channel>,
}

impl StorageClient {
    /// Connects using `cfg.nodestore_endpoint`, presenting the client
    /// certificate configured by `cfg.nodestore_{cert,key,ca}_file` if
    /// set. All three unset is a real, supported case in development
    /// (nodestore itself may be running with generated material an
    /// operator hasn't wired a matching client cert for yet) — the
    /// connection is attempted without a client identity and nodestore's
    /// own mTLS enforcement is what actually rejects it if that is wrong,
    /// not this function guessing on the client's behalf.
    pub async fn connect(cfg: &Config) -> Result<StorageClient> {
        let endpoint = Endpoint::from_shared(cfg.nodestore_endpoint.clone())
            .map_err(|e| Error::Connect { endpoint: cfg.nodestore_endpoint.clone(), source: e })?
            .connect_timeout(std::time::Duration::from_secs(5));

        let endpoint = match (&cfg.nodestore_cert_file, &cfg.nodestore_key_file, &cfg.nodestore_ca_file) {
            (Some(cert), Some(key), Some(ca)) => {
                let tls = client_tls_config(cert, key, ca)?;
                endpoint.tls_config(tls).map_err(Error::Tls)?
            }
            _ => endpoint,
        };

        let channel = endpoint.connect().await.map_err(|e| Error::Connect { endpoint: cfg.nodestore_endpoint.clone(), source: e })?;
        Ok(StorageClient { kv: KvClient::new(channel) })
    }

    /// `revision <= 0` means "the current revision" — matches etcd's own
    /// `RangeRequest.revision` semantics (0 is not a real revision; the
    /// first real one is 1), so `resourceVersion=0`/unset LIST requests
    /// (finding: `docs/APISERVER_PLAN.md`'s watch-cache section) pass
    /// straight through without a separate branch here.
    pub async fn range(&mut self, req: RangeRequest) -> Result<RangeResponse> {
        Ok(self.kv.range(req).await?.into_inner())
    }

    pub async fn put(&mut self, req: PutRequest) -> Result<PutResponse> {
        Ok(self.kv.put(req).await?.into_inner())
    }

    pub async fn delete_range(&mut self, req: DeleteRangeRequest) -> Result<DeleteRangeResponse> {
        Ok(self.kv.delete_range(req).await?.into_inner())
    }

    /// The optimistic-concurrency primitive: a `Txn` whose `compare` list
    /// checks `mod_revision` against the caller's expected
    /// `resourceVersion` (== nodestore's MVCC revision, `docs/APISERVER.md`
    /// finding 3) succeeds only if nobody wrote the key since. A failed
    /// compare is what a `409 Conflict` is built from one layer up (Group
    /// E) — this method itself just reports whether `succeeded` was true,
    /// same as etcd's own API does.
    pub async fn txn(&mut self, req: TxnRequest) -> Result<TxnResponse> {
        Ok(self.kv.txn(req).await?.into_inner())
    }
}

fn client_tls_config(cert: &std::path::Path, key: &std::path::Path, ca: &std::path::Path) -> Result<ClientTlsConfig> {
    let cert_pem = read(cert)?;
    let key_pem = read(key)?;
    let ca_pem = read(ca)?;
    Ok(ClientTlsConfig::new().ca_certificate(Certificate::from_pem(ca_pem)).identity(Identity::from_pem(cert_pem, key_pem)))
}

fn read(path: &std::path::Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error::ReadMaterial { path: path.to_path_buf(), source: e })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connecting_to_an_unreachable_endpoint_is_a_clear_connect_error_not_a_hang() {
        let mut cfg = Config::default();
        // A loopback port nothing is listening on — the point is that this
        // returns promptly with a named error rather than hanging or
        // panicking, not that it succeeds.
        cfg.nodestore_endpoint = "https://127.0.0.1:1".to_string();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), StorageClient::connect(&cfg)).await;
        let result = result.expect("must not hang past its own connect_timeout");
        assert!(matches!(result, Err(Error::Connect { .. })));
    }

    #[tokio::test]
    async fn a_missing_cert_file_is_a_named_read_error() {
        let mut cfg = Config::default();
        cfg.nodestore_cert_file = Some(std::path::PathBuf::from("/nonexistent/cert.pem"));
        cfg.nodestore_key_file = Some(std::path::PathBuf::from("/nonexistent/key.pem"));
        cfg.nodestore_ca_file = Some(std::path::PathBuf::from("/nonexistent/ca.pem"));
        let err = StorageClient::connect(&cfg).await.expect_err("a missing cert file must be a clear error");
        assert!(matches!(err, Error::ReadMaterial { .. }), "got {err:?}");
    }
}
