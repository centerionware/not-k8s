use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::request::Distribution;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceManager {
    Systemd,
    OpenRc,
    SysVInit,
    Runit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum K3sDatastore {
    Kine,
    Etcd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterConfig {
    pub data_dir: PathBuf,
    pub kubeconfig: Option<PathBuf>,
    pub service_cidr: Option<String>,
    pub cluster_cidr: Option<String>,
    pub cluster_domain: Option<String>,
    pub cluster_dns: Option<String>,
    pub cni: Option<String>,
    pub flannel_backend: Option<String>,
    pub datastore: K3sDatastore,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installation {
    pub distribution: Distribution,
    pub service_manager: Option<ServiceManager>,
    pub service_name: String,
    pub service_file: Option<PathBuf>,
    pub binary: Option<PathBuf>,
    pub config_files: Vec<PathBuf>,
    pub cluster: Option<ClusterConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryReport {
    pub installation: Option<Installation>,
}

impl InventoryReport {
    pub fn from_installation(installation: Option<Installation>) -> Self {
        Self { installation }
    }
}

#[derive(Debug, Clone)]
pub struct HostLayout {
    root: PathBuf,
}

impl HostLayout {
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/"),
        }
    }

    #[cfg(test)]
    fn under(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }
}

pub fn inspect_host(layout: &HostLayout) -> Result<Option<Installation>> {
    if let Some(installation) = inspect_k3s(layout)? {
        return Ok(Some(installation));
    }
    if let Some(installation) = inspect_nodestore(layout)? {
        return Ok(Some(installation));
    }
    inspect_kubernetes(layout)
}

fn inspect_k3s(layout: &HostLayout) -> Result<Option<Installation>> {
    let systemd_unit = first_file(
        layout,
        &[
            "/etc/systemd/system/k3s.service",
            "/usr/lib/systemd/system/k3s.service",
            "/lib/systemd/system/k3s.service",
        ],
    );
    let init_script = first_file(layout, &["/etc/init.d/k3s"]);
    let openrc_script = init_script
        .as_ref()
        .filter(|path| {
            std::fs::read_to_string(append(&layout.root, path))
                .is_ok_and(|text| text.starts_with("#!/sbin/openrc-run"))
        })
        .cloned();
    let sysv_script = init_script.filter(|_| openrc_script.is_none());
    let runit_service = first_dir(layout, &["/etc/service/k3s", "/var/service/k3s"]);
    let service_file = systemd_unit
        .clone()
        .or_else(|| openrc_script.clone())
        .or_else(|| sysv_script.clone())
        .or_else(|| runit_service.clone());

    let config_files = k3s_config_files(layout);
    let systemd_args = systemd_unit
        .as_deref()
        .map(|path| read_systemd_exec_start(&append(&layout.root, path)))
        .transpose()
        .with_context(|| "reading detected k3s systemd unit")?;
    let mut config = K3sConfig::default();
    for path in &config_files {
        let actual_path = append(&layout.root, path);
        let bytes = std::fs::read(&actual_path)
            .with_context(|| format!("reading k3s config {}", path.display()))?;
        let value: Value = serde_yaml::from_slice(&bytes)
            .with_context(|| format!("parsing k3s config {}", path.display()))?;
        config.merge_yaml(&value);
    }

    let data_dir = config
        .data_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("/var/lib/rancher/k3s"));
    if let Some(args) = systemd_args.as_deref() {
        config.merge_args(args);
    }
    let data_dir = config.data_dir.clone().unwrap_or(data_dir);
    let default_db = append(&layout.root, &data_dir.join("server/db/state.db"));
    let etcd_dir = append(&layout.root, &data_dir.join("server/db/etcd"));

    let binary = locate_binary(layout, systemd_args.as_deref(), &data_dir);
    let has_installation = service_file.is_some()
        || binary.is_some()
        || !config_files.is_empty()
        || default_db.exists()
        || etcd_dir.is_dir();
    if !has_installation {
        return Ok(None);
    }

    let datastore = detect_datastore(&config, default_db.exists(), etcd_dir.is_dir())?;
    let cluster = ClusterConfig {
        data_dir: data_dir.clone(),
        kubeconfig: config.write_kubeconfig,
        service_cidr: config.service_cidr,
        cluster_cidr: config.cluster_cidr,
        cluster_domain: config.cluster_domain,
        cluster_dns: config.cluster_dns,
        cni: if config.disable_flannel {
            None
        } else {
            Some("flannel".to_string())
        },
        flannel_backend: config.flannel_backend,
        datastore,
    };
    Ok(Some(Installation {
        distribution: Distribution::K3s,
        service_manager: detect_service_manager(
            layout,
            systemd_unit.is_some(),
            openrc_script.is_some(),
            sysv_script.is_some(),
            runit_service.is_some(),
        ),
        service_name: "k3s".to_string(),
        service_file,
        binary,
        config_files,
        cluster: Some(cluster),
    }))
}

fn inspect_nodestore(layout: &HostLayout) -> Result<Option<Installation>> {
    let systemd_unit = first_file(
        layout,
        &[
            "/etc/systemd/system/nodestore.service",
            "/usr/lib/systemd/system/nodestore.service",
            "/lib/systemd/system/nodestore.service",
        ],
    );
    let data_dir = layout.path("/var/lib/nodestore");
    let binary = first_file(layout, &["/usr/local/bin/nodestore", "/usr/bin/nodestore"]);
    let service_file = systemd_unit.clone();
    if service_file.is_none() && binary.is_none() && !data_dir.is_dir() {
        return Ok(None);
    }
    Ok(Some(Installation {
        distribution: Distribution::Nodestore,
        service_manager: detect_service_manager(
            layout,
            systemd_unit.is_some(),
            false,
            false,
            false,
        ),
        service_name: "nodestore".to_string(),
        service_file,
        binary,
        config_files: Vec::new(),
        cluster: None,
    }))
}

fn inspect_kubernetes(layout: &HostLayout) -> Result<Option<Installation>> {
    // kubeadm and other upstream distributions expose kubelet and the
    // kube-apiserver static-pod manifest. We require both signals so a worker
    // node is not mistaken for a local control plane.
    let kubelet = first_file(
        layout,
        &[
            "/etc/systemd/system/kubelet.service",
            "/usr/lib/systemd/system/kubelet.service",
            "/lib/systemd/system/kubelet.service",
        ],
    );
    let apiserver_manifest = layout.path("/etc/kubernetes/manifests/kube-apiserver.yaml");
    if kubelet.is_none() || !apiserver_manifest.is_file() {
        return Ok(None);
    }
    let service_file = kubelet;
    let binary = first_file(layout, &["/usr/bin/kubeadm", "/usr/local/bin/kubeadm"]);
    Ok(Some(Installation {
        distribution: Distribution::Kubernetes,
        service_manager: detect_service_manager(
            layout,
            service_file.is_some(),
            false,
            false,
            false,
        ),
        service_name: "kubelet".to_string(),
        service_file,
        binary,
        config_files: vec![strip_root(&layout.root, apiserver_manifest)],
        cluster: None,
    }))
}

fn detect_datastore(
    config: &K3sConfig,
    sqlite_exists: bool,
    etcd_dir_exists: bool,
) -> Result<K3sDatastore> {
    if config.cluster_init
        || config
            .datastore_endpoint
            .as_deref()
            .is_some_and(is_etcd_endpoint)
        || etcd_dir_exists
    {
        return Ok(K3sDatastore::Etcd);
    }
    if config
        .datastore_endpoint
        .as_deref()
        .is_some_and(is_kine_endpoint)
        || sqlite_exists
        || config.datastore_endpoint.is_none()
    {
        return Ok(K3sDatastore::Kine);
    }
    bail!("could not identify the k3s datastore backend from its config and data directory")
}

fn is_etcd_endpoint(endpoint: &str) -> bool {
    endpoint.starts_with("etcd://")
        || endpoint
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
            == Some(2379)
}

fn is_kine_endpoint(endpoint: &str) -> bool {
    ["sqlite://", "mysql://", "postgres://", "postgresql://"]
        .iter()
        .any(|scheme| endpoint.starts_with(scheme))
}

#[derive(Debug, Default)]
struct K3sConfig {
    data_dir: Option<PathBuf>,
    write_kubeconfig: Option<PathBuf>,
    service_cidr: Option<String>,
    cluster_cidr: Option<String>,
    cluster_domain: Option<String>,
    cluster_dns: Option<String>,
    datastore_endpoint: Option<String>,
    cluster_init: bool,
    disable_flannel: bool,
    flannel_backend: Option<String>,
}

impl K3sConfig {
    fn merge_yaml(&mut self, value: &Value) {
        self.data_dir = yaml_string(value, "data-dir")
            .map(PathBuf::from)
            .or(self.data_dir.take());
        self.write_kubeconfig = yaml_string(value, "write-kubeconfig")
            .map(PathBuf::from)
            .or(self.write_kubeconfig.take());
        self.service_cidr = yaml_string(value, "service-cidr").or(self.service_cidr.take());
        self.cluster_cidr = yaml_string(value, "cluster-cidr").or(self.cluster_cidr.take());
        self.cluster_domain = yaml_string(value, "cluster-domain").or(self.cluster_domain.take());
        self.cluster_dns = yaml_string(value, "cluster-dns").or(self.cluster_dns.take());
        self.datastore_endpoint =
            yaml_string(value, "datastore-endpoint").or(self.datastore_endpoint.take());
        self.cluster_init |= yaml_bool(value, "cluster-init");
        self.flannel_backend =
            yaml_string(value, "flannel-backend").or(self.flannel_backend.take());
        self.disable_flannel |= yaml_string(value, "flannel-backend").as_deref() == Some("none")
            || yaml_string_list(value, "disable")
                .iter()
                .any(|entry| entry == "flannel");
    }

    fn merge_args(&mut self, args: &str) {
        let tokens: Vec<&str> = args.split_whitespace().collect();
        for (index, token) in tokens.iter().enumerate() {
            if *token == "--cluster-init" {
                self.cluster_init = true;
                continue;
            }
            let value = token
                .split_once('=')
                .map(|(_, value)| value)
                .or_else(|| tokens.get(index + 1).copied());
            let Some(value) = value else { continue };
            if token.starts_with("--data-dir") {
                self.data_dir = Some(PathBuf::from(value));
            } else if token.starts_with("--write-kubeconfig") {
                self.write_kubeconfig = Some(PathBuf::from(value));
            } else if token.starts_with("--service-cidr") {
                self.service_cidr = Some(value.to_string());
            } else if token.starts_with("--cluster-cidr") {
                self.cluster_cidr = Some(value.to_string());
            } else if token.starts_with("--cluster-domain") {
                self.cluster_domain = Some(value.to_string());
            } else if token.starts_with("--cluster-dns") {
                self.cluster_dns = Some(value.to_string());
            } else if token.starts_with("--datastore-endpoint") {
                self.datastore_endpoint = Some(value.to_string());
            } else if token.starts_with("--flannel-backend") {
                self.flannel_backend = Some(value.to_string());
                self.disable_flannel |= value == "none";
            } else if token.starts_with("--disable") {
                self.disable_flannel |= value.split(',').any(|item| item == "flannel");
            }
        }
    }
}

fn yaml_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn yaml_bool(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn yaml_string_list(value: &Value, key: &str) -> Vec<String> {
    match value.get(key) {
        Some(Value::Sequence(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(value)) => vec![value.clone()],
        _ => Vec::new(),
    }
}

fn k3s_config_files(layout: &HostLayout) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let main = layout.path("/etc/rancher/k3s/config.yaml");
    if main.is_file() {
        files.push(strip_root(&layout.root, main));
    }
    let dropin = layout.path("/etc/rancher/k3s/config.yaml.d");
    if let Ok(entries) = std::fs::read_dir(dropin) {
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("yaml" | "yml")
                )
            })
            .collect();
        paths.sort();
        files.extend(paths.into_iter().map(|path| strip_root(&layout.root, path)));
    }
    files
}

