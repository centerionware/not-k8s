use super::*;

/// Real kubelet's `resourceFieldRef` output format (round 44; found in
/// round 35's re-audit): the raw value (millicores for CPU, bytes for
/// memory) is divided by the divisor (also in the same raw unit — a CPU
/// divisor is itself converted to millicores, a memory divisor to bytes),
/// then rounded **up** to a whole number of divisor-units and printed as a
/// plain integer — this is why an unset (default `"1"`) CPU divisor
/// famously reports whole cores, rounded up, rather than millicores. The
/// same formula handles both resource kinds identically; only the caller's
/// choice of raw unit and default divisor differs.
pub(crate) fn format_resource_field_value(raw: i64, divisor: i64) -> String {
    let divisor = divisor.max(1);
    ((raw + divisor - 1) / divisor).to_string()
}


/// Resolve one `resourceFieldRef` (env var or downwardAPI volume form) to
/// its plain-number string value. `limits.*` falls back to the node's own
/// capacity when the container has no such limit set — real kubelet's own
/// documented Downward API behavior (an unset limit is treated as "the
/// whole node," not zero/unbounded). `requests.*` falls back to the
/// container's own limit first (matching the general request-defaults-to-
/// limit rule), then to the node's capacity as the final fallback.
/// `ephemeral-storage` isn't tracked/enforced by nodelet at all (a
/// separate, pre-existing gap — see `docs/GAP_CLOSURE.md`), so it always
/// resolves to `"0"` rather than bailing.
pub(crate) fn resolve_resource_field_ref(
    reference: &k8s_openapi::api::core::v1::ResourceFieldSelector,
    resources: Option<&ResourceRequirements>,
    node_cpu_millicores: i64,
    node_memory_bytes: i64,
) -> Result<String> {
    let requests = resources.and_then(|r| r.requests.as_ref());
    let limits = resources.and_then(|r| r.limits.as_ref());
    let divisor_cpu = reference.divisor.as_ref().and_then(parse_cpu_millicores).filter(|d| *d > 0).unwrap_or(1000);
    let divisor_mem = reference.divisor.as_ref().and_then(parse_memory_bytes).filter(|d| *d > 0).unwrap_or(1);

    match reference.resource.as_str() {
        "limits.cpu" => {
            let m = limits.and_then(|r| r.get("cpu")).and_then(parse_cpu_millicores).unwrap_or(node_cpu_millicores);
            Ok(format_resource_field_value(m, divisor_cpu))
        }
        "requests.cpu" => {
            let m = requests
                .and_then(|r| r.get("cpu"))
                .and_then(parse_cpu_millicores)
                .or_else(|| limits.and_then(|r| r.get("cpu")).and_then(parse_cpu_millicores))
                .unwrap_or(node_cpu_millicores);
            Ok(format_resource_field_value(m, divisor_cpu))
        }
        "limits.memory" => {
            let b = limits.and_then(|r| r.get("memory")).and_then(parse_memory_bytes).unwrap_or(node_memory_bytes);
            Ok(format_resource_field_value(b, divisor_mem))
        }
        "requests.memory" => {
            let b = requests
                .and_then(|r| r.get("memory"))
                .and_then(parse_memory_bytes)
                .or_else(|| limits.and_then(|r| r.get("memory")).and_then(parse_memory_bytes))
                .unwrap_or(node_memory_bytes);
            Ok(format_resource_field_value(b, divisor_mem))
        }
        "limits.ephemeral-storage" | "requests.ephemeral-storage" => Ok("0".to_string()),
        other => bail!("resourceFieldRef: unsupported resource {other:?}"),
    }
}


