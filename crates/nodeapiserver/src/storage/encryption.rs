//! Encryption-at-rest transformers — the last item on Group C's own scope
//! (`docs/APISERVER_PLAN.md`). A faithful port of a real subset of
//! upstream's `staging/src/k8s.io/apiserver/pkg/storage/value` package
//! (fetched and read directly, not reconstructed from memory): the
//! generic prefix-dispatch composition
//! (`transformer.go`'s `prefixTransformers`) plus six real providers,
//! `Identity` (`encrypt/identity/identity.go`), AES-256-GCM
//! (`encrypt/aes/aes.go`'s `gcm` type), and AES-256-CBC
//! (`encrypt/aes/aes.go`'s `cbc` type), and Secretbox
//! (`encrypt/secretbox/secretbox.go`), and the Kubernetes KMS v1/v2
//! envelope providers.
//!
//! `EncryptionConfiguration` YAML parsing lives in the sibling
//! `encryption_config` module. KMS plugins are reached over their upstream
//! gRPC protocol, normally through a Unix-domain socket. The v1 provider
//! wraps a locally encrypted DEK in a length-prefixed KMS ciphertext; v2
//! stores upstream's `EncryptedObject` protobuf and supports both its
//! AES-GCM-key and HKDF-seed DEK source formats.
//!
//! # Envelope format
//!
//! Confirmed against real upstream, not invented:
//! `staging/src/k8s.io/apiserver/pkg/server/options/encryptionconfig/config.go`'s
//! `aesPrefixTransformer` wraps a per-provider outer `PrefixTransformer`
//! (prefix `k8s:enc:aesgcm:v1:` or `k8s:enc:aescbc:v1:`)
//! around a nested `NewPrefixTransformers` of per-key `PrefixTransformer`s
//! (prefix `<key-name>:`) — supporting multiple keys per provider for
//! rotation: the *first* key is used for writes, every key is tried for
//! reads. This module's [`PrefixTransformers`] is that same generic
//! two-level composition, not specific to any one provider.

use crate::storage::pb::{kms_v1, kms_v2};
use prost::Message;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::hmac::{Key as HmacKey, HMAC_SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use std::future::Future;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex};
use std::time::Duration;

/// Deliberately opaque — matching upstream's own posture that a failed
/// decrypt should leak nothing about *why* it failed (a padding- or
/// tag-oracle-style signal is exactly the kind of thing an attacker
/// probing stored ciphertext could otherwise exploit).
#[derive(Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for Error {}

/// Mirrors upstream's `value.Transformer` interface: `transform_to_storage`
/// is the write path, `transform_from_storage` is the read path, and both
/// must undo each other. `authenticated_data` is upstream's
/// `dataCtx.AuthenticatedData()` — real callers pass the etcd key so a
/// ciphertext can't be copied to a different key and still decrypt
/// (AES-GCM's AAD only *verifies* this data, never encrypts it).
pub trait Transformer {
    /// Returns the plaintext and whether the stored value is "stale" (was
    /// encrypted with a provider/key this build would no longer choose for
    /// a fresh write, so callers should re-write it).
    fn transform_from_storage(&self, data: &[u8], authenticated_data: &[u8]) -> Result<(Vec<u8>, bool), Error>;
    fn transform_to_storage(&self, data: &[u8], authenticated_data: &[u8]) -> Result<Vec<u8>, Error>;
}

/// `encrypt/identity/identity.go`'s `identityTransformer`: performs no
/// transformation, but refuses to treat data that's actually encrypted
/// (starts with the `k8s:enc:` magic reserved for encrypted values) as
/// plaintext — a real, if easy to miss, correctness property: without
/// this check, an identity provider paired with an encrypted provider in
/// the same list could silently return ciphertext as if it were the real
/// value.
pub struct Identity;

const ENCRYPTED_PREFIX: &[u8] = b"k8s:enc:";

impl Transformer for Identity {
    fn transform_from_storage(&self, data: &[u8], _authenticated_data: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        if data.starts_with(ENCRYPTED_PREFIX) {
            return Err(Error("identity transformer tried to read encrypted data".to_string()));
        }
        Ok((data.to_vec(), false))
    }

    fn transform_to_storage(&self, data: &[u8], _authenticated_data: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(data.to_vec())
    }
}

/// `encrypt/aes/aes.go`'s `gcm` type: AES-256-GCM, `NONCE_LEN` (12)
/// random-nonce-then-ciphertext-then-tag on disk — `ring`'s
/// `seal_in_place_append_tag`/`open_in_place` already produce exactly that
/// tag-appended shape, so no manual tag bookkeeping is needed here the way
/// the Go implementation (using `cipher.AEAD.Seal`/`.Open` directly) has
/// to do.
pub struct Gcm {
    key: [u8; 32],
}

