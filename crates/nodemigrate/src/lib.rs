pub mod detect;
pub mod request;
pub mod service;
pub mod transfer;

use std::{path::PathBuf, process::Command};

use anyhow::{bail, ensure, Context, Result};

pub fn run(args: impl IntoIterator<Item = String>) -> Result<()> {
    let args: Vec<String> = args.into_iter().collect();
    if args.is_empty()
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "help" | "--help" | "-h"))
    {
        print_help();
        return Ok(());
    }

    if args.as_slice() == ["inspect"] {
        let installations = detect::inspect_all(&detect::HostLayout::system())?;
        let report = detect::InventoryReport::from_installations(installations);
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("encoding inventory report")?
        );
        return Ok(());
    }

    let request = request::MigrationRequest::parse(&args)?;
    let layout = detect::HostLayout::system();
    let source = detect::inspect_distribution(&layout, request.from)?
        .with_context(|| format!("no local {:?} installation was detected", request.from))?;
    request.validate_source(&source)?;
    if request.uninstall_after_migrate {
        service::require_supported_uninstall(&source)?;
    }
    if request.to == request::Distribution::Nodestore {
        migrate_to_nodestore(&request, &source)
    } else {
        let target = detect::inspect_distribution(&layout, request.to)?.with_context(|| {
            format!(
                "no retained local {:?} target installation was detected",
                request.to
            )
        })?;
        migrate_to_existing(&request, &source, &target)
    }
}

fn migrate_to_nodestore(
    request: &request::MigrationRequest,
    source: &detect::Installation,
) -> Result<()> {
    service::validate_disable_support(source)?;
    ensure!(
        is_root(),
        "run nodemigrate as root so it can preserve service state and write the protected export"
    );
    let source_api = transfer::KubeApi::source(source)?;
    let source_nodes = source_api.node_count()?;
    let joins_existing = std::env::var_os("NODEBOOTSTRAP_JOIN_ENDPOINT").is_some();
    let replacement_member_id = if joins_existing {
        std::env::var("NODEBOOTSTRAP_MEMBER_ID")
            .ok()
            .map(|id| {
                ensure!(!id.is_empty(), "NODEBOOTSTRAP_MEMBER_ID is empty");
                id.parse::<u64>()
                    .context("NODEBOOTSTRAP_MEMBER_ID must be the old nodestore member id")
            })
            .transpose()?
    } else {
        None
    };
    let bootstrap = bootstrap_command(source)?;
    let target_api = transfer::KubeApi::destination(request::Distribution::Nodestore)?;
    if request.plan_only {
        source_api.ready()?;
        let cni = source
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.cni.as_deref())
            .unwrap_or("external or undetected");
        let replacement = replacement_member_id
            .map(|id| format!("; replace existing member {id} after the new node is Ready"))
            .unwrap_or_default();
        println!("Migration plan: {:?} -> nodestore; source nodes={source_nodes}; destination={}; source CNI={cni}{replacement}; {}source service will be disabled; uninstall-after-migrate={}", request.from, if joins_existing { "existing cluster" } else { "new cluster" }, if joins_existing { "install node agent on replacement node; " } else { "" }, request.uninstall_after_migrate);
        return Ok(());
    }

    let mut export = source_api.export(source)?;
    println!(
        "Protected API object export saved at {}",
        export.dir.display()
    );
    let previous_service = service::disable(source)?;
    if let Err(error) = export.snapshot_host_paths() {
        if let Err(restore_error) = service::restore(source, previous_service) {
            bail!("snapshotting local persistent volumes failed ({error:#}); restoring the source service also failed ({restore_error:#})");
        }
        return Err(error)
            .context("local persistent volume snapshot failed; original service was restored");
    }
    if let Err(error) = run_bootstrap(bootstrap) {
        if target_api.ready().is_ok() {
            bail!("nodebootstrap failed ({error:#}) but a destination API is already answering; source remains disabled to avoid a port conflict; recovery export: {}", export.dir.display());
        }
        if let Err(restore_error) = service::restore(source, previous_service) {
            bail!("nodebootstrap failed ({error:#}); restoring the source service also failed ({restore_error:#}); source remains disabled");
        }
        return Err(error).context("nodestore bootstrap failed; original service was restored");
    }

    wait_for_api(&target_api).context(format!(
        "destination did not become ready; source remains disabled and the protected export is at {}",
        export.dir.display()
    ))?;
    if let Err(error) = target_api.import(&export) {
        bail!("destination bootstrap succeeded but Kubernetes object import failed: {error:#}; source remains disabled and the protected export is at {}", export.dir.display());
    }
    if joins_existing {
        let worker = replacement_worker_command(source, &node_name(source))?;
        run_bootstrap(worker).with_context(|| format!(
            "installing the node agent on the joined replacement node; source remains disabled and the protected export is at {}",
            export.dir.display()
        ))?;
    }
    let node_name = node_name(source);
    wait_for_node(&target_api, &node_name)?;
    if joins_existing {
        if let Some(old_member_id) = replacement_member_id {
            run_bootstrap(replace_member_command(&old_member_id.to_string())?).with_context(|| format!(
                "promoting this Ready replacement before retiring old nodestore member {old_member_id}; source remains disabled and recovery export is at {}",
                export.dir.display()
            ))?;
        }
    }
    if request.uninstall_after_migrate {
        service::uninstall_source(source)?;
        export.restore_host_paths()?;
        run_bootstrap(bootstrap_command(source)?)
            .context("reconciling nodestore after source uninstall")?;
        wait_for_api(&target_api)
            .context("destination failed API readiness after K3s uninstall cleanup")?;
        wait_for_node(&target_api, &node_name)
            .context("replacement node failed readiness after K3s uninstall cleanup")?;
    }
    println!("Migration completed and the destination API passed readiness checks. Export retained at {}", export.dir.display());
    Ok(())
}