/// Build the CRI `DnsConfig` for a pod, honoring `dnsPolicy` +
/// custom `dnsConfig`. `dnsPolicy: Default` means "inherit the node's own
/// resolv.conf" — returning `None` leaves containerd's own default in place,
/// which is exactly that. `ClusterFirst` (the pod-spec default) only takes
/// effect if the node was actually configured with cluster DNS servers
/// (`NODELET_CLUSTER_DNS`); an edge device with no cluster DNS server falls
/// back to the host's resolv.conf rather than pointing pods at nothing.
pub(crate) fn dns_config_for(pod: &Pod, cluster_dns: &[String], cluster_domain: &str) -> Option<DnsConfig> {
    let policy = pod
        .spec
        .as_ref()
        .and_then(|s| s.dns_policy.clone())
        .unwrap_or_else(|| "ClusterFirst".to_string());

    let mut servers = Vec::new();
    let mut searches = Vec::new();
    let mut options = Vec::new();

    if matches!(policy.as_str(), "ClusterFirst" | "ClusterFirstWithHostNet") && !cluster_dns.is_empty() {
        servers = cluster_dns.to_vec();
        let ns = pod.metadata.namespace.as_deref().unwrap_or("default");
        searches = vec![
            format!("{ns}.svc.{cluster_domain}"),
            format!("svc.{cluster_domain}"),
            cluster_domain.to_string(),
        ];
        options = vec!["ndots:5".to_string()];
    } else if policy == "Default" {
        return None; // explicit "use the host's own resolv.conf" — nothing to set
    }

    if let Some(dns_config) = pod.spec.as_ref().and_then(|s| s.dns_config.as_ref()) {
        servers.extend(dns_config.nameservers.clone().unwrap_or_default());
        searches.extend(dns_config.searches.clone().unwrap_or_default());
        options.extend(dns_config.options.clone().unwrap_or_default().into_iter().filter_map(|o| {
            let name = o.name?;
            Some(match o.value {
                Some(v) => format!("{name}:{v}"),
                None => name,
            })
        }));
    }

    if servers.is_empty() && searches.is_empty() && options.is_empty() {
        None
    } else {
        Some(DnsConfig { servers, searches, options })
    }
}


/// Real kubelet's `GeneratePodHostNameAndDomain` + `ShouldSetHostnameAsFQDN`
/// logic (round 38; found in round 35's re-audit): `spec.hostname` overrides
/// the sandbox hostname (defaults to the pod name); `spec.subdomain`
/// combines with the namespace/cluster domain to form the pod's headless-
/// Service search domain; `setHostnameAsFQDN` (only meaningful when
/// `subdomain` is also set) makes the *hostname itself* the full FQDN
/// instead of just the short name. Linux's `sethostname(2)` rejects
/// anything over `HOST_NAME_MAX` (64 bytes) — real kubelet fails the pod
/// rather than silently truncating, so this does too (via `Err`, which
/// `ensure_pod()`'s existing retry-and-report-failure path already handles,
/// no new failure mechanism needed).
pub(crate) fn resolve_pod_hostname(
    hostname: Option<&str>,
    subdomain: Option<&str>,
    set_hostname_as_fqdn: bool,
    pod_name: &str,
    namespace: &str,
    cluster_domain: &str,
) -> Result<String> {
    let short = hostname.unwrap_or(pod_name);
    let Some(subdomain) = subdomain.filter(|s| !s.is_empty()) else {
        return Ok(short.to_string());
    };
    if !set_hostname_as_fqdn {
        return Ok(short.to_string());
    }
    let fqdn = format!("{short}.{subdomain}.{namespace}.svc.{cluster_domain}");
    if fqdn.len() > 64 {
        bail!("setHostnameAsFQDN: FQDN '{fqdn}' is {} bytes, longer than the 64-byte Linux hostname limit", fqdn.len());
    }
    Ok(fqdn)
}


/// `spec.securityContext.sysctls` -> CRI's `LinuxPodSandboxConfig.sysctls`
/// map (round 41; found in round 39's re-audit). A later duplicate `name`
/// in the list simply overwrites an earlier one in the resulting map —
/// the apiserver's own validation already rejects duplicate sysctl names
/// within a single Pod, so this never has to arbitrate a real conflict.
pub(crate) fn pod_sysctls(pod_sc: Option<&PodSecurityContext>) -> HashMap<String, String> {
    pod_sc
        .and_then(|sc| sc.sysctls.as_ref())
        .map(|list| list.iter().map(|s| (s.name.clone(), s.value.clone())).collect())
        .unwrap_or_default()
}


