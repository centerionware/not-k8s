//! certificatesigningrequest-{approving,signing,cleaner}-controller (Group
//! I): approves, signs, and eventually deletes `CertificateSigningRequest`
//! objects for the one signer this project's own stack actually requests —
//! `kubernetes.io/kube-apiserver-client-kubelet`, nodelet's own TLS
//! bootstrap flow (`crates/nodelet/src/bootstrap.rs`, active only when
//! `NODELET_BOOTSTRAP_KUBECONFIG` is set). Without this controller, a node
//! bootstrapping via that flow submits a CSR and then waits forever —
//! nodelet's own doc comment says plainly "approval is entirely the
//! apiserver's job, same as upstream," and upstream's job is exactly this
//! controller.
//!
//! # The one real external dependency in this whole crate
//!
//! Signing needs the cluster CA's **private key**, not just its cert (every
//! other controller in this crate only ever needs the cert, read from the
//! ambient kubeconfig — see `root_ca_publisher.rs`). This is the one
//! documented exception: `NODECONTROLLER_CSR_SIGNING_CA_CERT_PATH`/
//! `NODECONTROLLER_CSR_SIGNING_CA_KEY_PATH` (`config.rs`) let an operator
//! point at any control plane's CA explicitly; left unset, this controller
//! tries each well-known `(cert, key)` path pair in
//! `config.rs`'s `defaults::CSR_SIGNING_CA_CANDIDATES` in order — today
//! that list has exactly one entry, k3s's own on-disk CA
//! (`/var/lib/rancher/k3s/server/tls/server-ca.{crt,key}`, confirmed as a
//! live operational path by the unmerged, profiling-only
//! `upstream-kube-apiserver-controller-manager` branch, which had to source
//! the same files) — deliberately a list, not a single hardcoded default,
//! since this project won't run on k3s forever and a future control plane
//! just adds its own pair rather than needing a bigger refactor. If none of
//! the candidates (or an explicit override) is readable, signing silently
//! does nothing rather than crashing the whole process — approving and
//! cleaning still work, and a CSR just sits `Approved` with no certificate,
//! the same degraded state as if this controller didn't exist at all.
//!
//! # Scope of this slice
//!
//! **Only `kubernetes.io/kube-apiserver-client-kubelet`** — the other two
//! well-known signers (`kubernetes.io/kubelet-serving`,
//! `kubernetes.io/kube-apiserver-client`) are never requested by anything
//! in this project's own stack, so there's nothing to verify approval
//! logic for them against; a CSR for any other signer name is left alone
//! entirely, the same as upstream leaves a third-party signer's CSRs for
//! that signer's own controller.
//!
//! **Approval is a group-membership check, not a real
//! `SubjectAccessReview`.** Upstream's own `csrapproving-controller` asks
//! "can the requesting user `create` `certificatesigningrequests/nodeclient`"
//! via SAR — the real security boundary is RBAC on that permission, bound
//! to the `system:bootstrappers` group by the standard bootstrap-token
//! addon. This controller approximates that by checking `spec.groups`
//! (apiserver-populated from the authenticated identity, not
//! attacker-controlled) for `system:bootstrappers` directly, skipping the
//! SAR round-trip — same effective security property in this project's
//! deployment (nothing else grants that group `certificatesigningrequests`
//! write access), a real simplification if RBAC is ever hand-edited to grant
//! that group to something else.
//!
//! **No `expirationSeconds` honoring** — every issued certificate gets
//! rcgen's own default validity rather than the CSR's requested duration
//! clamped to a configured maximum (`--cluster-signing-duration` upstream).
//! Real, named gap.
//!
//! **The cleaner is a flat periodic scan**, same "plain heap tier, no
//! wheel" reasoning `ttl_after_finished.rs` documents — deletes a CSR once
//! it reached a terminal condition (`Approved` with a certificate issued,
//! `Denied`, or `Failed`) more than an hour ago, one flat threshold rather
//! than upstream's slightly different windows per outcome.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use k8s_openapi::api::certificates::v1::{CertificateSigningRequest, CertificateSigningRequestCondition};
use kube::api::{Api, DeleteParams, Patch, PatchParams};
use kube::runtime::watcher::Event;
use kube::{Client, ResourceExt};
use std::collections::HashMap;
use std::time::Duration as StdDuration;