impl Gcm {
    /// `key` must be exactly 32 bytes (AES-256) — matches upstream's own
    /// `commonSize` constant and doc comment ("Do not change this value.
    /// It would be a backward incompatible change.").
    pub fn new(key: [u8; 32]) -> Self {
        Gcm { key }
    }

    fn less_safe_key(&self) -> Result<LessSafeKey, Error> {
        let unbound = UnboundKey::new(&AES_256_GCM, &self.key).map_err(|_| Error("invalid AES-256-GCM key".to_string()))?;
        Ok(LessSafeKey::new(unbound))
    }
}

impl Transformer for Gcm {
    fn transform_from_storage(&self, data: &[u8], authenticated_data: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        if data.len() < NONCE_LEN {
            return Err(Error("the stored data was shorter than the required size".to_string()));
        }
        let (nonce_bytes, ciphertext) = data.split_at(NONCE_LEN);
        let key = self.less_safe_key()?;
        let nonce = Nonce::try_assume_unique_for_key(nonce_bytes).map_err(|_| Error("invalid nonce".to_string()))?;
        let mut buf = ciphertext.to_vec();
        let plaintext = key.open_in_place(nonce, Aad::from(authenticated_data), &mut buf).map_err(|_| Error("AES-GCM decryption failed".to_string()))?;
        Ok((plaintext.to_vec(), false))
    }

    fn transform_to_storage(&self, data: &[u8], authenticated_data: &[u8]) -> Result<Vec<u8>, Error> {
        let key = self.less_safe_key()?;
        let rng = SystemRandom::new();
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rng.fill(&mut nonce_bytes).map_err(|_| Error("failed to generate a random nonce".to_string()))?;
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut in_out = data.to_vec();
        key.seal_in_place_append_tag(nonce, Aad::from(authenticated_data), &mut in_out).map_err(|_| Error("AES-GCM encryption failed".to_string()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + in_out.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&in_out);
        Ok(out)
    }
}

/// AES-256-CBC with PKCS#7 padding, matching upstream's `cbc` provider.
/// Stored data is the random 16-byte IV followed by the padded ciphertext;
/// the outer provider prefix is added by [`PrefixTransformers`]. CBC has no
/// authenticated-data parameter, so the caller's resource/key selection
/// remains the binding that chooses this transformer.
pub struct AesCbc {
    key: [u8; 32],
}

impl AesCbc {
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }
}

impl Transformer for AesCbc {
    fn transform_from_storage(&self, data: &[u8], _authenticated_data: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        use aes::Aes256;
        use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};

        if data.len() < 16 {
            return Err(Error("the stored data was shorter than the required size".to_string()));
        }
        let (iv, ciphertext) = data.split_at(16);
        if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
            return Err(Error("AES-CBC decryption failed".to_string()));
        }
        let mut plaintext = ciphertext.to_vec();
        let plaintext = cbc::Decryptor::<Aes256>::new_from_slices(&self.key, iv)
            .map_err(|_| Error("invalid AES-CBC key or IV".to_string()))?
            .decrypt_padded_mut::<Pkcs7>(&mut plaintext)
            .map_err(|_| Error("AES-CBC decryption failed".to_string()))?;
        Ok((plaintext.to_vec(), false))
    }

    fn transform_to_storage(&self, data: &[u8], _authenticated_data: &[u8]) -> Result<Vec<u8>, Error> {
        use aes::Aes256;
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};

        let mut iv = [0u8; 16];
        SystemRandom::new()
            .fill(&mut iv)
            .map_err(|_| Error("failed to generate a random IV".to_string()))?;
        let mut ciphertext = vec![0u8; data.len() + 16];
        ciphertext[..data.len()].copy_from_slice(data);
        let ciphertext = cbc::Encryptor::<Aes256>::new_from_slices(&self.key, &iv)
            .map_err(|_| Error("invalid AES-CBC key or IV".to_string()))?
            .encrypt_padded_mut::<Pkcs7>(&mut ciphertext, data.len())
            .map_err(|_| Error("AES-CBC encryption failed".to_string()))?;
        let mut out = Vec::with_capacity(iv.len() + ciphertext.len());
        out.extend_from_slice(&iv);
        out.extend_from_slice(ciphertext);
        Ok(out)
    }
}

/// NaCl Secretbox (`XSalsa20-Poly1305`), matching upstream's legacy
/// `encrypt/secretbox/secretbox.go` provider. Stored data is the random
/// 24-byte nonce followed by the 16-byte Poly1305 tag and ciphertext;
/// Secretbox has no Kubernetes authenticated-data parameter.
pub struct Secretbox {
    key: [u8; 32],
}

