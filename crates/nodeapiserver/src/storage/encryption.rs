//! Encryption-at-rest transformers — the last item on Group C's own scope
//! (`docs/APISERVER_PLAN.md`). A faithful port of a real subset of
//! upstream's `staging/src/k8s.io/apiserver/pkg/storage/value` package
//! (fetched and read directly, not reconstructed from memory): the
//! generic prefix-dispatch composition
//! (`transformer.go`'s `prefixTransformers`) plus three real providers,
//! `Identity` (`encrypt/identity/identity.go`), AES-256-GCM
//! (`encrypt/aes/aes.go`'s `gcm` type), and AES-256-CBC
//! (`encrypt/aes/aes.go`'s `cbc` type).
//!
//! # What this deliberately doesn't cover yet
//!
//! Secretbox and KMS (v1/v2) are real, separate providers upstream also has
//! and remain outside this module's scope. KMS needs a gRPC plugin protocol
//! this crate hasn't vendored; secretbox needs a separate NaCl-compatible
//! implementation. `EncryptionConfiguration` YAML parsing lives in the
//! sibling `encryption_config` module.
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

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

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