const KUBELET_CLIENT_SIGNER: &str = "kubernetes.io/kube-apiserver-client-kubelet";
const BOOTSTRAP_GROUP: &str = "system:bootstrappers";
const CLEANUP_AGE: chrono::Duration = chrono::Duration::hours(1);
const TICK_PERIOD: StdDuration = StdDuration::from_secs(60);

/// Should this CSR be auto-approved? Pure given the fields the apiserver
/// itself populated on creation (never attacker-controlled) — see module
/// doc for why a group check stands in for a real `SubjectAccessReview`.
pub fn should_auto_approve(signer_name: &str, groups: &[String], usages: &[String]) -> bool {
    signer_name == KUBELET_CLIENT_SIGNER && groups.iter().any(|g| g == BOOTSTRAP_GROUP) && usages.iter().any(|u| u == "client auth")
}

pub fn already_decided(conditions: &[CertificateSigningRequestCondition]) -> bool {
    conditions.iter().any(|c| matches!(c.type_.as_str(), "Approved" | "Denied" | "Failed"))
}

/// Approved, but no certificate issued yet — exactly what the signing half
/// waits for.
pub fn needs_signing(conditions: &[CertificateSigningRequestCondition], has_certificate: bool) -> bool {
    !has_certificate && conditions.iter().any(|c| c.type_ == "Approved" && c.status == "True")
}

/// A terminal CSR (issued, denied, or failed) past `CLEANUP_AGE` since its
/// deciding condition — pure given the already-known "when did this become
/// terminal" instant, so the age arithmetic is testable without a live
/// clock or object.
pub fn is_due_for_cleanup(terminal_since: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now - terminal_since >= CLEANUP_AGE
}

/// The instant a CSR became terminal — the latest
/// `last_transition_time` among its `Approved`/`Denied`/`Failed`
/// conditions, or `None` if it isn't terminal yet.
fn terminal_since(conditions: &[CertificateSigningRequestCondition]) -> Option<DateTime<Utc>> {
    conditions
        .iter()
        .filter(|c| matches!(c.type_.as_str(), "Approved" | "Denied" | "Failed") && c.status == "True")
        .filter_map(|c| c.last_transition_time.as_ref().and_then(crate::k8s_time::to_chrono))
        .max()
}

fn condition(type_: &str, reason: &str, message: &str) -> CertificateSigningRequestCondition {
    let now = crate::k8s_time::from_chrono(crate::k8s_time::now());
    CertificateSigningRequestCondition {
        type_: type_.to_string(),
        status: "True".to_string(),
        reason: Some(reason.to_string()),
        message: Some(message.to_string()),
        last_transition_time: Some(now.clone()),
        last_update_time: Some(now),
    }
}

struct SigningCa {
    cert: rcgen::Certificate,
    key: rcgen::KeyPair,
}

/// Tries each `(cert, key)` candidate in order (see
/// `Config::csr_signing_ca_candidates()` — the explicit env-var override
/// alone if set, otherwise every well-known path this project's supported
/// control planes might use) and loads the first pair that's actually
/// present and parses. Every candidate's outcome is logged at `warn` (not
/// just failures) — with today's single-entry candidate list a "missing"
/// result is exactly the signal an operator needs to diagnose a
/// misconfigured mount, and `debug`-level noise would be filtered out
/// under this project's own `RUST_LOG=info` default, silently hiding it.
fn load_signing_ca(cfg: &crate::config::Config) -> Option<SigningCa> {
    for (cert_path, key_path) in cfg.csr_signing_ca_candidates() {
        let cert_pem = match std::fs::read_to_string(&cert_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %cert_path, error = ?e, "csr-signing-controller: candidate CA cert not present, trying the next one");
                continue;
            }
        };
        let key_pem = match std::fs::read_to_string(&key_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %key_path, error = ?e, "csr-signing-controller: candidate CA key not present, trying the next one");
                continue;
            }
        };
        let key = match rcgen::KeyPair::from_pem(&key_pem) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(path = %key_path, error = ?e, "csr-signing-controller: found a CA key but failed to parse it");
                continue;
            }
        };
        let params = match rcgen::CertificateParams::from_ca_cert_pem(&cert_pem) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(path = %cert_path, error = ?e, "csr-signing-controller: found a CA cert but failed to parse it");
                continue;
            }
        };
        let cert = match params.self_signed(&key) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %cert_path, error = ?e, "csr-signing-controller: failed to load the CA as a self-signed issuer");
                continue;
            }
        };
        tracing::info!(cert_path = %cert_path, "csr-signing-controller: loaded the cluster CA, signing is active");
        return Some(SigningCa { cert, key });
    }
    None
}