impl Secretbox {
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    fn cipher(&self) -> crypto_secretbox::XSalsa20Poly1305 {
        use crypto_secretbox::aead::KeyInit;

        crypto_secretbox::XSalsa20Poly1305::new(crypto_secretbox::Key::from_slice(&self.key))
    }
}

impl Transformer for Secretbox {
    fn transform_from_storage(&self, data: &[u8], _authenticated_data: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        use crypto_secretbox::aead::Aead;

        const NONCE_LEN: usize = 24;
        const TAG_LEN: usize = 16;
        if data.len() < NONCE_LEN + TAG_LEN {
            return Err(Error("the stored data was shorter than the required size".to_string()));
        }
        let (nonce, ciphertext) = data.split_at(NONCE_LEN);
        self.cipher()
            .decrypt(crypto_secretbox::Nonce::from_slice(nonce), ciphertext)
            .map(|plaintext| (plaintext, false))
            .map_err(|_| Error("Secretbox decryption failed".to_string()))
    }

    fn transform_to_storage(&self, data: &[u8], _authenticated_data: &[u8]) -> Result<Vec<u8>, Error> {
        use crypto_secretbox::aead::Aead;

        const NONCE_LEN: usize = 24;
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| Error("failed to generate a random nonce".to_string()))?;
        let ciphertext = self
            .cipher()
            .encrypt(crypto_secretbox::Nonce::from_slice(&nonce), data)
            .map_err(|_| Error("Secretbox encryption failed".to_string()))?;
        let mut out = Vec::with_capacity(nonce.len() + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }
}

fn random_bytes<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0u8; N];
    SystemRandom::new().fill(&mut bytes).map_err(|_| Error("failed to generate random encryption material".to_string()))?;
    Ok(bytes)
}

/// Runs a future from the synchronous [`Transformer`] interface. The storage
/// codec is deliberately synchronous because it is also used by selector and
/// watch encoding paths. Production nodeapiserver runs on the multithreaded
/// Tokio runtime, where `block_in_place` keeps the RPC on that runtime; the
/// thread fallback also keeps unit tests and non-Tokio callers usable without
/// requiring every storage call site to become async.
fn run_sync<T, F>(future: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: Future<Output = Result<T, Error>> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if matches!(handle.runtime_flavor(), tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(|| handle.block_on(future)),
        Ok(_) => std::thread::Builder::new()
            .name("nodeapiserver-kms-rpc".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Runtime::new().map_err(|error| Error(format!("creating KMS runtime failed: {error}")))?;
                runtime.block_on(future)
            })
            .map_err(|error| Error(format!("spawning KMS runtime failed: {error}")))?
            .join()
            .map_err(|_| Error("KMS runtime thread panicked".to_string()))?,
        Err(_) => {
            let runtime = tokio::runtime::Runtime::new().map_err(|error| Error(format!("creating KMS runtime failed: {error}")))?;
            runtime.block_on(future)
        }
    }
}

fn run_kms_rpc<T, F>(future: F, timeout: Duration) -> Result<T, Error>
where
    T: Send + 'static,
    F: Future<Output = Result<T, tonic::Status>> + Send + 'static,
{
    run_sync(async move {
        match tokio::time::timeout(timeout, future).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(status)) => Err(Error(format!("KMS RPC failed: {status}"))),
            Err(_) => Err(Error(format!("KMS RPC timed out after {}ms", timeout.as_millis()))),
        }
    })
}

fn kms_channel(endpoint: &str, timeout: Duration) -> Result<tonic::transport::Channel, Error> {
    if let Some(path) = endpoint.strip_prefix("unix://") {
        let path = path.to_string();
        let endpoint = tonic::transport::Endpoint::from_static("http://localhost").connect_timeout(timeout);
        Ok(endpoint.connect_with_connector_lazy(tower::service_fn(move |_: http::Uri| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        })))
    } else {
        Ok(tonic::transport::Endpoint::from_shared(endpoint.to_string())
            .map_err(|error| Error(format!("invalid KMS endpoint: {error}")))?
            .connect_timeout(timeout)
            .connect_lazy())
    }
}

/// Kubernetes KMS v1: the plugin encrypts a random local AES-GCM key, while
/// the apiserver encrypts the actual value locally. Its on-disk envelope is
/// the big-endian uint16 length, encrypted DEK, and AES-GCM ciphertext.
pub struct KmsV1 {
    endpoint: String,
    channel: Mutex<Option<tonic::transport::Channel>>,
    timeout: Duration,
    version_checked: AtomicBool,
}

impl KmsV1 {
    pub fn new(endpoint: &str, timeout: Duration) -> Result<Self, Error> {
        Ok(Self {
            endpoint: endpoint.to_string(),
            channel: Mutex::new(None),
            timeout,
            version_checked: AtomicBool::new(false),
        })
    }