/// Return the Kubernetes VolumeSource variant for diagnostics. A volume's
/// name alone is not enough to explain why it was skipped — in particular,
/// kube-controller-manager injects a volume named `kube-api-access-*` whose
/// source is `projected`, not `hostPath` or `emptyDir`.
pub(crate) fn volume_source_type(v: &Volume) -> &'static str {
    if v.config_map.is_some() {
        "configMap"
    } else if v.secret.is_some() {
        "secret"
    } else if v.empty_dir.is_some() {
        "emptyDir"
    } else if v.projected.is_some() {
        "projected"
    } else if v.host_path.is_some() {
        "hostPath"
    } else if v.downward_api.is_some() {
        "downwardAPI"
    } else if v.persistent_volume_claim.is_some() {
        "persistentVolumeClaim"
    } else if v.csi.is_some() {
        "csi"
    } else if v.ephemeral.is_some() {
        "ephemeral"
    } else if v.nfs.is_some() {
        "nfs"
    } else if v.aws_elastic_block_store.is_some() {
        "awsElasticBlockStore"
    } else if v.azure_disk.is_some() {
        "azureDisk"
    } else if v.azure_file.is_some() {
        "azureFile"
    } else if v.cephfs.is_some() {
        "cephfs"
    } else if v.cinder.is_some() {
        "cinder"
    } else if v.fc.is_some() {
        "fc"
    } else if v.flex_volume.is_some() {
        "flexVolume"
    } else if v.flocker.is_some() {
        "flocker"
    } else if v.gce_persistent_disk.is_some() {
        "gcePersistentDisk"
    } else if v.git_repo.is_some() {
        "gitRepo"
    } else if v.glusterfs.is_some() {
        "glusterfs"
    } else if v.iscsi.is_some() {
        "iscsi"
    } else if v.photon_persistent_disk.is_some() {
        "photonPersistentDisk"
    } else if v.portworx_volume.is_some() {
        "portworx"
    } else if v.quobyte.is_some() {
        "quobyte"
    } else if v.rbd.is_some() {
        "rbd"
    } else if v.scale_io.is_some() {
        "scaleIO"
    } else if v.storageos.is_some() {
        "storageos"
    } else if v.vsphere_volume.is_some() {
        "vsphereVolume"
    } else {
        "unknown"
    }
}


/// Convert a Service or port name to the form used by Kubernetes' legacy
/// service-environment mechanism (`my-api` -> `MY_API`).
pub(crate) fn env_name_component(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect()
}


/// Add the Service discovery variables kubelet normally injects into every
/// container. In particular, client-go's in-cluster configuration requires
/// `KUBERNETES_SERVICE_HOST` and `KUBERNETES_SERVICE_PORT`.
///
/// Services in the Pod's namespace are included, as are Services in the
/// default namespace. The latter makes the cluster's `kubernetes` Service
/// discoverable from system namespaces such as `kube-system`.
pub(crate) fn service_env_vars(services: &[Service], pod_namespace: &str) -> BTreeMap<String, Vec<u8>> {
    let mut values = BTreeMap::new();

    // Add default-namespace Services first so a same-named Service in the
    // Pod's own namespace takes precedence below.
    for service in services.iter().filter(|s| {
        s.metadata.namespace.as_deref().unwrap_or("default") == "default" && pod_namespace != "default"
    }) {
        add_service_env_vars(&mut values, service);
    }
    for service in services.iter().filter(|s| {
        s.metadata.namespace.as_deref().unwrap_or("default") == pod_namespace
    }) {
        add_service_env_vars(&mut values, service);
    }

    values
}