fn sign_csr(ca: &SigningCa, csr_pem: &str) -> Result<String> {
    let params = rcgen::CertificateSigningRequestParams::from_pem(csr_pem).context("parsing CSR PEM")?;
    let signed = params.signed_by(&ca.cert, &ca.key).context("signing CSR with the cluster CA")?;
    Ok(signed.pem())
}

async fn approve(client: &Client, name: &str, existing: &[CertificateSigningRequestCondition]) {
    let mut conditions = existing.to_vec();
    conditions.push(condition("Approved", "AutoApproved", "Auto-approved by nodecontroller's certificatesigningrequest-approving-controller"));
    let patch = serde_json::json!({ "status": { "conditions": conditions } });
    let bytes = match serde_json::to_vec(&patch) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(csr = %name, error = ?e, "failed to serialize CSR approval patch");
            return;
        }
    };
    let req = match http::Request::builder()
        .method("PATCH")
        .uri(format!("/apis/certificates.k8s.io/v1/certificatesigningrequests/{name}/approval"))
        .header("Content-Type", "application/merge-patch+json")
        .body(bytes)
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(csr = %name, error = ?e, "failed to build CSR approval request");
            return;
        }
    };
    match client.request::<serde_json::Value>(req).await {
        Ok(_) => tracing::info!(csr = %name, "certificatesigningrequest-approving-controller auto-approved a kubelet-client CSR"),
        Err(e) => tracing::warn!(csr = %name, error = ?e, "failed to approve CSR"),
    }
}

async fn reconcile_csr(client: &Client, ca: &Option<SigningCa>, name: &str) {
    let api: Api<CertificateSigningRequest> = Api::all(client.clone());
    let csr = match api.get_opt(name).await {
        Ok(Some(c)) => c,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(csr = %name, error = ?e, "failed to read CertificateSigningRequest for reconcile");
            return;
        }
    };
    if csr.spec.signer_name != KUBELET_CLIENT_SIGNER {
        return;
    }
    let conditions = csr.status.as_ref().and_then(|s| s.conditions.clone()).unwrap_or_default();
    let has_certificate = csr.status.as_ref().is_some_and(|s| s.certificate.is_some());

    if !already_decided(&conditions) {
        let groups = csr.spec.groups.clone().unwrap_or_default();
        let usages = csr.spec.usages.clone().unwrap_or_default();
        if should_auto_approve(&csr.spec.signer_name, &groups, &usages) {
            approve(client, name, &conditions).await;
        }
        return;
    }

    if !needs_signing(&conditions, has_certificate) {
        return;
    }
    let Some(ca) = ca else {
        tracing::warn!(csr = %name, "CertificateSigningRequest is Approved and needs signing, but no CA was loaded at startup — see this process's earlier startup logs for why");
        return;
    };
    let Ok(csr_pem) = String::from_utf8(csr.spec.request.0.clone()) else {
        tracing::warn!(csr = %name, "CSR request field is not valid UTF-8 PEM");
        return;
    };
    match sign_csr(ca, &csr_pem) {
        Ok(cert_pem) => {
            let status_patch = serde_json::json!({ "status": { "certificate": base64_pem(&cert_pem) } });
            if let Err(e) = api.patch_status(name, &PatchParams::default(), &Patch::Merge(&status_patch)).await {
                tracing::warn!(csr = %name, error = ?e, "failed to patch issued certificate onto CertificateSigningRequest");
            } else {
                tracing::info!(csr = %name, "certificatesigningrequest-signing-controller issued a certificate");
            }
        }
        Err(e) => tracing::warn!(csr = %name, error = ?e, "failed to sign CertificateSigningRequest"),
    }
}

/// `status.certificate` is a `ByteString`, which serializes to base64 —
/// serde_json's own `Serialize` for `k8s_openapi::ByteString` handles this,
/// but since we're building the patch as a bare JSON value (no typed
/// `CertificateSigningRequestStatus` round-trip needed for one field),
/// base64-encode directly.
fn base64_pem(pem: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(pem.as_bytes())
}