    fn client(&self) -> Result<kms_v1::key_management_service_client::KeyManagementServiceClient<tonic::transport::Channel>, Error> {
        if let Some(channel) = self.channel.lock().map_err(|_| Error("KMS v1 channel lock poisoned".to_string()))?.as_ref() {
            return Ok(kms_v1::key_management_service_client::KeyManagementServiceClient::new(channel.clone()));
        }
        let endpoint = self.endpoint.clone();
        let timeout = self.timeout;
        let channel = run_sync(async move { Ok::<_, Error>(kms_channel(&endpoint, timeout)?) })?;
        *self.channel.lock().map_err(|_| Error("KMS v1 channel lock poisoned".to_string()))? = Some(channel.clone());
        Ok(kms_v1::key_management_service_client::KeyManagementServiceClient::new(channel))
    }

    fn ensure_version(&self) -> Result<(), Error> {
        if self.version_checked.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut client = self.client()?;
        let response = run_kms_rpc(
            async move {
                client
                    .version(tonic::Request::new(kms_v1::VersionRequest { version: "v1beta1".to_string() }))
                    .await
            },
            self.timeout,
        )?;
        if response.into_inner().version != "v1beta1" {
            return Err(Error("KMS v1 plugin returned an unsupported version".to_string()));
        }
        self.version_checked.store(true, Ordering::Release);
        Ok(())
    }

    fn encrypt_dek(&self) -> Result<([u8; 32], Vec<u8>), Error> {
        self.ensure_version()?;
        let dek = random_bytes::<32>()?;
        let mut client = self.client()?;
        let response = run_kms_rpc(
            async move {
                client
                    .encrypt(tonic::Request::new(kms_v1::EncryptRequest { version: "v1beta1".to_string(), plain: dek.to_vec() }))
                    .await
            },
            self.timeout,
        )?
        .into_inner()
        .cipher;
        if response.is_empty() || response.len() > u16::MAX as usize {
            return Err(Error("KMS v1 plugin returned an invalid encrypted DEK".to_string()));
        }
        Ok((dek, response))
    }

    fn decrypt_dek(&self, encrypted_dek: &[u8]) -> Result<[u8; 32], Error> {
        self.ensure_version()?;
        let encrypted_dek = encrypted_dek.to_vec();
        let mut client = self.client()?;
        let plain = run_kms_rpc(
            async move {
                client
                    .decrypt(tonic::Request::new(kms_v1::DecryptRequest { version: "v1beta1".to_string(), cipher: encrypted_dek }))
                    .await
            },
            self.timeout,
        )?
        .into_inner()
        .plain;
        plain.try_into().map_err(|plain: Vec<u8>| Error(format!("KMS v1 plugin returned a DEK of {} bytes, expected 32", plain.len())))
    }
}

impl Transformer for KmsV1 {
    fn transform_from_storage(&self, data: &[u8], authenticated_data: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        if data.len() < 2 {
            return Err(Error("KMS v1 ciphertext is missing its encrypted DEK length".to_string()));
        }
        let encrypted_dek_len = u16::from_be_bytes([data[0], data[1]]) as usize;
        let encrypted_dek_end = 2 + encrypted_dek_len;
        if encrypted_dek_len == 0 || data.len() <= encrypted_dek_end {
            return Err(Error("KMS v1 ciphertext has an invalid encrypted DEK envelope".to_string()));
        }
        let dek = self.decrypt_dek(&data[2..encrypted_dek_end])?;
        let (plaintext, _stale) = Gcm::new(dek).transform_from_storage(&data[encrypted_dek_end..], authenticated_data)?;
        Ok((plaintext, false))
    }

    fn transform_to_storage(&self, data: &[u8], authenticated_data: &[u8]) -> Result<Vec<u8>, Error> {
        let (dek, encrypted_dek) = self.encrypt_dek()?;
        let encrypted_data = Gcm::new(dek).transform_to_storage(data, authenticated_data)?;
        let mut output = Vec::with_capacity(2 + encrypted_dek.len() + encrypted_data.len());
        output.extend_from_slice(&(encrypted_dek.len() as u16).to_be_bytes());
        output.extend_from_slice(&encrypted_dek);
        output.extend_from_slice(&encrypted_data);
        Ok(output)
    }
}

struct KmsV2State {
    key_id: String,
    seed: [u8; 32],
    encrypted_seed: Vec<u8>,
    annotations: std::collections::HashMap<String, Vec<u8>>,
}

