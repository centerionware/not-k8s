//! A small throwaway-rig test for the real nodeapiserver process.
//!
//! The other nodeapiserver integration tests exercise the REST layer directly
//! against a real nodestore. This test crosses the process and TLS boundary as
//! well: it starts both binaries, waits for live `/readyz`, and drives the
//! listener with a real HTTP client through discovery and generic CRUD. This
//! is intentionally cheap enough to run with the normal nodeapiserver test
//! target; the full cluster e2e remains the final nodeapiserver-to-main gate.

use rcgen::generate_simple_self_signed;
use reqwest::Client;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};

fn find_binary(name: &str) -> Option<PathBuf> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .to_path_buf();
    let candidates = [
        repo_root.join(format!("target/debug/{name}")),
        repo_root.join(format!("target/release/{name}")),
        repo_root.join(format!("bin/{name}")),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

fn find_nodestore_binary() -> Option<PathBuf> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .to_path_buf();
    let candidates = [
        repo_root.join("target/debug/nodestore"),
        repo_root.join("target/release/nodestore"),
        repo_root.join("bin/nodestore"),
    ];
    if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
        return Some(path);
    }
    if !repo_root.join("crates/nodestore").is_dir() {
        return None;
    }
    eprintln!("no nodestore binary found; building one for the throwaway rig");
    let status = std::process::Command::new("cargo")
        .args(["build", "-p", "nodestore"])
        .current_dir(repo_root)
        .status()
        .ok()?;
    status.success().then(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("repository root")
            .join("target/debug/nodestore")
    })
}

