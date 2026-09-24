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
        let installation = detect::inspect_host(&detect::HostLayout::system())?;
        let report = detect::InventoryReport::from_installation(installation);
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("encoding inventory report")?
        );
        return Ok(());
    }

    let request = request::MigrationRequest::parse(&args)?;
    let installation = detect::inspect_host(&detect::HostLayout::system())?;
    let installation =
        installation.context("no supported local Kubernetes installation was detected")?;
    request.validate_source(&installation)?;
    ensure!(
        request.from == request::Distribution::K3s && request.to == request::Distribution::Nodestore,
        "this release implements K3s-to-nodestore migration; the reverse migration path is not available yet"
    );
    if request.uninstall_after_migrate {
        service::require_supported_uninstall(&installation)?;
    }
    service::validate_disable_support(&installation)?;
    ensure!(
        is_root(),
        "run nodemigrate as root so it can preserve service state and write the protected export"
    );
    let source_api = transfer::KubeApi::source(&installation)?;
    let bootstrap = bootstrap_command(&installation)?;
    let target_api = transfer::KubeApi::destination()?;
    if request.plan_only {
        source_api.ready()?;
        source_api.single_node()?;
        source_api.validate_no_volumes()?;
        println!("Migration plan: K3s API objects -> nodestore; source service will be disabled; uninstall-after-migrate={}", request.uninstall_after_migrate);
        return Ok(());
    }

    let export = source_api.export(&installation)?;
    println!(
        "Protected API object export saved at {}",
        export.dir.display()
    );
    let previous_service = service::disable(&installation)?;
    if let Err(error) = run_bootstrap(bootstrap) {
        if target_api.ready().is_ok() {
            bail!("nodebootstrap failed ({error:#}) but a destination API is already answering; source K3s remains disabled to avoid a port conflict; recovery export: {}", export.dir.display());
        }
        let restore_result = service::restore(&installation, previous_service);
        if let Err(restore_error) = restore_result {
            bail!("nodebootstrap failed ({error:#}); restoring the source service also failed ({restore_error:#}); source remains disabled");
        }
        return Err(error).context("nodestore bootstrap failed; original K3s service was restored");
    }

    wait_for_target(&target_api).context(format!(
        "destination did not become ready; source K3s remains disabled and the protected export is at {}",
        export.dir.display()
    ))?;
    if let Err(error) = target_api.import(&export) {
        bail!("destination bootstrap succeeded but Kubernetes object import failed: {error:#}; source K3s remains disabled and the protected export is at {}", export.dir.display());
    }
    wait_for_target(&target_api)?;
    if request.uninstall_after_migrate {
        service::uninstall_k3s()?;
        run_bootstrap(bootstrap_command(&installation)?)
            .context("reconciling nodestore after the K3s uninstall script")?;
        wait_for_target(&target_api)
            .context("destination failed readiness after K3s uninstall cleanup")?;
    }
    println!("Migration completed and the nodestore API passed readiness and single-node checks. Export retained at {}", export.dir.display());
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

fn wait_for_target(target: &transfer::KubeApi) -> Result<()> {
    let mut last_error = None;
    for _ in 0..60 {
        match target.ready().and_then(|()| target.single_node()) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("destination cluster did not become ready")))
        .context("waiting for the destination API and node to become ready")
}

fn bootstrap_command(installation: &detect::Installation) -> Result<Command> {
    let mut args = vec!["--release".to_string()];
    let config = installation
        .cluster
        .as_ref()
        .context("K3s cluster config was not detected")?;
    ensure!(config.cni.as_deref() == Some("flannel"),
        "migration currently requires K3s's bundled flannel CNI; external or disabled CNI migration is not supported");
    if config.cni.is_some() {
        ensure!(
            config.flannel_backend.as_deref().unwrap_or("vxlan") == "vxlan",
            "K3s flannel backend '{}' cannot be represented by nodebootstrap's current CNI setup",
            config.flannel_backend.as_deref().unwrap_or_default()
        );
    }
    args.push("--cni=flannel".to_string());
    if let Some(domain) = &config.cluster_domain {
        args.push(format!("--cluster-domain={domain}"));
    }
    let service_cidrs = split_cidrs(config.service_cidr.as_deref().unwrap_or("10.43.0.0/16"));
    ensure!(
        service_cidrs
            .iter()
            .filter(|cidr| !cidr.contains(':'))
            .count()
            <= 1
            && service_cidrs
                .iter()
                .filter(|cidr| cidr.contains(':'))
                .count()
                <= 1,
        "nodebootstrap supports at most one IPv4 and one IPv6 service CIDR"
    );
    if let Some(ipv4) = service_cidrs.iter().find(|cidr| !cidr.contains(':')) {
        args.push(format!("--cidr={ipv4}"));
    }
    if let Some(ipv6) = service_cidrs.iter().find(|cidr| cidr.contains(':')) {
        args.push(format!("--cidr6={ipv6}"));
    }
    let cluster_cidrs = split_cidrs(config.cluster_cidr.as_deref().unwrap_or("10.42.0.0/16"));
    ensure!(
        cluster_cidrs
            .iter()
            .filter(|cidr| !cidr.contains(':'))
            .count()
            <= 1
            && cluster_cidrs
                .iter()
                .filter(|cidr| cidr.contains(':'))
                .count()
                <= 1,
        "nodebootstrap supports at most one IPv4 and one IPv6 pod CIDR"
    );
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
    if let Some(configured_dns) = config.cluster_dns.as_deref() {
        let expected_dns = service_ip_plus_offset(
            service_cidrs
                .iter()
                .find(|cidr| !cidr.contains(':'))
                .copied()
                .unwrap_or("10.43.0.0/16"),
            10,
        )?;
        ensure!(configured_dns == expected_dns,
            "K3s cluster-dns {configured_dns} differs from nodebootstrap's derived service DNS {expected_dns}");
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
    Ok(command)
}

fn split_cidrs(value: &str) -> Vec<&str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|cidr| !cidr.is_empty())
        .collect()
}

fn service_ip_plus_offset(cidr: &str, offset: u32) -> Result<String> {
    let (address, prefix) = cidr
        .split_once('/')
        .context("service CIDR is missing its prefix")?;
    let address = address
        .parse::<std::net::Ipv4Addr>()
        .context("service CIDR is not IPv4")?;
    let prefix = prefix
        .parse::<u8>()
        .context("service CIDR prefix is invalid")?;
    ensure!(prefix <= 32, "service CIDR prefix is invalid");
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Ok(std::net::Ipv4Addr::from(u32::from(address) | (offset & !mask)).to_string())
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
         \x20 nodemigrate to=k3s from=nodestore [uninstall-after-migrate=true]\n\
         \n\
         `inspect` reports detected local Kubernetes installations. Migration\n\
         exports Kubernetes API objects into a protected recovery directory,\n\
         disables the source service, and keeps the export. Source uninstall\n\
         happens only with uninstall-after-migrate=true."
    );
}