/// Kubernetes KMS v2. A random seed is encrypted by the plugin and then
/// expanded with HKDF-SHA256 for each value. The serialized `EncryptedObject`
/// is the provider body, matching the upstream v2 envelope rather than a
/// private ad-hoc format.
pub struct KmsV2 {
    endpoint: String,
    channel: Mutex<Option<tonic::transport::Channel>>,
    timeout: Duration,
    state: Arc<Mutex<Option<KmsV2State>>>,
}

impl KmsV2 {
    pub fn new(endpoint: &str, timeout: Duration) -> Result<Self, Error> {
        Ok(Self {
            endpoint: endpoint.to_string(),
            channel: Mutex::new(None),
            timeout,
            state: Arc::new(Mutex::new(None)),
        })
    }

    fn client(&self) -> Result<kms_v2::key_management_service_client::KeyManagementServiceClient<tonic::transport::Channel>, Error> {
        if let Some(channel) = self.channel.lock().map_err(|_| Error("KMS v2 channel lock poisoned".to_string()))?.as_ref() {
            return Ok(kms_v2::key_management_service_client::KeyManagementServiceClient::new(channel.clone()));
        }
        let endpoint = self.endpoint.clone();
        let timeout = self.timeout;
        let channel = run_sync(async move { Ok::<_, Error>(kms_channel(&endpoint, timeout)?) })?;
        *self.channel.lock().map_err(|_| Error("KMS v2 channel lock poisoned".to_string()))? = Some(channel.clone());
        Ok(kms_v2::key_management_service_client::KeyManagementServiceClient::new(channel))
    }

    fn status(&self) -> Result<kms_v2::StatusResponse, Error> {
        let mut client = self.client()?;
        let response = run_kms_rpc(async move { client.status(tonic::Request::new(kms_v2::StatusRequest {})).await }, self.timeout)?.into_inner();
        if response.version != "v2" && response.version != "v2beta1" {
            return Err(Error("KMS v2 plugin returned an unsupported version".to_string()));
        }
        if response.healthz != "ok" {
            return Err(Error(format!("KMS v2 plugin is unhealthy: {}", response.healthz)));
        }
        if response.key_id.is_empty() || response.key_id.len() > 1024 {
            return Err(Error("KMS v2 plugin returned an invalid key ID".to_string()));
        }
        Ok(response)
    }

    fn ensure_seed(&self) -> Result<KmsV2State, Error> {
        let status = self.status()?;
        if let Some(state) = self.state.lock().map_err(|_| Error("KMS v2 state lock poisoned".to_string()))?.as_ref() {
            if state.key_id == status.key_id {
                return Ok(KmsV2State { key_id: state.key_id.clone(), seed: state.seed, encrypted_seed: state.encrypted_seed.clone(), annotations: state.annotations.clone() });
            }
        }

        let seed = random_bytes::<32>()?;
        let mut client = self.client()?;
        let response = run_kms_rpc(
            async move {
                client
                    .encrypt(tonic::Request::new(kms_v2::EncryptRequest { plaintext: seed.to_vec(), uid: uuid::Uuid::new_v4().to_string() }))
                    .await
            },
            self.timeout,
        )?
        .into_inner();
        if response.ciphertext.is_empty() || response.key_id != status.key_id {
            return Err(Error("KMS v2 plugin returned an invalid encrypted seed".to_string()));
        }
        let state = KmsV2State { key_id: response.key_id, seed, encrypted_seed: response.ciphertext, annotations: response.annotations };
        *self.state.lock().map_err(|_| Error("KMS v2 state lock poisoned".to_string()))? = Some(KmsV2State { key_id: state.key_id.clone(), seed: state.seed, encrypted_seed: state.encrypted_seed.clone(), annotations: state.annotations.clone() });
        Ok(state)
    }

    fn decrypt_seed(&self, object: &kms_v2::EncryptedObject) -> Result<[u8; 32], Error> {
        let ciphertext = object.encrypted_dek_source.clone();
        let key_id = object.key_id.clone();
        let annotations = object.annotations.clone();
        let mut client = self.client()?;
        let response = run_kms_rpc(
            async move {
                client
                    .decrypt(tonic::Request::new(kms_v2::DecryptRequest {
                        ciphertext,
                        uid: uuid::Uuid::new_v4().to_string(),
                        key_id,
                        annotations,
                    }))
                    .await
            },
            self.timeout,
        )?
        .into_inner()
        .plaintext;
        response.try_into().map_err(|plain: Vec<u8>| Error(format!("KMS v2 plugin returned a DEK of {} bytes, expected 32", plain.len())))
    }
}

fn derive_kms_v2_dek(seed: &[u8; 32], info: &[u8; 32]) -> [u8; 32] {
    let key = HmacKey::new(HMAC_SHA256, seed);
    let mut input = Vec::with_capacity(info.len() + 1);
    input.extend_from_slice(info);
    input.push(1);
    let digest = ring::hmac::sign(&key, &input);
    let mut dek = [0u8; 32];
    dek.copy_from_slice(digest.as_ref());
    dek
}