fn migrate_to_existing(
    request: &request::MigrationRequest,
    source: &detect::Installation,
    target: &detect::Installation,
) -> Result<()> {
    ensure!(
        source.distribution == request::Distribution::Nodestore,
        "migration to an existing Kubernetes installation currently starts from nodestore"
    );
    service::validate_disable_support(source)?;
    ensure!(
        is_root(),
        "run nodemigrate as root to control both service stacks"
    );
    let source_api = transfer::KubeApi::source(source)?;
    let target_api = transfer::KubeApi::destination(request.to)?;
    source_api.ready()?;
    if request.plan_only {
        let cni = target
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.cni.as_deref())
            .unwrap_or("external or undetected");
        println!("Migration plan: nodestore -> {:?}; retained target service '{}' will be enabled and started; target CNI={cni}; uninstall-after-migrate={}", request.to, target.service_name, request.uninstall_after_migrate);
        return Ok(());
    }
    let mut export = source_api.export(source)?;
    println!(
        "Protected API object export saved at {}",
        export.dir.display()
    );
    let previous_service = service::disable(source)?;
    if let Err(error) = export.snapshot_host_paths() {
        if let Err(restore_error) = service::restore(source, previous_service) {
            bail!("snapshotting local persistent volumes failed ({error:#}); restoring nodestore also failed ({restore_error:#})");
        }
        return Err(error)
            .context("local persistent volume snapshot failed; nodestore was restored");
    }
    if let Err(error) = service::activate(target) {
        if let Err(restore_error) = service::restore(source, previous_service) {
            bail!("starting the retained target failed ({error:#}); restoring nodestore also failed ({restore_error:#})");
        }
        return Err(error).context("starting the retained target; nodestore was restored");
    }
    wait_for_api(&target_api).context("retained destination API did not become ready")?;
    if let Err(error) = target_api.import(&export) {
        bail!("destination started but API import failed: {error:#}; nodestore remains stopped and export is at {}", export.dir.display());
    }
    wait_for_node(&target_api, &node_name(target))?;
    if request.uninstall_after_migrate {
        service::uninstall_source(source)?;
        export.restore_host_paths()?;
    }
    println!(
        "Migration to {:?} completed. Export retained at {}",
        request.to,
        export.dir.display()
    );
    Ok(())
}

