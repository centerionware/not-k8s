use super::*;

fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

mod identity {
    use super::*;

    #[test]
    fn matches_real_kubelets_node_identity_convention() {
        let (cn, o) = node_identity_dn("edge-1");
        assert_eq!(cn, "system:node:edge-1");
        assert_eq!(o, "system:nodes");
    }
}

mod csr_generation {
    use super::*;

    #[test]
    fn produces_a_pem_csr_and_a_pem_private_key() {
        ensure_crypto_provider();
        let (csr_pem, key_pem) = generate_csr("edge-1").expect("csr generation should succeed");
        assert!(csr_pem.contains("BEGIN CERTIFICATE REQUEST"));
        assert!(key_pem.contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn build_csr_object_carries_the_signer_name_and_request_bytes() {
        let obj = build_csr_object("edge-1", "-----BEGIN CERTIFICATE REQUEST-----\nabc\n-----END CERTIFICATE REQUEST-----\n");
        assert_eq!(obj.spec.signer_name, "kubernetes.io/kube-apiserver-client-kubelet");
        assert_eq!(obj.spec.request.0, b"-----BEGIN CERTIFICATE REQUEST-----\nabc\n-----END CERTIFICATE REQUEST-----\n");
        assert_eq!(obj.metadata.generate_name.as_deref(), Some("nodelet-edge-1-"));
        assert_eq!(obj.spec.usages.as_deref(), Some(&["digital signature".to_string(), "key encipherment".to_string(), "client auth".to_string()][..]));
    }
}

mod outcome {
    use super::*;

    fn condition(ty: &str, status: &str) -> CertificateSigningRequestCondition {
        CertificateSigningRequestCondition {
            type_: ty.to_string(),
            status: status.to_string(),
            message: Some(format!("{ty} message")),
            ..Default::default()
        }
    }

    #[test]
    fn certificate_present_means_issued_regardless_of_conditions() {
        let cert = ByteString(b"cert-pem".to_vec());
        assert_eq!(csr_outcome(&[condition("Denied", "True")], Some(&cert)), CsrOutcome::Issued("cert-pem".to_string()));
    }

    #[test]
    fn a_true_denied_condition_with_no_certificate_is_denied() {
        assert_eq!(csr_outcome(&[condition("Denied", "True")], None), CsrOutcome::Denied("Denied message".to_string()));
    }

    #[test]
    fn a_true_failed_condition_with_no_certificate_is_denied() {
        assert_eq!(csr_outcome(&[condition("Failed", "True")], None), CsrOutcome::Denied("Failed message".to_string()));
    }

    #[test]
    fn a_false_denied_condition_is_still_pending() {
        assert_eq!(csr_outcome(&[condition("Denied", "False")], None), CsrOutcome::Pending);
    }

    #[test]
    fn an_approved_condition_alone_is_still_pending_until_a_certificate_shows_up() {
        assert_eq!(csr_outcome(&[condition("Approved", "True")], None), CsrOutcome::Pending);
    }

    #[test]
    fn no_conditions_and_no_certificate_is_pending() {
        assert_eq!(csr_outcome(&[], None), CsrOutcome::Pending);
    }
}

mod output_kubeconfig {
    use super::*;
    use kube::config::{Cluster, NamedCluster};

    fn bootstrap_kubeconfig() -> Kubeconfig {
        Kubeconfig {
            clusters: vec![NamedCluster {
                name: "bootstrap-cluster".to_string(),
                cluster: Some(Cluster {
                    server: Some("https://10.0.0.1:6443".to_string()),
                    certificate_authority_data: Some("ZmFrZS1jYQ==".to_string()),
                    ..Default::default()
                }),
                other: Default::default(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn carries_over_the_cluster_server_and_ca() {
        let out = build_output_kubeconfig(&bootstrap_kubeconfig(), "cert-pem", "key-pem").unwrap();
        let cluster = out.clusters[0].cluster.as_ref().unwrap();
        assert_eq!(cluster.server.as_deref(), Some("https://10.0.0.1:6443"));
        assert_eq!(cluster.certificate_authority_data.as_deref(), Some("ZmFrZS1jYQ=="));
    }

    #[test]
    fn base64_encodes_the_issued_cert_and_key() {
        use base64::Engine;
        let out = build_output_kubeconfig(&bootstrap_kubeconfig(), "cert-pem", "key-pem").unwrap();
        let auth_info = out.auth_infos[0].auth_info.as_ref().unwrap();
        let b64 = base64::engine::general_purpose::STANDARD;
        assert_eq!(auth_info.client_certificate_data.as_deref(), Some(b64.encode("cert-pem").as_str()));
    }

    #[test]
    fn sets_a_usable_current_context() {
        let out = build_output_kubeconfig(&bootstrap_kubeconfig(), "cert-pem", "key-pem").unwrap();
        assert_eq!(out.current_context.as_deref(), Some("default"));
        assert_eq!(out.contexts[0].context.as_ref().unwrap().cluster, "default");
        assert_eq!(out.contexts[0].context.as_ref().unwrap().user.as_deref(), Some("nodelet"));
    }

    #[test]
    fn errors_if_the_bootstrap_kubeconfig_has_no_cluster() {
        let empty = Kubeconfig::default();
        assert!(build_output_kubeconfig(&empty, "cert-pem", "key-pem").is_err());
    }
}
