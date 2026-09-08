//! Removal of the local installation owned by nodebootstrap.
//!
//! This intentionally does not attempt to delete arbitrary Kubernetes
//! objects. The control plane is local state, while workload cleanup is the
//! operator's choice; this command removes the host services and artifacts
//! that bootstrap created.

use anyhow::Result;

use crate::config::Config;

const SERVICES: &[&str] = &[
    "flanneld",
    "kube-apiserver",
    "nodeapiserver",
    "nodestore",
    "nodescheduler",
    "nodecontroller",
    "nodelet",
    "nodeproxy",
];

pub fn run(cfg: &Config) -> Result<()> {
    let cni_owned = cfg.cni_marker().is_file();
    let containerd_owned = cfg.containerd_marker().is_file();
    let mut failures = Vec::new();

    for service in SERVICES {
        crate::service_mgr::remove(cfg, service);
    }
    if containerd_owned {
        crate::service_mgr::remove(cfg, "containerd");
    }

    if cni_owned {
        remove_file(std::path::Path::new("/etc/cni/net.d/10-flannel.conflist"), &mut failures);
        remove_file(std::path::Path::new("/opt/cni/bin/flannel"), &mut failures);
        remove_dir(std::path::Path::new("/etc/kube-flannel"), &mut failures);
        remove_dir(std::path::Path::new("/run/flannel"), &mut failures);
    }

    if containerd_owned {
        remove_file(std::path::Path::new("/etc/containerd/config.toml"), &mut failures);
        remove_dir(std::path::Path::new("/var/lib/containerd"), &mut failures);
        remove_dir(std::path::Path::new("/run/containerd"), &mut failures);
    }

    remove_nftables_table();
    remove_tracked_packages(&mut failures);

    // These paths are all nodebootstrap's configured paths, not discovered
    // from a broad filesystem search. Removing the exact configured
    // directories also handles artifacts from source/release installation.
    for path in [
        cfg.kubeconfig_dir(),
        cfg.toolchain_dir(),
        cfg.src_dir(),
        cfg.work_dir(),
        cfg.log_dir(),
        cfg.pki_dir(),
        cfg.nodelet_server_cert_dir(),
    ] {
        remove_dir(&path, &mut failures);
    }

    let nodestore_data = std::env::var("NODESTORE_DATA_DIR")
        .unwrap_or_else(|_| "/var/lib/nodestore".to_string());
    remove_dir(std::path::Path::new(&nodestore_data), &mut failures);
    remove_file(&cfg.flags_path(), &mut failures);
    remove_file(&cfg.cni_marker(), &mut failures);
    remove_file(&cfg.containerd_marker(), &mut failures);
    remove_empty_dir(&cfg.state_dir(), &mut failures);

    if failures.is_empty() {
        tracing::info!("nodebootstrap installation removed");
        Ok(())
    } else {
        anyhow::bail!("nodebootstrap uninstall completed with errors: {}", failures.join("; "))
    }
}

fn remove_tracked_packages(failures: &mut Vec<String>) {
    if let Err(error) = crate::pkg::remove_tracked_packages() {
        failures.push(error.to_string());
    }
}

fn remove_file(path: &std::path::Path, failures: &mut Vec<String>) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::info!(path = %path.display(), "removed nodebootstrap file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => failures.push(format!("removing {}: {error}", path.display())),
    }
}

fn remove_dir(path: &std::path::Path, failures: &mut Vec<String>) {
    if !path.exists() {
        return;
    }
    if path.parent().is_none()
        || matches!(
            path.to_str(),
            Some("/etc" | "/opt" | "/run" | "/tmp" | "/usr" | "/var" | "/var/lib")
        )
    {
        failures.push(format!("refusing to remove broad system directory {}", path.display()));
        return;
    }
    match std::fs::remove_dir_all(path) {
        Ok(()) => tracing::info!(path = %path.display(), "removed nodebootstrap directory"),
        Err(error) => failures.push(format!("removing {}: {error}", path.display())),
    }
}

fn remove_empty_dir(path: &std::path::Path, failures: &mut Vec<String>) {
    if !path.is_dir() {
        return;
    }
    match std::fs::remove_dir(path) {
        Ok(()) => tracing::info!(path = %path.display(), "removed empty nodebootstrap state directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
        Err(error) => failures.push(format!("removing {}: {error}", path.display())),
    }
}

fn remove_nftables_table() {
    if !crate::pkg::command_exists("nft") {
        return;
    }
    let status = std::process::Command::new("nft")
        .args(["delete", "table", "inet", "not_k8s_svc"])
        .status();
    if let Ok(status) = status {
        if status.success() {
            tracing::info!("removed nodeproxy nftables table");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SERVICES;

    #[test]
    fn uninstall_covers_every_bootstrap_service() {
        for service in [
            "flanneld",
            "kube-apiserver",
            "nodeapiserver",
            "nodestore",
            "nodescheduler",
            "nodecontroller",
            "nodelet",
            "nodeproxy",
        ] {
            assert!(SERVICES.contains(&service));
        }
    }
}