fn run_bootstrap(mut command: Command) -> Result<()> {
    let status = command
        .status()
        .context("launching nodebootstrap for the nodestore destination")?;
    ensure!(
        status.success(),
        "nodebootstrap exited with status {status}"
    );
    Ok(())
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn wait_for_api(target: &transfer::KubeApi) -> Result<()> {
    let mut last_error = None;
    for _ in 0..60 {
        match target.ready() {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("destination cluster did not become ready")))
        .context("waiting for the destination API and node to become ready")
}

fn wait_for_node(target: &transfer::KubeApi, name: &str) -> Result<()> {
    let mut last_error = None;
    for _ in 0..60 {
        match target.ready().and_then(|()| target.node_ready(name)) {
            Ok(true) => return Ok(()),
            Ok(false) => last_error = Some(anyhow::anyhow!("node {name} is not Ready")),
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("node {name} did not become Ready")))
        .context("waiting for the replacement node")
}

fn hostname() -> String {
    std::process::Command::new("uname")
        .arg("-n")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

fn node_name(installation: &detect::Installation) -> String {
    installation
        .cluster
        .as_ref()
        .and_then(|cluster| cluster.node_name.as_deref())
        .map(str::to_owned)
        .or_else(|| std::env::var("NODELET_NODE_NAME").ok())
        .unwrap_or_else(hostname)
}

fn bootstrap_command(installation: &detect::Installation) -> Result<Command> {
    let mut args = vec!["--release".to_string()];
    if let Some(endpoint) = std::env::var_os("NODEBOOTSTRAP_JOIN_ENDPOINT") {
        let endpoint = endpoint.to_string_lossy();
        ensure!(!endpoint.is_empty(), "NODEBOOTSTRAP_JOIN_ENDPOINT is empty");
        let peer_url = std::env::var("NODEBOOTSTRAP_PEER_URL")
            .context("joining an existing nodestore cluster requires NODEBOOTSTRAP_PEER_URL")?;
        args.push("--control-plane".to_string());
        args.push(format!("--join={endpoint}"));
        args.push(format!("--peer-url={peer_url}"));
    }
    let config = installation
        .cluster
        .as_ref()
        .context("source cluster config was not detected")?;
    // nodebootstrap installs Flannel itself; all other providers remain
    // externally managed on the host and are restored from the API export.
    args.push(
        if config.cni.as_deref() == Some("flannel")
            && config.flannel_backend.as_deref().unwrap_or("vxlan") == "vxlan"
        {
            "--cni=flannel".to_string()
        } else {
            "--cni=none".to_string()
        },
    );
    if let Some(node_name) = &config.node_name {
        args.push(format!("--node-name={node_name}"));
    }
    bootstrap_command_with_config(args, config)
}

fn replacement_worker_command(
    installation: &detect::Installation,
    node_name: &str,
) -> Result<Command> {
    let config = installation
        .cluster
        .as_ref()
        .context("source cluster config was not detected")?;
    let kubeconfig = std::env::var_os("NODEBOOTSTRAP_WORKER_KUBECONFIG")
        .or_else(|| std::env::var_os("NODEMIGRATE_DESTINATION_KUBECONFIG"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("NODEBOOTSTRAP_KUBECONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/etc/nodebootstrap"))
                .join("admin.kubeconfig")
        });
    let args = replacement_worker_args(config, &kubeconfig, node_name);
    bootstrap_command_with_config(args, config)
}

fn replace_member_command(old_member_id: &str) -> Result<Command> {
    let endpoint = std::env::var("NODEBOOTSTRAP_JOIN_ENDPOINT")
        .context("member replacement requires NODEBOOTSTRAP_JOIN_ENDPOINT")?;
    let peer_url = std::env::var("NODEBOOTSTRAP_PEER_URL")
        .context("member replacement requires NODEBOOTSTRAP_PEER_URL")?;
    let binary = find_executable(&["notk8s", "nodebootstrap"])
        .context("could not find the combined notk8s or standalone nodebootstrap binary on PATH")?;
    let mut command = Command::new(binary);
    if command.get_program().to_string_lossy().ends_with("notk8s") {
        command.arg("bootstrap");
    }
    command.args([
        "--release".to_string(),
        format!("--join={endpoint}"),
        format!("--peer-url={peer_url}"),
        format!("--member-id={old_member_id}"),
        "replace-member".to_string(),
    ]);
    Ok(command)
}

fn replacement_worker_args(
    config: &detect::ClusterConfig,
    kubeconfig: &std::path::Path,
    node_name: &str,
) -> Vec<String> {
    let mut args = vec![
        "--release".to_string(),
        "--worker".to_string(),
        format!("--kubeconfig={}", kubeconfig.display()),
        format!("--node-name={node_name}"),
        if config.cni.as_deref() == Some("flannel")
            && config.flannel_backend.as_deref().unwrap_or("vxlan") == "vxlan"
        {
            "--cni=flannel".to_string()
        } else {
            "--cni=none".to_string()
        },
    ];
    if let Some(domain) = &config.cluster_domain {
        args.push(format!("--cluster-domain={domain}"));
    }
    args
}

fn bootstrap_command_with_config(
    mut args: Vec<String>,
    config: &detect::ClusterConfig,
) -> Result<Command> {
    if let Some(domain) = &config.cluster_domain {
        if !args.iter().any(|arg| arg.starts_with("--cluster-domain=")) {
            args.push(format!("--cluster-domain={domain}"));
        }
    }
    let service_cidrs = split_cidrs(config.service_cidr.as_deref().unwrap_or("10.43.0.0/16"));
    if let Some(ipv4) = service_cidrs.iter().find(|cidr| !cidr.contains(':')) {
        args.push(format!("--cidr={ipv4}"));
    }
    if let Some(ipv6) = service_cidrs.iter().find(|cidr| cidr.contains(':')) {
        args.push(format!("--cidr6={ipv6}"));
    }
    let cluster_cidrs = split_cidrs(config.cluster_cidr.as_deref().unwrap_or("10.42.0.0/16"));
    let ipv4_cluster_cidr = cluster_cidrs
        .iter()
        .find(|cidr| !cidr.contains(':'))
        .copied()
        .unwrap_or("10.42.0.0/16");
    let ipv6_cluster_cidr = cluster_cidrs
        .iter()
        .find(|cidr| cidr.contains(':'))
        .copied()
        .unwrap_or("fd00:42::/56");
    let mut cluster_dns_ipv4 = None;
    let mut cluster_dns_ipv6 = None;
    if let Some(dns) = config.cluster_dns.as_deref() {
        for address in split_cidrs(dns) {
            match address
                .parse::<std::net::IpAddr>()
                .context("source cluster-dns must contain IP addresses")?
            {
                std::net::IpAddr::V4(address) => cluster_dns_ipv4 = Some(address.to_string()),
                std::net::IpAddr::V6(address) => cluster_dns_ipv6 = Some(address.to_string()),
            }
        }
    }
    let binary = find_executable(&["notk8s", "nodebootstrap"])
        .context("could not find the combined notk8s or standalone nodebootstrap binary on PATH")?;
    let mut command = Command::new(binary);
    if command.get_program().to_string_lossy().ends_with("notk8s") {
        command.arg("bootstrap");
    }
    command
        .args(args)
        .env("NODEBOOTSTRAP_IPV4_CLUSTER_CIDR", ipv4_cluster_cidr)
        .env("NODEBOOTSTRAP_IPV6_CLUSTER_CIDR", ipv6_cluster_cidr);
    if let Some(address) = cluster_dns_ipv4 {
        command.env("NODEBOOTSTRAP_CLUSTER_DNS_IP", address);
    }
    if let Some(address) = cluster_dns_ipv6 {
        command.env("NODEBOOTSTRAP_CLUSTER_DNS_IP6", address);
    }
    Ok(command)
}

fn split_cidrs(value: &str) -> Vec<&str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|cidr| !cidr.is_empty())
        .collect()
}