fn read_systemd_exec_start(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading service unit {}", path.display()))?;
    let mut exec = String::new();
    for line in text.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("ExecStart=") {
            exec.push_str(value.trim_end_matches('\\'));
            exec.push(' ');
        } else if !exec.is_empty() && line.ends_with('\\') {
            exec.push_str(line.trim_end_matches('\\'));
            exec.push(' ');
        } else if !exec.is_empty() {
            exec.push_str(line);
            break;
        }
    }
    Ok(exec)
}

fn locate_binary(layout: &HostLayout, command: Option<&str>, data_dir: &Path) -> Option<PathBuf> {
    let command_path = command.and_then(|exec| {
        exec.split_whitespace()
            .find(|part| part.ends_with("/k3s") || *part == "k3s")
            .map(|part| part.strip_prefix("ExecStart=").unwrap_or(part))
    });
    let candidates = [
        command_path.map(PathBuf::from),
        Some(PathBuf::from("/usr/local/bin/k3s")),
        Some(PathBuf::from("/usr/bin/k3s")),
        Some(PathBuf::from("/usr/sbin/k3s")),
        Some(data_dir.join("data/current/bin/k3s")),
    ];
    candidates
        .into_iter()
        .flatten()
        .map(|path| append(&layout.root, &path))
        .find(|path| path.is_file())
        .map(|path| strip_root(&layout.root, path))
}

