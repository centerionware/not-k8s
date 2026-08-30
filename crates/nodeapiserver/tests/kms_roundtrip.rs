//! Protocol-level KMS v1/v2 coverage. The fake plugins implement the real
//! generated gRPC services over Unix sockets, while the encryption config and
//! transformer are the production implementations. This catches wire-shape
//! mistakes that a parser-only test cannot see.

use nodeapiserver::storage::encryption::Transformer;
use nodeapiserver::storage::pb::{kms_v1, kms_v2};
use nodeapiserver::storage::encryption_config::{parse, transformers_for};
use prost::Message;
use std::path::Path;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{Request, Response, Status};

#[derive(Clone, Default)]
struct FakeKmsV1;

#[tonic::async_trait]
impl kms_v1::key_management_service_server::KeyManagementService for FakeKmsV1 {
    async fn version(&self, _request: Request<kms_v1::VersionRequest>) -> Result<Response<kms_v1::VersionResponse>, Status> {
        Ok(Response::new(kms_v1::VersionResponse { version: "v1beta1".to_string(), runtime_name: "test-kms".to_string(), runtime_version: "1".to_string() }))
    }

    async fn decrypt(&self, request: Request<kms_v1::DecryptRequest>) -> Result<Response<kms_v1::DecryptResponse>, Status> {
        Ok(Response::new(kms_v1::DecryptResponse { plain: request.into_inner().cipher }))
    }

    async fn encrypt(&self, request: Request<kms_v1::EncryptRequest>) -> Result<Response<kms_v1::EncryptResponse>, Status> {
        Ok(Response::new(kms_v1::EncryptResponse { cipher: request.into_inner().plain }))
    }
}

#[derive(Clone, Default)]
struct FakeKmsV2;

#[tonic::async_trait]
impl kms_v2::key_management_service_server::KeyManagementService for FakeKmsV2 {
    async fn status(&self, _request: Request<kms_v2::StatusRequest>) -> Result<Response<kms_v2::StatusResponse>, Status> {
        Ok(Response::new(kms_v2::StatusResponse { version: "v2".to_string(), healthz: "ok".to_string(), key_id: "test-kek-v1".to_string() }))
    }

    async fn decrypt(&self, request: Request<kms_v2::DecryptRequest>) -> Result<Response<kms_v2::DecryptResponse>, Status> {
        Ok(Response::new(kms_v2::DecryptResponse { plaintext: request.into_inner().ciphertext }))
    }

    async fn encrypt(&self, request: Request<kms_v2::EncryptRequest>) -> Result<Response<kms_v2::EncryptResponse>, Status> {
        Ok(Response::new(kms_v2::EncryptResponse { ciphertext: request.into_inner().plaintext, key_id: "test-kek-v1".to_string(), annotations: Default::default() }))
    }
}

fn socket_path(tempdir: &TempDir, name: &str) -> String {
    tempdir.path().join(name).to_string_lossy().into_owned()
}

async fn start_v1(path: &str) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
    let listener = UnixListener::bind(Path::new(path)).expect("binding fake KMS v1 socket");
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(kms_v1::key_management_service_server::KeyManagementServiceServer::new(FakeKmsV1))
            .serve_with_incoming(UnixListenerStream::new(listener))
            .await
    })
}

async fn start_v2(path: &str) -> tokio::task::JoinHandle<Result<(), tonic::transport::Error>> {
    let listener = UnixListener::bind(Path::new(path)).expect("binding fake KMS v2 socket");
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(kms_v2::key_management_service_server::KeyManagementServiceServer::new(FakeKmsV2))
            .serve_with_incoming(UnixListenerStream::new(listener))
            .await
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kms_v1_uses_the_real_plugin_protocol_and_envelope() {
    let tempdir = tempfile::tempdir().expect("creating KMS v1 scratch directory");
    let socket = socket_path(&tempdir, "kms-v1.sock");
    let server = start_v1(&socket).await;
    let yaml = format!(
        "resources:\n- resources:\n  - secrets\n  providers:\n  - kms:\n      name: test-v1\n      apiVersion: v1\n      endpoint: unix://{socket}\n"
    );
    let config = parse(&yaml).expect("parsing KMS v1 config");
    let transformer = transformers_for(&config, "", "secrets").expect("secrets entry");
    let aad = b"/registry/secrets/default/kms-v1";
    let stored = transformer.transform_to_storage(b"secret-v1", aad).expect("KMS v1 write");
    assert!(stored.starts_with(b"k8s:enc:kms:v1:test-v1:"));
    let (plaintext, stale) = transformer.transform_from_storage(&stored, aad).expect("KMS v1 read");
    assert_eq!(plaintext, b"secret-v1");
    assert!(!stale);
    assert!(transformer.transform_from_storage(&stored, b"different-key").is_err());
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kms_v2_uses_the_real_plugin_protocol_and_encrypted_object() {
    let tempdir = tempfile::tempdir().expect("creating KMS v2 scratch directory");
    let socket = socket_path(&tempdir, "kms-v2.sock");
    let server = start_v2(&socket).await;
    let yaml = format!(
        "resources:\n- resources:\n  - secrets\n  providers:\n  - kms:\n      name: test-v2\n      apiVersion: v2\n      endpoint: unix://{socket}\n"
    );
    let config = parse(&yaml).expect("parsing KMS v2 config");
    let transformer = transformers_for(&config, "", "secrets").expect("secrets entry");
    let aad = b"/registry/secrets/default/kms-v2";
    let stored = transformer.transform_to_storage(b"secret-v2", aad).expect("KMS v2 write");
    assert!(stored.starts_with(b"k8s:enc:kms:v2:test-v2:"));
    let object = kms_v2::EncryptedObject::decode(&stored[b"k8s:enc:kms:v2:test-v2:".len()..]).expect("decoding KMS v2 envelope");
    assert_eq!(object.key_id, "test-kek-v1");
    assert_eq!(object.encrypted_dek_source_type, 1);
    let (plaintext, stale) = transformer.transform_from_storage(&stored, aad).expect("KMS v2 read");
    assert_eq!(plaintext, b"secret-v2");
    assert!(!stale);
    assert!(transformer.transform_from_storage(&stored, b"different-key").is_err());
    server.abort();
}