async fn spawn_nodestore(nodestore: &Path, port: u16) -> (Child, tempfile::TempDir, Vec<PathBuf>) {
    let data_dir = tempfile::tempdir().expect("creating a scratch nodestore data dir");
    let listen = format!("127.0.0.1:{port}");
    let mut child = Command::new(nodestore)
        .env("NODESTORE_LISTEN", &listen)
        .env("NODESTORE_DATA_DIR", data_dir.path())
        .env("RUST_LOG", "warn")
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the real nodestore binary");

    let pki_dir = data_dir.path().join("pki/client");
    let files = vec![
        pki_dir.join("client.crt"),
        pki_dir.join("client.key"),
        pki_dir.join("ca.crt"),
    ];
    for _ in 0..100 {
        if let Some(status) = child.try_wait().expect("checking nodestore startup") {
            panic!("nodestore exited during startup with {status:?}");
        }
        if files.iter().all(|path| path.is_file())
            && tokio::net::TcpStream::connect(&listen).await.is_ok()
        {
            return (child, data_dir, files);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("nodestore did not create client TLS material within 10s");
}

async fn wait_ready(client: &Client, endpoint: &str) {
    for _ in 0..100 {
        if let Ok(response) = client.get(format!("{endpoint}/readyz")).send().await {
            if response.status().is_success() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("nodeapiserver did not become ready within 10s");
}

#[tokio::test]
async fn listener_serves_a_real_discovery_and_crud_round_trip() {
    let Some(nodestore) = find_nodestore_binary() else {
        eprintln!("SKIPPED: nodestore binary is not available for the throwaway rig");
        return;
    };
    let Some(nodeapiserver) = std::env::var_os("CARGO_BIN_EXE_nodeapiserver")
        .map(PathBuf::from)
        .or_else(|| find_binary("nodeapiserver"))
    else {
        eprintln!("SKIPPED: nodeapiserver binary is not available for the throwaway rig");
        return;
    };

    let nodestore_port = 23901 + (std::process::id() % 100) as u16;
    let listener_port = 24901 + (std::process::id() % 100) as u16;
    let (mut nodestore_child, _data_dir, nodestore_tls) =
        spawn_nodestore(&nodestore, nodestore_port).await;

    let tls_dir = tempfile::tempdir().expect("creating listener TLS directory");
    let certificate = generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generating listener certificate");
    let cert_file = tls_dir.path().join("server.crt");
    let key_file = tls_dir.path().join("server.key");
    std::fs::write(&cert_file, certificate.cert.pem()).expect("writing listener certificate");
    std::fs::write(&key_file, certificate.key_pair.serialize_pem()).expect("writing listener key");

    let endpoint = format!("https://127.0.0.1:{listener_port}");
    let mut nodeapiserver_child = Command::new(nodeapiserver)
        .env(
            "NODEAPISERVER_BIND_ADDR",
            format!("127.0.0.1:{listener_port}"),
        )
        .env("NODEAPISERVER_TLS_CERT_FILE", &cert_file)
        .env("NODEAPISERVER_TLS_KEY_FILE", &key_file)
        .env(
            "NODEAPISERVER_NODESTORE_ENDPOINT",
            format!("https://127.0.0.1:{nodestore_port}"),
        )
        .env("NODEAPISERVER_NODESTORE_CERT_FILE", &nodestore_tls[0])
        .env("NODEAPISERVER_NODESTORE_KEY_FILE", &nodestore_tls[1])
        .env("NODEAPISERVER_NODESTORE_CA_FILE", &nodestore_tls[2])
        .env("RUST_LOG", "warn")
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the real nodeapiserver binary");

    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("building the throwaway-rig HTTP client");
    wait_ready(&client, &endpoint).await;

    let version = client
        .get(format!("{endpoint}/version"))
        .send()
        .await
        .expect("requesting /version")
        .error_for_status()
        .expect("/version should succeed")
        .json::<serde_json::Value>()
        .await
        .expect("decoding /version");
    assert_eq!(version["major"], "1");
    assert!(version["gitVersion"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));

    let discovery = client
        .get(format!("{endpoint}/api/v1"))
        .send()
        .await
        .expect("requesting core discovery")
        .error_for_status()
        .expect("core discovery should succeed")
        .json::<serde_json::Value>()
        .await
        .expect("decoding core discovery");
    assert_eq!(discovery["kind"], "APIResourceList");
    assert!(discovery["resources"]
        .as_array()
        .is_some_and(|resources| resources
            .iter()
            .any(|resource| resource["name"] == "namespaces")));

    let dra_discovery = client
        .get(format!("{endpoint}/apis/resource.k8s.io/v1"))
        .send()
        .await
        .expect("requesting DRA discovery")
        .error_for_status()
        .expect("DRA discovery should succeed")
        .json::<serde_json::Value>()
        .await
        .expect("decoding DRA discovery");
    assert!(dra_discovery["resources"]
        .as_array()
        .is_some_and(|resources| {
            resources.iter().any(|resource| {
                resource["name"] == "resourceclaimtemplates" && resource["namespaced"] == true
            }) && resources.iter().any(|resource| {
                resource["name"] == "resourceslices" && resource["namespaced"] == false
            })
        }));

    let name = format!("listener-rig-{}", std::process::id());
    let created = client
        .post(format!("{endpoint}/api/v1/namespaces"))
        .header("content-type", "application/json")
        .json(&json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {"name": name},
        }))
        .send()
        .await
        .expect("creating a Namespace through the listener");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);

    let fetched = client
        .get(format!("{endpoint}/api/v1/namespaces/{name}"))
        .send()
        .await
        .expect("getting the Namespace through the listener")
        .error_for_status()
        .expect("Namespace GET should succeed")
        .json::<serde_json::Value>()
        .await
        .expect("decoding Namespace GET");
    assert_eq!(fetched["metadata"]["name"], name);
    // Real upstream's `rest.BeforeCreate` stamps every created object's
    // `metadata.generation` to 1 unconditionally — without it, nothing
    // that keys off generation (e.g. `PodCondition.observedGeneration`)
    // ever has a real value to observe (docs/APISERVER_E2E_FIX.md,
    // "Pod Ready condition missing observedGeneration").
    assert_eq!(fetched["metadata"]["generation"], 1);

    // A plain Service create with no `spec.clusterIP` of its own must come
    // back with a real, allocated address — nothing upstream calls
    // "defaulting" can do this (it's stateless), so it's the create path's
    // own job (docs/APISERVER_E2E_FIX.md, "ClusterIP Service never gets a
    // routable IP").
    let service_name = format!("{name}-svc");
    let created_service = client
        .post(format!("{endpoint}/api/v1/namespaces/{name}/services"))
        .header("content-type", "application/json")
        .json(&json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"name": service_name},
            "spec": {"ports": [{"port": 80, "targetPort": 8080}]},
        }))
        .send()
        .await
        .expect("creating a Service through the listener")
        .error_for_status()
        .expect("Service create should succeed")
        .json::<serde_json::Value>()
        .await
        .expect("decoding the created Service");
    let cluster_ip = created_service["spec"]["clusterIP"]
        .as_str()
        .expect("created Service should have an allocated spec.clusterIP")
        .to_string();
    assert_ne!(cluster_ip, "None");
    assert!(cluster_ip.starts_with("10.43."));
    assert_eq!(
        created_service["spec"]["clusterIPs"],
        json!([cluster_ip.clone()])
    );

    // A second Service must not collide with the first one's address.
    let second_service_name = format!("{name}-svc-2");
    let second_service = client
        .post(format!("{endpoint}/api/v1/namespaces/{name}/services"))
        .header("content-type", "application/json")
        .json(&json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"name": second_service_name},
            "spec": {"ports": [{"port": 81, "targetPort": 8081}]},
        }))
        .send()
        .await
        .expect("creating a second Service through the listener")
        .error_for_status()
        .expect("second Service create should succeed")
        .json::<serde_json::Value>()
        .await
        .expect("decoding the second created Service");
    let second_cluster_ip = second_service["spec"]["clusterIP"]
        .as_str()
        .expect("second Service should also have an allocated clusterIP");
    assert_ne!(second_cluster_ip, cluster_ip);

    // A headless Service must keep `clusterIP: "None"` rather than have one
    // allocated for it.
    let headless_service_name = format!("{name}-svc-headless");
    let headless_service = client
        .post(format!("{endpoint}/api/v1/namespaces/{name}/services"))
        .header("content-type", "application/json")
        .json(&json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"name": headless_service_name},
            "spec": {"clusterIP": "None", "ports": [{"port": 82, "targetPort": 8082}]},
        }))
        .send()
        .await
        .expect("creating a headless Service through the listener")
        .error_for_status()
        .expect("headless Service create should succeed")
        .json::<serde_json::Value>()
        .await
        .expect("decoding the headless created Service");
    assert_eq!(headless_service["spec"]["clusterIP"], "None");

    let template_name = format!("{name}-template");
    let template_path =
        format!("{endpoint}/apis/resource.k8s.io/v1/namespaces/{name}/resourceclaimtemplates");
    let template = client
        .post(&template_path)
        .header("content-type", "application/json")
        .json(&json!({
            "apiVersion": "resource.k8s.io/v1",
            "kind": "ResourceClaimTemplate",
            "metadata": {"name": template_name},
            "spec": {"spec": {"devices": {"requests": []}}}
        }))
        .send()
        .await
        .expect("creating a ResourceClaimTemplate through the listener");
    assert_eq!(template.status(), reqwest::StatusCode::CREATED);

    let template_fetched = client
        .get(format!("{template_path}/{template_name}"))
        .send()
        .await
        .expect("getting the ResourceClaimTemplate through the listener")
        .error_for_status()
        .expect("ResourceClaimTemplate GET should succeed")
        .json::<serde_json::Value>()
        .await
        .expect("decoding ResourceClaimTemplate GET");
    assert_eq!(template_fetched["metadata"]["name"], template_name);

    let template_deleted = client
        .delete(format!("{template_path}/{template_name}"))
        .send()
        .await
        .expect("deleting the ResourceClaimTemplate through the listener");
    assert_eq!(template_deleted.status(), reqwest::StatusCode::OK);

    // Regression for docs/APISERVER_E2E_FIX.md's "TLS bootstrap client
    // certificate kubeconfig" finding: nodecontroller's
    // certificatesigningrequest-signing-controller issues a certificate by
    // sending an `application/merge-patch+json` PATCH to a CSR's `/status`
    // subresource -- the admission check there was unconditionally passing
    // `None` as the candidate object for a PATCH (only the `verb ==
    // "update"` full-object-replace path had a real candidate), so every
    // real signing PATCH was rejected 403 regardless of RBAC, and
    // nodelet's bootstrap flow (crates/nodelet/src/bootstrap.rs) timed out
    // waiting for a certificate that could never be issued.
    let csr_name = format!("{name}-csr");
    let csr_path = format!("{endpoint}/apis/certificates.k8s.io/v1/certificatesigningrequests");
    let csr_request_pem = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(
            b"-----BEGIN CERTIFICATE REQUEST-----\nMA0=\n-----END CERTIFICATE REQUEST-----\n",
        )
    };
    let created_csr = client
        .post(&csr_path)
        .header("content-type", "application/json")
        .json(&json!({
            "apiVersion": "certificates.k8s.io/v1",
            "kind": "CertificateSigningRequest",
            "metadata": {"name": &csr_name},
            "spec": {
                "request": csr_request_pem,
                "signerName": "kubernetes.io/kube-apiserver-client-kubelet",
                "usages": ["digital signature", "key encipherment", "client auth"],
            }
        }))
        .send()
        .await
        .expect("creating a CertificateSigningRequest through the listener");
    assert_eq!(created_csr.status(), reqwest::StatusCode::CREATED);

    let status_patch = client
        .patch(format!("{csr_path}/{csr_name}/status"))
        .header("content-type", "application/merge-patch+json")
        .body(r#"{"status":{"certificate":"LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0tCk1BMD0KLS0tLS1FTkQgQ0VSVElGSUNBVEUtLS0tLQo="}}"#)
        .send()
        .await
        .expect("PATCHing a CSR /status subresource through the listener");
    assert_eq!(
        status_patch.status(),
        reqwest::StatusCode::OK,
        "signing a CSR via a merge-patch PATCH to /status must not be rejected: {}",
        status_patch.text().await.unwrap_or_default()
    );

    let deleted = client
        .delete(format!("{endpoint}/api/v1/namespaces/{name}"))
        .send()
        .await
        .expect("deleting the Namespace through the listener");
    assert_eq!(deleted.status(), reqwest::StatusCode::OK);

    let _ = nodeapiserver_child.kill().await;
    let _ = nodeapiserver_child.wait().await;
    let _ = nodestore_child.kill().await;
    let _ = nodestore_child.wait().await;
}