async fn sweep_cleanup(client: &Client, csrs: &HashMap<String, CertificateSigningRequest>) {
    let now = Utc::now();
    for csr in csrs.values() {
        let conditions = csr.status.as_ref().and_then(|s| s.conditions.as_ref()).cloned().unwrap_or_default();
        let Some(since) = terminal_since(&conditions) else { continue };
        if !is_due_for_cleanup(since, now) {
            continue;
        }
        let name = csr.name_any();
        let api: Api<CertificateSigningRequest> = Api::all(client.clone());
        match api.delete(&name, &DeleteParams::default()).await {
            Ok(_) => tracing::info!(csr = %name, "certificatesigningrequest-cleaner-controller deleted a terminal CSR past its age threshold"),
            Err(kube::Error::Api(ref e)) if e.is_not_found() => {}
            Err(e) => tracing::warn!(csr = %name, error = ?e, "failed to delete terminal CSR"),
        }
    }
}

pub async fn run(client: Client, cfg: &crate::config::Config) -> Result<()> {
    let ca = load_signing_ca(cfg);
    if ca.is_none() {
        tracing::warn!("certificatesigningrequest-signing-controller starting without a usable CA — approval and cleanup still work, signing will not");
    }

    let mut csrs: HashMap<String, CertificateSigningRequest> = HashMap::new();
    let api: Api<CertificateSigningRequest> = Api::all(client.clone());
    for c in api.list(&Default::default()).await.context("listing CertificateSigningRequests to seed csr controllers")?.items {
        let name = c.name_any();
        csrs.insert(name.clone(), c);
        reconcile_csr(&client, &ca, &name).await;
    }

    let mut stream = crate::watch::watch_certificate_signing_requests(&client);
    let mut ticker = tokio::time::interval(TICK_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            ev = stream.next() => {
                match ev {
                    Some(Ok(Event::Apply(csr))) | Some(Ok(Event::InitApply(csr))) => {
                        let name = csr.name_any();
                        csrs.insert(name.clone(), csr);
                        reconcile_csr(&client, &ca, &name).await;
                    }
                    Some(Ok(Event::Delete(csr))) => { csrs.remove(&csr.name_any()); }
                    Some(Ok(Event::Init | Event::InitDone)) => {}
                    Some(Err(e)) => tracing::warn!(error = ?e, "watch error in csr controllers"),
                    None => return Ok(()),
                }
            }
            _ = ticker.tick() => {
                sweep_cleanup(&client, &csrs).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dt(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    #[test]
    fn approves_a_bootstrapper_client_auth_csr() {
        assert!(should_auto_approve(KUBELET_CLIENT_SIGNER, &["system:bootstrappers".to_string()], &["client auth".to_string()]));
    }

    #[test]
    fn rejects_the_wrong_signer() {
        assert!(!should_auto_approve("kubernetes.io/kubelet-serving", &["system:bootstrappers".to_string()], &["client auth".to_string()]));
    }

    #[test]
    fn rejects_without_the_bootstrap_group() {
        assert!(!should_auto_approve(KUBELET_CLIENT_SIGNER, &["some-other-group".to_string()], &["client auth".to_string()]));
    }

    #[test]
    fn rejects_without_client_auth_usage() {
        assert!(!should_auto_approve(KUBELET_CLIENT_SIGNER, &["system:bootstrappers".to_string()], &["server auth".to_string()]));
    }

    fn cond(type_: &str, status: &str) -> CertificateSigningRequestCondition {
        CertificateSigningRequestCondition {
            type_: type_.to_string(),
            status: status.to_string(),
            last_transition_time: None,
            last_update_time: None,
            reason: None,
            message: None,
        }
    }

    #[test]
    fn undecided_csr_has_no_terminal_condition() {
        assert!(!already_decided(&[]));
        assert!(!already_decided(&[cond("Approved", "False")]));
    }

    #[test]
    fn a_true_approved_condition_counts_as_decided() {
        assert!(already_decided(&[cond("Approved", "True")]));
        assert!(already_decided(&[cond("Denied", "True")]));
    }

    #[test]
    fn approved_without_a_certificate_needs_signing() {
        assert!(needs_signing(&[cond("Approved", "True")], false));
        assert!(!needs_signing(&[cond("Approved", "True")], true));
        assert!(!needs_signing(&[cond("Denied", "True")], false));
    }

    #[test]
    fn cleanup_age_threshold() {
        assert!(!is_due_for_cleanup(dt(0), dt(1000)));
        assert!(is_due_for_cleanup(dt(0), dt(3600)));
        assert!(is_due_for_cleanup(dt(0), dt(7200)));
    }
}
