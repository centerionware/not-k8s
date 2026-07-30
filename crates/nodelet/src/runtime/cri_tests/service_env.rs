//! Service discovery environment variables and Pod fieldRef lookup.
use super::*;
use k8s_openapi::api::core::v1::{ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

fn service(namespace: &str, name: &str, cluster_ip: &str, port: i32, port_name: Option<&str>) -> Service {
    Service {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            cluster_ip: Some(cluster_ip.to_string()),
            ports: Some(vec![ServicePort {
                name: port_name.map(str::to_string),
                port,
                protocol: Some("TCP".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn injects_default_namespace_kubernetes_service_into_system_namespace() {
    let services = vec![service("default", "kubernetes", "10.43.0.1", 443, Some("https"))];
    let env = service_env_vars(&services, "kube-system");
    assert_eq!(env.get("KUBERNETES_SERVICE_HOST"), Some(&b"10.43.0.1".to_vec()));
    assert_eq!(env.get("KUBERNETES_SERVICE_PORT"), Some(&b"443".to_vec()));
    assert_eq!(env.get("KUBERNETES_SERVICE_PORT_HTTPS"), Some(&b"443".to_vec()));
    assert_eq!(env.get("KUBERNETES_PORT_443_TCP"), Some(&b"tcp://10.43.0.1:443".to_vec()));
}

#[test]
fn injects_services_from_the_pods_own_namespace() {
    let services = vec![service("kube-system", "kube-dns", "10.43.0.10", 53, None)];
    let env = service_env_vars(&services, "kube-system");
    assert_eq!(env.get("KUBE_DNS_SERVICE_HOST"), Some(&b"10.43.0.10".to_vec()));
    assert_eq!(env.get("KUBE_DNS_SERVICE_PORT"), Some(&b"53".to_vec()));
}

#[test]
fn skips_external_name_and_headless_services() {
    let mut external = service("default", "external", "10.43.0.20", 80, None);
    external.spec.as_mut().unwrap().type_ = Some("ExternalName".to_string());
    let headless = service("default", "headless", "None", 80, None);
    let env = service_env_vars(&[external, headless], "default");
    assert!(!env.keys().any(|key| key.starts_with("EXTERNAL_")));
    assert!(!env.keys().any(|key| key.starts_with("HEADLESS_")));
}

#[test]
fn resolves_common_downward_api_field_refs() {
    let pod = Pod {
        metadata: ObjectMeta {
            name: Some("coredns-abc".to_string()),
            namespace: Some("kube-system".to_string()),
            uid: Some("pod-uid".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(pod_field_value(&pod, "metadata.name"), Some("coredns-abc".to_string()));
    assert_eq!(pod_field_value(&pod, "metadata.namespace"), Some("kube-system".to_string()));
    assert_eq!(pod_field_value(&pod, "metadata.uid"), Some("pod-uid".to_string()));
}