fn find_executable(names: &[&str]) -> Option<PathBuf> {
    let mut directories: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            directories.push(parent.to_path_buf());
        }
    }
    directories.extend([PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin")]);
    for directory in directories {
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if candidate.metadata().ok()?.permissions().mode() & 0o111 == 0 {
                        continue;
                    }
                }
                return Some(candidate);
            }
        }
    }
    None
}

fn print_help() {
    println!(
        "nodemigrate\n\
         Usage:\n\
         \x20 nodemigrate inspect\n\
         \x20 nodemigrate to=nodestore from=k3s [uninstall-after-migrate=true]\n\
         \x20 nodemigrate to=nodestore from=kubernetes\n\
         \x20 nodemigrate to=k3s from=nodestore\n\
         \x20 nodemigrate to=kubernetes from=nodestore\n\
         \n\
         `inspect` reports detected local Kubernetes installations. Migration\n\
         exports Kubernetes API objects into a protected recovery directory,\n\
         disables the source service, and keeps the export. Source uninstall\n\
         happens only with uninstall-after-migrate=true."
    );
}

#[cfg(test)]
mod tests {
    use super::replacement_worker_args;
    use crate::detect::{ClusterConfig, K3sDatastore};
    use std::path::Path;

    fn cluster(cni: Option<&str>, backend: Option<&str>) -> ClusterConfig {
        ClusterConfig {
            data_dir: "/var/lib/test".into(),
            kubeconfig: None,
            service_cidr: Some("10.96.0.0/12".to_string()),
            cluster_cidr: Some("10.244.0.0/16".to_string()),
            cluster_domain: Some("cluster.example".to_string()),
            cluster_dns: None,
            node_name: Some("old-node".to_string()),
            cni: cni.map(str::to_string),
            flannel_backend: backend.map(str::to_string),
            datastore: Some(K3sDatastore::Etcd),
        }
    }