pub(crate) fn add_service_env_vars(values: &mut BTreeMap<String, Vec<u8>>, service: &Service) {
    let Some(name) = service.metadata.name.as_deref().filter(|n| !n.is_empty()) else {
        return;
    };
    let Some(spec) = service.spec.as_ref() else { return };
    if spec.type_.as_deref() == Some("ExternalName") {
        return;
    }
    let Some(cluster_ip) = spec.cluster_ip.as_deref().filter(|ip| !ip.is_empty() && *ip != "None") else {
        return;
    };

    let prefix = env_name_component(name);
    put_env(values, format!("{prefix}_SERVICE_HOST"), cluster_ip.as_bytes().to_vec());

    for (index, port) in spec.ports.as_deref().unwrap_or(&[]).iter().enumerate() {
        let port_value = port.port.to_string();
        if index == 0 {
            put_env(values, format!("{prefix}_SERVICE_PORT"), port_value.as_bytes().to_vec());
        }
        if let Some(port_name) = port.name.as_deref().filter(|n| !n.is_empty()) {
            put_env(
                values,
                format!("{prefix}_SERVICE_PORT_{}", env_name_component(port_name)),
                port_value.as_bytes().to_vec(),
            );
        }

        // The legacy *_PORT_* variables are still emitted by kubelet and are
        // used by some images even when *_SERVICE_* is not.
        let protocol = port.protocol.as_deref().unwrap_or("TCP");
        let protocol_lower = protocol.to_ascii_lowercase();
        let protocol_upper = protocol.to_ascii_uppercase();
        let uri_host = if cluster_ip.contains(':') {
            format!("[{cluster_ip}]")
        } else {
            cluster_ip.to_string()
        };
        let uri = format!("{protocol_lower}://{uri_host}:{port_value}");
        put_env(values, format!("{prefix}_PORT"), uri.as_bytes().to_vec());
        let port_prefix = format!("{prefix}_PORT_{}_{}", port.port, protocol_upper);
        put_env(values, port_prefix.clone(), uri.as_bytes().to_vec());
        put_env(values, format!("{port_prefix}_PROTO"), protocol_lower.as_bytes().to_vec());
        put_env(values, format!("{port_prefix}_PORT"), port_value.as_bytes().to_vec());
        put_env(values, format!("{port_prefix}_ADDR"), cluster_ip.as_bytes().to_vec());
    }
}


pub(crate) fn put_env(values: &mut BTreeMap<String, Vec<u8>>, key: String, value: Vec<u8>) {
    values.insert(key, value);
}


/// Resolve a downward-API fieldRef from the Pod object supplied by the API
/// server.
pub(crate) fn pod_field_value(pod: &Pod, field_path: &str) -> Option<String> {
    match field_path {
        "metadata.name" => pod.metadata.name.clone(),
        "metadata.namespace" => pod.metadata.namespace.clone(),
        "metadata.uid" => pod.metadata.uid.clone(),
        "spec.nodeName" => pod.spec.as_ref()?.node_name.clone(),
        "spec.serviceAccountName" => Some(
            pod.spec
                .as_ref()?
                .service_account_name
                .clone()
                .unwrap_or_else(|| "default".to_string()),
        ),
        "status.hostIP" => pod.status.as_ref()?.host_ip.clone(),
        "status.podIP" => pod.status.as_ref()?.pod_ip.clone(),
        "status.podIPs" => pod
            .status
            .as_ref()?
            .pod_ips
            .as_ref()?
            .first()
            .map(|ip| ip.ip.clone()),
        _ => {
            if let Some(key) = field_path
                .strip_prefix("metadata.labels[")
                .and_then(|s| s.strip_suffix(']'))
            {
                return pod
                    .metadata
                    .labels
                    .as_ref()?
                    .get(key.trim_matches(|c| c == '\'' || c == '"'))
                    .cloned();
            }
            if let Some(key) = field_path
                .strip_prefix("metadata.annotations[")
                .and_then(|s| s.strip_suffix(']'))
            {
                return pod
                    .metadata
                    .annotations
                    .as_ref()?
                    .get(key.trim_matches(|c| c == '\'' || c == '"'))
                    .cloned();
            }
            None
        }
    }
}


impl CriRuntime {
    pub(crate) async fn resolve_service_env(&self, namespace: &str) -> Result<BTreeMap<String, Vec<u8>>> {
        let api: Api<Service> = Api::all(self.client.clone());
        let services = api.list(&ListParams::default()).await.context("listing Services")?;
        Ok(service_env_vars(&services.items, namespace))
    }