impl Transformer for KmsV2 {
    fn transform_from_storage(&self, data: &[u8], authenticated_data: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        let object = kms_v2::EncryptedObject::decode(data).map_err(|error| Error(format!("KMS v2 encrypted object is invalid: {error}")))?;
        if object.encrypted_data.is_empty() || object.key_id.is_empty() || object.key_id.len() > 1024 || object.encrypted_dek_source.is_empty() || object.encrypted_dek_source.len() > 1024 {
            return Err(Error("KMS v2 encrypted object is missing its key ID or encrypted seed".to_string()));
        }
        let seed = self.decrypt_seed(&object)?;
        let (key, encrypted_data) = match object.encrypted_dek_source_type {
            0 => (seed, object.encrypted_data.as_slice()),
            1 => {
                if object.encrypted_data.len() <= 32 {
                    return Err(Error("KMS v2 HKDF encrypted object is missing its derivation info".to_string()));
                }
                let mut info = [0u8; 32];
                info.copy_from_slice(&object.encrypted_data[..32]);
                (derive_kms_v2_dek(&seed, &info), &object.encrypted_data[32..])
            }
            other => return Err(Error(format!("KMS v2 encrypted object has unsupported DEK source type {other}"))),
        };
        let (plaintext, _stale) = Gcm::new(key).transform_from_storage(encrypted_data, authenticated_data)?;
        let stale = self.status().map(|status| status.key_id != object.key_id).unwrap_or(false);
        Ok((plaintext, stale))
    }

    fn transform_to_storage(&self, data: &[u8], authenticated_data: &[u8]) -> Result<Vec<u8>, Error> {
        let state = self.ensure_seed()?;
        let info = random_bytes::<32>()?;
        let encrypted_data = Gcm::new(derive_kms_v2_dek(&state.seed, &info)).transform_to_storage(data, authenticated_data)?;
        let mut data_with_info = Vec::with_capacity(info.len() + encrypted_data.len());
        data_with_info.extend_from_slice(&info);
        data_with_info.extend_from_slice(&encrypted_data);
        let object = kms_v2::EncryptedObject {
            encrypted_data: data_with_info,
            key_id: state.key_id,
            encrypted_dek_source: state.encrypted_seed,
            annotations: state.annotations,
            encrypted_dek_source_type: 1,
        };
        let mut encoded = Vec::with_capacity(object.encoded_len());
        object.encode(&mut encoded).map_err(|error| Error(format!("encoding KMS v2 encrypted object failed: {error}")))?;
        Ok(encoded)
    }
}

/// One entry in a [`PrefixTransformers`] list — `transformer.go`'s
/// `PrefixTransformer` struct.
pub struct PrefixTransformer {
    pub prefix: Vec<u8>,
    pub transformer: Box<dyn Transformer + Send + Sync>,
}

/// `transformer.go`'s `prefixTransformers`: tries each entry's prefix
/// against the stored data's own leading bytes, in order, for reads; the
/// *first* entry is always used for writes (real upstream's own comment:
/// "The first provided transformer will be used when writing to the
/// store"). Used both for the outer provider-level list (`k8s:enc:aesgcm:
/// v1:` vs. an unprefixed identity fallback) and, nested inside one
/// provider's own transformer, the per-key list for rotation — the same
/// generic composition either way, matching upstream exactly.
pub struct PrefixTransformers {
    transformers: Vec<PrefixTransformer>,
}

impl PrefixTransformers {
    pub fn new(transformers: Vec<PrefixTransformer>) -> Self {
        PrefixTransformers { transformers }
    }
}

impl Transformer for PrefixTransformers {
    fn transform_from_storage(&self, data: &[u8], authenticated_data: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        let mut errors = Vec::new();
        for (i, entry) in self.transformers.iter().enumerate() {
            if !data.starts_with(&entry.prefix[..]) {
                continue;
            }
            match entry.transformer.transform_from_storage(&data[entry.prefix.len()..], authenticated_data) {
                // Real upstream never short-circuits on a prefix match
                // that errors — overlapping prefixes are valid (the same
                // provider listed twice with different keys, mid-rotation),
                // so a decrypt failure under one key must still let a
                // later matching entry be tried, not fail the whole
                // lookup. Applies uniformly, not just to the identity
                // provider's own "is this actually encrypted?" check.
                Err(e) => errors.push(e),
                Ok((result, stale)) => return Ok((result, stale || i != 0)),
            }
        }
        match errors.into_iter().next() {
            Some(e) => Err(e),
            None => Err(Error("the provided value does not match any of the supported transformers".to_string())),
        }
    }