    #[test]
    fn joined_replacement_runs_worker_bootstrap_with_destination_kubeconfig() {
        let config = cluster(None, None);
        let args = replacement_worker_args(
            &config,
            Path::new("/etc/nodebootstrap/admin.kubeconfig"),
            "old-node",
        );
        assert!(args.iter().any(|arg| arg == "--worker"));
        assert!(args
            .iter()
            .any(|arg| arg == "--kubeconfig=/etc/nodebootstrap/admin.kubeconfig"));
        assert!(args.iter().any(|arg| arg == "--node-name=old-node"));
        assert!(args.iter().any(|arg| arg == "--cni=none"));
        assert!(args
            .iter()
            .any(|arg| arg == "--cluster-domain=cluster.example"));
    }

    #[test]
    fn joined_replacement_preserves_explicit_flannel_setup() {
        let config = cluster(Some("flannel"), Some("vxlan"));
        let args = replacement_worker_args(&config, Path::new("/tmp/admin.kubeconfig"), "node");
        assert!(args.iter().any(|arg| arg == "--cni=flannel"));
    }

    #[test]
    fn joined_replacement_does_not_replace_non_vxlan_or_external_cni() {
        let config = cluster(Some("flannel"), Some("wireguard-native"));
        let args = replacement_worker_args(&config, Path::new("/tmp/admin.kubeconfig"), "node");
        assert!(args.iter().any(|arg| arg == "--cni=none"));
    }
}
