//! A thin etcd v3 KV client over nodestore's gRPC API. Wraps
//! `Range`/`Put`/`DeleteRange`/`Txn`/`Watch` — Group C's storage layer and
//! Group D's watch cache both build on this. `Lease` is still a follow-up,
//! added when a lease-backed subresource actually needs it.
//!
//! TLS setup mirrors `crates/nodestore/src/tls.rs`'s own
//! `client_tls_config()` almost exactly (see that function's doc comment)
//! — nodestore's client API has no plaintext mode, so this is not optional
//! machinery.

use crate::config::Config;
use crate::storage::pb::etcdserverpb::kv_client::KvClient;
use crate::storage::pb::etcdserverpb::watch_client::WatchClient;
use crate::storage::pb::etcdserverpb::{
    watch_request::RequestUnion, DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, RangeRequest, RangeResponse,
    TxnRequest, TxnResponse, WatchCreateRequest, WatchRequest, WatchResponse,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
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
    watch: WatchClient<Channel>,
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
        Ok(StorageClient { kv: KvClient::new(channel.clone()), watch: WatchClient::new(channel) })
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

    /// Opens a watcher over `[key, range_end)` starting at `start_revision`
    /// (`0` means "now" — matches `WatchCreateRequest`'s own doc comment).
    /// Use [`prefix_range_end`] to watch every key under a prefix.
    ///
    /// Returns a [`WatchHandle`]: `responses` is the raw etcd v3 response
    /// stream, not yet decoded into this crate's own object model (Group
    /// D's watch cache is what turns a `mvccpb::Event` into a typed
    /// `Added`/`Modified`/`Deleted` the rest of the apiserver understands).
    /// The first item is always the `created: true` acknowledgement.
    ///
    /// `requests` must be kept alive for as long as the watcher should stay
    /// open — dropping it closes the request-side of the bidi stream,
    /// which the server reads as the client canceling. The caller owning
    /// it (rather than this method leaking it to keep the stream open
    /// implicitly) is deliberate: a watch cache that reconnects/rewatches
    /// repeatedly must not leak one channel per attempt, and holding
    /// `requests` is also what a future multiplexed
    /// `WatchCancelRequest`/second-watcher-per-connection path needs
    /// anyway.
    pub async fn watch(&mut self, key: Vec<u8>, range_end: Vec<u8>, start_revision: i64) -> Result<WatchHandle> {
        // Buffer of 1 is enough for the initial create; a caller that later
        // sends more requests on this channel (cancel, a second watcher)
        // is free to, since they now own the sender.
        let (tx, rx) = mpsc::channel(1);
        let create = WatchCreateRequest { key, range_end, start_revision, ..Default::default() };
        tx.send(WatchRequest { request_union: Some(RequestUnion::CreateRequest(create)) })
            .await
            .expect("channel was just created with capacity 1 and nothing else could have closed it");
        let responses = self.watch.watch(ReceiverStream::new(rx)).await?.into_inner();
        Ok(WatchHandle { requests: tx, responses })
    }
}

/// An open watch: `requests` must be kept alive for as long as the watcher
/// should stay open (see [`StorageClient::watch`]'s own doc comment),
/// `responses` is the event stream.
pub struct WatchHandle {
    pub requests: mpsc::Sender<WatchRequest>,
    pub responses: tonic::Streaming<WatchResponse>,
}

/// The standard etcd client convention for turning a key prefix into the
/// `[key, range_end)` half-open interval that matches every key under it:
/// increment the last byte that isn't already `0xff`, dropping everything
/// after it. A prefix that is all `0xff` bytes (or empty) has no finite
/// successor, so its range end is `\0` — `WatchCreateRequest.range_end`'s
/// own doc comment states `\0` means "to the end of the keyspace" for
/// exactly this reason.
pub fn prefix_range_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    for i in (0..end.len()).rev() {
        if end[i] < 0xff {
            end[i] += 1;
            end.truncate(i + 1);
            return end;
        }
    }
    vec![0]
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

    #[test]
    fn prefix_range_end_increments_the_last_byte() {
        assert_eq!(prefix_range_end(b"/registry/pods/"), b"/registry/pods0".to_vec());
        assert_eq!(prefix_range_end(b"a"), b"b".to_vec());
    }

    #[test]
    fn prefix_range_end_drops_trailing_0xff_bytes() {
        assert_eq!(prefix_range_end(&[0x01, 0xff, 0xff]), vec![0x02]);
    }

    #[test]
    fn prefix_range_end_of_all_0xff_or_empty_means_to_the_end_of_the_keyspace() {
        assert_eq!(prefix_range_end(&[0xff, 0xff]), vec![0]);
        assert_eq!(prefix_range_end(&[]), vec![0]);
    }
}