    pub(crate) async fn resolve_env_from(&self, source: &EnvFromSource, namespace: &str) -> Result<BTreeMap<String, Vec<u8>>> {
        let mut values = BTreeMap::new();
        let prefix = source.prefix.clone().unwrap_or_default();

        if let Some(reference) = &source.config_map_ref {
            let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), namespace);
            let config_map = match api.get(&reference.name).await {
                Ok(obj) => obj,
                Err(_) if reference.optional.unwrap_or(false) => return Ok(values),
                Err(e) => return Err(e).with_context(|| format!("fetching ConfigMap {} for envFrom", reference.name)),
            };
            for (key, value) in config_map.data.unwrap_or_default() {
                values.insert(format!("{prefix}{key}"), value.into_bytes());
            }
        }

        if let Some(reference) = &source.secret_ref {
            let api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
            let secret = match api.get(&reference.name).await {
                Ok(obj) => obj,
                Err(_) if reference.optional.unwrap_or(false) => return Ok(values),
                Err(e) => return Err(e).with_context(|| format!("fetching Secret {} for envFrom", reference.name)),
            };
            for (key, value) in secret.data.unwrap_or_default() {
                values.insert(format!("{prefix}{key}"), value.0);
            }
        }

        Ok(values)
    }

    pub(crate) async fn resolve_env_var_source(
        &self,
        source: &EnvVarSource,
        pod: &Pod,
        id: &PodId,
        container: &k8s_openapi::api::core::v1::Container,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(reference) = &source.config_map_key_ref {
            let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &id.namespace);
            let config_map = match api.get(&reference.name).await {
                Ok(obj) => obj,
                Err(_) if reference.optional.unwrap_or(false) => return Ok(None),
                Err(e) => return Err(e).with_context(|| format!("fetching ConfigMap {} for env", reference.name)),
            };
            return match config_map.data.unwrap_or_default().remove(&reference.key) {
                Some(value) => Ok(Some(value.into_bytes())),
                None if reference.optional.unwrap_or(false) => Ok(None),
                None => anyhow::bail!("ConfigMap {} has no key {}", reference.name, reference.key),
            };
        }

        if let Some(reference) = &source.secret_key_ref {
            let api: Api<Secret> = Api::namespaced(self.client.clone(), &id.namespace);
            let secret = match api.get(&reference.name).await {
                Ok(obj) => obj,
                Err(_) if reference.optional.unwrap_or(false) => return Ok(None),
                Err(e) => return Err(e).with_context(|| format!("fetching Secret {} for env", reference.name)),
            };
            return match secret.data.unwrap_or_default().remove(&reference.key) {
                Some(value) => Ok(Some(value.0)),
                None if reference.optional.unwrap_or(false) => Ok(None),
                None => anyhow::bail!("Secret {} has no key {}", reference.name, reference.key),
            };
        }

        if let Some(reference) = &source.field_ref {
            let value = pod_field_value(pod, &reference.field_path)
                .with_context(|| format!("unsupported or unavailable fieldRef {}", reference.field_path))?;
            return Ok(Some(value.into_bytes()));
        }

        if let Some(reference) = &source.resource_field_ref {
            let value = resolve_resource_field_ref(
                reference,
                container.resources.as_ref(),
                self.node_cpu_millicores,
                self.node_memory_bytes,
            )?;
            return Ok(Some(value.into_bytes()));
        }

        Ok(None)
    }

    pub(crate) async fn resolve_container_env(
        &self,
        pod: &Pod,
        id: &PodId,
        container: &k8s_openapi::api::core::v1::Container,
        service_env: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Vec<KeyValue>> {
        let mut values = service_env.clone();

        for source in container.env_from.as_deref().unwrap_or(&[]) {
            for (key, value) in self.resolve_env_from(source, &id.namespace).await? {
                put_env(&mut values, key, value);
            }
        }

        for env in container.env.as_deref().unwrap_or(&[]) {
            if let Some(source) = &env.value_from {
                if let Some(value) = self.resolve_env_var_source(source, pod, id, container).await? {
                    values.insert(env.name.clone(), value);
                }
            } else {
                values.insert(env.name.clone(), env.value.clone().unwrap_or_default().into_bytes());
            }
        }

        Ok(values.into_iter().map(|(key, value)| KeyValue { key, value }).collect())
    }

}