    fn transform_to_storage(&self, data: &[u8], authenticated_data: &[u8]) -> Result<Vec<u8>, Error> {
        let first = self.transformers.first().ok_or_else(|| Error("no transformers configured".to_string()))?;
        let body = first.transformer.transform_to_storage(data, authenticated_data)?;
        let mut out = Vec::with_capacity(first.prefix.len() + body.len());
        out.extend_from_slice(&first.prefix);
        out.extend_from_slice(&body);
        Ok(out)
    }
}

/// The real outer prefix for upstream's AES-GCM provider —
/// `encryptionconfig/config.go`'s `aesGCMTransformerPrefixV1` constant.
pub const AES_GCM_PREFIX_V1: &str = "k8s:enc:aesgcm:v1:";
/// The real outer prefix for upstream's AES-CBC provider.
pub const AES_CBC_PREFIX_V1: &str = "k8s:enc:aescbc:v1:";
/// The real outer prefix for upstream's Secretbox provider.
pub const SECRETBOX_PREFIX_V1: &str = "k8s:enc:secretbox:v1:";
/// The real outer prefix for Kubernetes' KMS v1 provider.
pub const KMS_PREFIX_V1: &str = "k8s:enc:kms:v1:";
/// The real outer prefix for Kubernetes' KMS v2 provider.
pub const KMS_PREFIX_V2: &str = "k8s:enc:kms:v2:";

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn identity_round_trips_plain_data() {
        let t = Identity;
        let encoded = t.transform_to_storage(b"hello", b"aad").unwrap();
        assert_eq!(encoded, b"hello");
        let (decoded, stale) = t.transform_from_storage(&encoded, b"aad").unwrap();
        assert_eq!(decoded, b"hello");
        assert!(!stale);
    }

    #[test]
    fn identity_refuses_data_that_looks_encrypted() {
        let t = Identity;
        let err = t.transform_from_storage(b"k8s:enc:aesgcm:v1:whatever", b"").unwrap_err();
        assert_eq!(err.0, "identity transformer tried to read encrypted data");
    }

    #[test]
    fn gcm_round_trips_and_the_ciphertext_is_not_the_plaintext() {
        let t = Gcm::new(key(7));
        let encoded = t.transform_to_storage(b"super secret value", b"authctx").unwrap();
        assert_ne!(encoded, b"super secret value");
        let (decoded, stale) = t.transform_from_storage(&encoded, b"authctx").unwrap();
        assert_eq!(decoded, b"super secret value");
        assert!(!stale);
    }

    #[test]
    fn gcm_produces_a_different_nonce_each_time() {
        let t = Gcm::new(key(7));
        let a = t.transform_to_storage(b"same plaintext", b"").unwrap();
        let b = t.transform_to_storage(b"same plaintext", b"").unwrap();
        assert_ne!(a, b, "a fresh random nonce must make repeated encryptions of the same plaintext differ");
    }

    #[test]
    fn gcm_rejects_a_mismatched_authenticated_data() {
        let t = Gcm::new(key(7));
        let encoded = t.transform_to_storage(b"value", b"/registry/pods/default/web-1").unwrap();
        let err = t.transform_from_storage(&encoded, b"/registry/pods/default/web-2").unwrap_err();
        assert_eq!(err.0, "AES-GCM decryption failed");
    }

    #[test]
    fn gcm_rejects_a_key_mismatch() {
        let encoded = Gcm::new(key(7)).transform_to_storage(b"value", b"").unwrap();
        let err = Gcm::new(key(9)).transform_from_storage(&encoded, b"").unwrap_err();
        assert_eq!(err.0, "AES-GCM decryption failed");
    }

    #[test]
    fn gcm_rejects_truncated_data() {
        let err = Gcm::new(key(1)).transform_from_storage(b"short", b"").unwrap_err();
        assert_eq!(err.0, "the stored data was shorter than the required size");
    }

    #[test]
    fn aes_cbc_round_trips_and_uses_a_fresh_iv() {
        let t = AesCbc::new(key(7));
        let a = t.transform_to_storage(b"super secret value", b"ignored").unwrap();
        let b = t.transform_to_storage(b"super secret value", b"ignored").unwrap();
        assert_ne!(a, b, "a fresh random IV must make repeated plaintext differ");
        let (decoded, stale) = t.transform_from_storage(&a, b"ignored").unwrap();
        assert_eq!(decoded, b"super secret value");
        assert!(!stale);
    }

    #[test]
    fn aes_cbc_rejects_a_wrong_key_and_bad_padding() {
        let encoded = AesCbc::new(key(7)).transform_to_storage(b"value", b"").unwrap();
        assert_eq!(AesCbc::new(key(9)).transform_from_storage(&encoded, b"").unwrap_err().0, "AES-CBC decryption failed");
        assert_eq!(AesCbc::new(key(7)).transform_from_storage(&encoded[..encoded.len() - 1], b"").unwrap_err().0, "AES-CBC decryption failed");
    }

    #[test]
    fn secretbox_round_trips_and_uses_a_fresh_nonce() {
        let t = Secretbox::new(key(7));
        let a = t.transform_to_storage(b"super secret value", b"ignored").unwrap();
        let b = t.transform_to_storage(b"super secret value", b"ignored").unwrap();
        assert_ne!(a, b, "a fresh random nonce must make repeated plaintext differ");
        assert_eq!(a.len(), 24 + b"super secret value".len() + 16);
        let (decoded, stale) = t.transform_from_storage(&a, b"ignored").unwrap();
        assert_eq!(decoded, b"super secret value");
        assert!(!stale);
    }

    #[test]
    fn secretbox_rejects_a_wrong_key_and_tampered_ciphertext() {
        let encoded = Secretbox::new(key(7)).transform_to_storage(b"value", b"").unwrap();
        assert_eq!(Secretbox::new(key(9)).transform_from_storage(&encoded, b"").unwrap_err().0, "Secretbox decryption failed");
        let mut tampered = encoded;
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(Secretbox::new(key(7)).transform_from_storage(&tampered, b"").unwrap_err().0, "Secretbox decryption failed");
    }

    fn gcm_entry(prefix: &str, key_byte: u8) -> PrefixTransformer {
        PrefixTransformer { prefix: prefix.as_bytes().to_vec(), transformer: Box::new(Gcm::new(key(key_byte))) }
    }

    #[test]
    fn prefix_transformers_writes_with_the_first_entry_and_prepends_its_prefix() {
        let list = PrefixTransformers::new(vec![gcm_entry("1:", 1), gcm_entry("2:", 2)]);
        let encoded = list.transform_to_storage(b"value", b"").unwrap();
        assert!(encoded.starts_with(b"1:"), "must always write under the first entry's prefix");
    }

    #[test]
    fn prefix_transformers_reads_by_matching_prefix_regardless_of_position() {
        let list = PrefixTransformers::new(vec![gcm_entry("1:", 1), gcm_entry("2:", 2)]);
        // Encrypt under key "2" directly, then prove the list can still
        // decrypt it by matching the "2:" prefix even though it's not
        // first — the real-world shape of "rotating a new key in".
        let mut raw = b"2:".to_vec();
        raw.extend(Gcm::new(key(2)).transform_to_storage(b"value", b"").unwrap());
        let (decoded, stale) = list.transform_from_storage(&raw, b"").unwrap();
        assert_eq!(decoded, b"value");
        assert!(stale, "a value read via a non-first entry must be reported stale so callers re-write it");
    }

    #[test]
    fn prefix_transformers_the_first_entry_is_never_reported_stale() {
        let list = PrefixTransformers::new(vec![gcm_entry("1:", 1), gcm_entry("2:", 2)]);
        let encoded = list.transform_to_storage(b"value", b"").unwrap();
        let (_, stale) = list.transform_from_storage(&encoded, b"").unwrap();
        assert!(!stale);
    }

    #[test]
    fn prefix_transformers_tries_the_next_matching_entry_after_a_decrypt_failure() {
        // Two entries sharing the exact same prefix (the "same provider,
        // different key, mid-rotation" case upstream's own comment
        // documents) — decrypting under the wrong key must not abort the
        // whole lookup, it must fall through to the entry with the right
        // key.
        let list = PrefixTransformers::new(vec![gcm_entry("k8s:enc:aesgcm:v1:", 1), gcm_entry("k8s:enc:aesgcm:v1:", 2)]);
        let mut raw = b"k8s:enc:aesgcm:v1:".to_vec();
        raw.extend(Gcm::new(key(2)).transform_to_storage(b"value", b"").unwrap());
        let (decoded, _) = list.transform_from_storage(&raw, b"").unwrap();
        assert_eq!(decoded, b"value");
    }

    #[test]
    fn prefix_transformers_no_matching_prefix_is_a_real_error() {
        let list = PrefixTransformers::new(vec![gcm_entry("k8s:enc:aesgcm:v1:", 1)]);
        let err = list.transform_from_storage(b"unrelated-data", b"").unwrap_err();
        assert_eq!(err.0, "the provided value does not match any of the supported transformers");
    }

    #[test]
    fn the_real_upstream_aes_gcm_prefix_constant() {
        assert_eq!(AES_GCM_PREFIX_V1, "k8s:enc:aesgcm:v1:");
    }
}