fn detect_service_manager(
    layout: &HostLayout,
    has_systemd_unit: bool,
    has_openrc_script: bool,
    has_sysv_script: bool,
    has_runit_service: bool,
) -> Option<ServiceManager> {
    if has_systemd_unit || layout.path("/run/systemd/system").is_dir() {
        Some(ServiceManager::Systemd)
    } else if (has_openrc_script || layout.path("/run/openrc/softlevel").is_file()) {
        Some(ServiceManager::OpenRc)
    } else if has_runit_service || layout.path("/run/runit/service").is_dir() {
        Some(ServiceManager::Runit)
    } else if has_sysv_script && layout.path("/etc/init.d/rc").exists() {
        Some(ServiceManager::SysVInit)
    } else {
        None
    }
}

fn first_file(layout: &HostLayout, candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|path| layout.path(path))
        .find(|path| path.is_file())
        .map(|path| strip_root(&layout.root, path))
}

fn first_dir(layout: &HostLayout, candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|path| layout.path(path))
        .find(|path| path.is_dir())
        .map(|path| strip_root(&layout.root, path))
}

fn append(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        root.join(path.strip_prefix("/").unwrap_or(path))
    } else {
        root.join(path)
    }
}

fn strip_root(root: &Path, path: PathBuf) -> PathBuf {
    path.strip_prefix(root)
        .map(|relative| Path::new("/").join(relative))
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{inspect_host, HostLayout, K3sDatastore, ServiceManager};

    #[test]
    fn detects_systemd_k3s_and_kine_config() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("etc/systemd/system")).unwrap();
        fs::create_dir_all(root.path().join("etc/rancher/k3s")).unwrap();
        fs::create_dir_all(root.path().join("var/lib/rancher/k3s/server/db")).unwrap();
        fs::write(
            root.path().join("etc/systemd/system/k3s.service"),
            "[Service]\nExecStart=/usr/local/bin/k3s server --cluster-cidr=10.44.0.0/16\n",
        )
        .unwrap();
        fs::write(
            root.path().join("etc/rancher/k3s/config.yaml"),
            "service-cidr: 10.45.0.0/16\ncluster-domain: corp.local\n",
        )
        .unwrap();
        fs::create_dir_all(root.path().join("etc/rancher/k3s/config.yaml.d")).unwrap();
        fs::write(
            root.path()
                .join("etc/rancher/k3s/config.yaml.d/10-domain.yaml"),
            "cluster-domain: override.local\n",
        )
        .unwrap();
        fs::write(
            root.path().join("var/lib/rancher/k3s/server/db/state.db"),
            "",
        )
        .unwrap();

        let found = inspect_host(&HostLayout::under(root.path()))
            .unwrap()
            .unwrap();
        assert_eq!(found.service_manager, Some(ServiceManager::Systemd));
        let cluster = found.cluster.unwrap();
        assert_eq!(cluster.datastore, K3sDatastore::Kine);
        assert_eq!(cluster.cluster_cidr.as_deref(), Some("10.44.0.0/16"));
        assert_eq!(cluster.service_cidr.as_deref(), Some("10.45.0.0/16"));
        assert_eq!(cluster.cluster_domain.as_deref(), Some("override.local"));
        assert_eq!(cluster.cni.as_deref(), Some("flannel"));
    }

    #[test]
    fn detects_openrc_and_embedded_etcd() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("etc/init.d")).unwrap();
        fs::write(root.path().join("etc/init.d/rc"), "#!/bin/sh\n").unwrap();
        fs::create_dir_all(root.path().join("etc/rancher/k3s")).unwrap();
        fs::create_dir_all(root.path().join("var/lib/rancher/k3s/server/db/etcd")).unwrap();
        fs::write(root.path().join("etc/init.d/k3s"), "#!/sbin/openrc-run\n").unwrap();
        fs::write(
            root.path().join("etc/rancher/k3s/config.yaml"),
            "cluster-init: true\n",
        )
        .unwrap();

        let found = inspect_host(&HostLayout::under(root.path()))
            .unwrap()
            .unwrap();
        assert_eq!(found.service_manager, Some(ServiceManager::OpenRc));
        assert_eq!(found.cluster.unwrap().datastore, K3sDatastore::Etcd);
    }

    #[test]
    fn distinguishes_sysv_init_from_openrc() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("etc/init.d")).unwrap();
        fs::create_dir_all(root.path().join("etc/rancher/k3s")).unwrap();
        fs::write(root.path().join("etc/init.d/k3s"), "#!/bin/sh\n").unwrap();
        fs::write(
            root.path().join("etc/rancher/k3s/config.yaml"),
            "datastore-endpoint: sqlite:///var/lib/k3s/state.db\n",
        )
        .unwrap();
        let found = inspect_host(&HostLayout::under(root.path()))
            .unwrap()
            .unwrap();
        assert_eq!(found.service_manager, Some(ServiceManager::SysVInit));
    }

    #[test]
    fn reports_absent_installation_without_inventing_defaults() {
        let root = tempfile::tempdir().unwrap();
        assert!(inspect_host(&HostLayout::under(root.path()))
            .unwrap()
            .is_none());
    }
}
