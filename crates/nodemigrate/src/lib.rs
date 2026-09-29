pub mod detect;
pub mod request;
pub mod service;
pub mod transfer;

use std::{
    io::{self, BufRead, IsTerminal, Write},
    path::PathBuf,
    process::Command,
};

use anyhow::{Context, Error, Result, bail, ensure};

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
        if !request.plan_only {
            confirm_migration_if_interactive()?;
        }
        migrate_to_nodestore(&request, &source)
    } else {
        let target = detect::inspect_distribution(&layout, request.to)?.with_context(|| {
            format!(
                "no retained local {:?} target installation was detected",
                request.to
            )
        })?;
        if !request.plan_only {
            confirm_migration_if_interactive()?;
        }
        migrate_to_existing(&request, &source, &target)
    }
}

fn confirm_migration_if_interactive() -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let interactive = stdin.is_terminal() && stdout.is_terminal();
    if interactive {
        confirm_migration(true, &mut stdin.lock(), &mut stdout.lock())
    } else {
        // Keep the risk notice visible in service-manager and CI logs when no
        // terminal is attached, without blocking automated invocations.
        confirm_migration(false, &mut stdin.lock(), &mut io::stderr().lock())
    }
}

fn confirm_migration(
    interactive: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    writeln!(
        output,
        "\n⚠️⚠️⚠️⚠️⚠️ WARNING: THIS MIGRATION IS HIGH RISK.\n\
         THERE IS A HIGH PROBABILITY OF DATA LOSS. BACK UP ALL CLUSTER AND HOST DATA\n\
         USING EXTERNAL BACKUP TOOLS BEFORE CONTINUING. RUN THIS AT YOUR OWN RISK."
    )?;
    output.flush()?;
    if !interactive {
        return Ok(());
    }

    write!(output, "Type exactly \"yes\" to continue: ")?;
    output.flush()?;

    let mut response = String::new();
    if input.read_line(&mut response)? == 0 || response.trim_end_matches(['\r', '\n']) != "yes" {
        bail!("migration cancelled; confirmation must be exactly 'yes'");
    }
    Ok(())
}

fn migrate_to_nodestore(
    request: &request::MigrationRequest,
    source: &detect::Installation,
) -> Result<()> {
    let joins_existing =
        std::env::var("NODEBOOTSTRAP_JOIN_ENDPOINT").is_ok_and(|endpoint| !endpoint.is_empty());
    validate_skip_api_import(request, source, joins_existing)?;
    if source.role == detect::NodeRole::Worker {
        return migrate_worker_to_nodestore(request, source);
    }
    service::validate_disable_support(source)?;
    ensure!(
        is_root(),
        "run nodemigrate as root so it can preserve service state and write the protected export"
    );
    let migrating_node_name = node_name(source);
    let mut export = request
        .source_export
        .as_ref()
        .map(transfer::Export::load)
        .transpose()?;
    let source_api = if export.is_some() {
        None
    } else {
        Some(transfer::KubeApi::source(source)?)
    };
    let cilium_kube_proxy_replacement = source
        .cluster
        .as_ref()
        .is_some_and(|cluster| cluster.cni.as_deref() == Some("cilium"))
        && match (&source_api, &export) {
            (Some(api), _) => api.cilium_kube_proxy_replacement()?,
            (_, Some(export)) => export.cilium_kube_proxy_replacement()?,
            _ => false,
        };
    // Preserve a source kube-proxy DaemonSet as the Service router instead of
    // starting nodeproxy beside it. K3s's embedded proxy is not represented by
    // a DaemonSet, so nodeproxy replaces it after the K3s service stops.
    let source_kube_proxy_daemonset = match (&source_api, &export) {
        (Some(api), _) => api.kube_proxy_daemonset_present()?,
        (_, Some(export)) => export.kube_proxy_daemonset_present()?,
        _ => false,
    };
    let disable_nodeproxy =
        nodeproxy_should_be_disabled(cilium_kube_proxy_replacement, source_kube_proxy_daemonset)?;
    let source_nodes = if let Some(api) = &source_api {
        api.node_count()?
    } else {
        let count = export
            .as_ref()
            .map_or(0, transfer::Export::node_state_count);
        ensure!(
            count > 0,
            "protected source export contains no source node metadata"
        );
        count
    };
    let source_node_state = if let Some(api) = &source_api {
        api.node_scheduling_state(&migrating_node_name)?
    } else {
        Some(
            export
                .as_ref()
                .and_then(|export| export.node_state(&migrating_node_name))
                .with_context(|| format!("protected source export has no scheduling metadata for node {migrating_node_name}"))?
                .clone(),
        )
    };
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
    let mut bootstrap = bootstrap_command(source, disable_nodeproxy)?;
    let target_api = transfer::KubeApi::destination(request::Distribution::Nodestore)?;
    let destination_node_exists = if joins_existing {
        target_api.ready().context(
            "joining an existing nodestore cluster requires its Kubernetes API to be ready before cutover",
        )?;
        target_api.node_exists(&migrating_node_name)?
    } else {
        false
    };
    let replace_existing_node = replace_existing_node_requested()?;
    validate_destination_node_replacement(destination_node_exists, replace_existing_node)
        .with_context(|| {
            format!(
                "control-plane node {} requires explicit replacement",
                migrating_node_name
            )
        })?;
    if request.plan_only {
        if let Some(api) = &source_api {
            api.ready()?;
        }
        let cni = source
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.cni.as_deref())
            .unwrap_or("external or undetected");
        let replacement = replacement_member_id
            .map(|id| format!("; replace existing member {id} after the new node is Ready"))
            .unwrap_or_default();
        let proxy = if cilium_kube_proxy_replacement {
            "Cilium eBPF Service routing; nodeproxy disabled"
        } else if source_kube_proxy_daemonset {
            "source kube-proxy DaemonSet preserved; nodeproxy disabled"
        } else {
            "nodeproxy enabled"
        };
        println!(
            "Migration plan: {:?} -> nodestore; source nodes={source_nodes}; destination={}; source CNI={cni}; proxy={proxy}{replacement}; replace-existing-node={replace_existing_node}; cluster-api-import={}; {}source service will be disabled; uninstall-after-migrate={}",
            request.from,
            if joins_existing {
                "existing cluster"
            } else {
                "new cluster"
            },
            if request.skip_api_import {
                "skipped (state imported by an earlier control plane)"
            } else {
                "enabled"
            },
            if joins_existing {
                "install node agent on joined node; "
            } else {
                ""
            },
            request.uninstall_after_migrate
        );
        return Ok(());
    }

    if let Some(source_export) = export.take() {
        export = Some(
            source_export
                .private_copy_for_node(&migrating_node_name)
                .with_context(|| {
                    format!("creating private protected export for node {migrating_node_name}")
                })?,
        );
    }

    let proxy_owner = if cilium_kube_proxy_replacement {
        "Cilium eBPF"
    } else if source_kube_proxy_daemonset {
        "kube-proxy DaemonSet"
    } else {
        "nodeproxy"
    };
    eprintln!("nodemigrate: Service routing owner will be {proxy_owner}");

    if let Some(api) = &source_api {
        export = Some(api.export(source)?);
    }
    let mut export = export.context("source API export was not prepared")?;
    prepare_migration_pki(&mut bootstrap, source, &export)?;
    println!(
        "Protected API object export saved at {}",
        export.dir.display()
    );
    // Stop the source stack as a unit. The service layer handles separate
    // upstream CRI services and only removes kubeadm control-plane static
    // pods that must release their API/etcd ports; workload sandboxes remain
    // intact for source reactivation and reboot recovery.
    let previous_service = service::disable(source)?;
    let snapshot_result = if request.source_export.is_some() {
        export.snapshot_host_paths_for_node(&migrating_node_name)
    } else {
        export.snapshot_host_paths()
    };
    if let Err(error) = snapshot_result {
        if let Err(restore_error) = service::restore(source, previous_service) {
            bail!(
                "snapshotting local persistent volumes failed ({error:#}); restoring the source service also failed ({restore_error:#})"
            );
        }
        return Err(error)
            .context("local persistent volume snapshot failed; original service was restored");
    }
    if request.uninstall_after_migrate {
        if let Err(error) = export.snapshot_k3s_cni_paths(source) {
            if let Err(restore_error) = service::restore(source, previous_service) {
                bail!(
                    "snapshotting K3s CNI files failed ({error:#}); restoring the source service also failed ({restore_error:#})"
                );
            }
            return Err(error).context("K3s CNI snapshot failed; original service was restored");
        }
    }
    let rollback =
        |cause| rollback_forward_migration(source, previous_service.clone(), &export, cause);
    if let Err(error) = run_bootstrap(bootstrap) {
        return Err(rollback(error.context("nodestore bootstrap failed")));
    }

    if let Err(error) = wait_for_api(&target_api) {
        return Err(rollback(error.context("destination did not become ready")));
    }
    if !request.skip_api_import {
        if let Err(error) = target_api.import(&export) {
            return Err(rollback(error.context(
                "destination bootstrap succeeded but Kubernetes object import failed",
            )));
        }
    }
    if !joins_existing
        && source
            .cluster
            .as_ref()
            .is_some_and(|cluster| cluster.cni.as_deref() == Some("cilium"))
    {
        if let Err(error) = target_api.reset_cilium_agent_state(&migrating_node_name) {
            return Err(rollback(error.context("rebuilding destination Cilium host datapath state")));
        }
    }
    let replacement_state = if destination_node_exists {
        match remove_replaced_node(&target_api, &migrating_node_name, replace_existing_node) {
            Ok(state) => state.or(source_node_state),
            Err(error) => {
                return Err(rollback(error.context(format!(
                    "preparing destination control-plane node; export retained at {}",
                    export.dir.display()
                ))));
            }
        }
    } else {
        source_node_state
    };
    if joins_existing {
        let cilium_kube_proxy_replacement = if source
            .cluster
            .as_ref()
            .is_some_and(|cluster| cluster.cni.as_deref() == Some("cilium"))
        {
            match target_api.cilium_kube_proxy_replacement() {
                Ok(value) => value,
                Err(error) => {
                    return Err(rollback(
                        error.context("checking destination Cilium proxy ownership"),
                    ));
                }
            }
        } else {
            false
        };
        let kube_proxy_present = match target_api.kube_proxy_daemonset_present() {
            Ok(value) => value,
            Err(error) => {
                return Err(rollback(
                    error.context("checking destination kube-proxy ownership"),
                ));
            }
        };
        let disable_nodeproxy =
            match nodeproxy_should_be_disabled(cilium_kube_proxy_replacement, kube_proxy_present) {
                Ok(value) => value,
                Err(error) => {
                    return Err(rollback(
                        error.context("selecting destination Service proxy"),
                    ));
                }
            };
        let worker =
            match replacement_worker_command(source, &migrating_node_name, disable_nodeproxy) {
                Ok(worker) => worker,
                Err(error) => {
                    return Err(rollback(error.context("preparing replacement node agent")));
                }
            };
        if let Err(error) = run_bootstrap(worker) {
            return Err(rollback(error.context(format!(
                "installing the node agent on the joined replacement node; protected export is at {}",
                export.dir.display()
            ))));
        }
    }
    if let Err(error) = wait_for_node(&target_api, &migrating_node_name) {
        return Err(rollback(
            error.context("replacement control-plane node did not become Ready"),
        ));
    }
    // A joining node needs to exist in the destination API before Cilium's
    // DaemonSet can place its agent here. Resetting Cilium host state before
    // the replacement node agent was installed waits forever for an agent
    // that has no Node to match; do the reset now and recheck readiness.
    if joins_existing
        && source
            .cluster
            .as_ref()
            .is_some_and(|cluster| cluster.cni.as_deref() == Some("cilium"))
    {
        if let Err(error) = target_api.reset_cilium_agent_state(&migrating_node_name) {
            return Err(rollback(
                error.context("rebuilding destination Cilium host datapath state"),
            ));
        }
        if let Err(error) = wait_for_node(&target_api, &migrating_node_name) {
            return Err(rollback(error.context(
                "Cilium reset left replacement control-plane node unready",
            )));
        }
    }
    if let Err(error) = restore_node_scheduling_state(
        &target_api,
        &migrating_node_name,
        replacement_state.as_ref(),
    ) {
        return Err(rollback(error.context(
            "restoring destination control-plane node labels and scheduling state",
        )));
    }
    let repaired_owner_references =
        match target_api.restore_node_owner_references(&export, &migrating_node_name) {
            Ok(count) => count,
            Err(error) => {
                return Err(rollback(
                    error.context("repairing destination Node owner references"),
                ));
            }
        };
    if repaired_owner_references > 0 {
        eprintln!(
            "nodemigrate: restored {repaired_owner_references} owner reference(s) to replacement Node {migrating_node_name}"
        );
    }
    if joins_existing {
        let membership_command = if let Some(old_member_id) = replacement_member_id {
            match replace_member_command(&old_member_id.to_string()) {
                Ok(command) => command,
                Err(error) => {
                    return Err(rollback(
                        error.context("preparing control-plane member replacement"),
                    ));
                }
            }
        } else {
            match promote_member_command() {
                Ok(command) => command,
                Err(error) => {
                    return Err(rollback(error.context("preparing control-plane promotion")));
                }
            }
        };
        if let Err(error) = run_bootstrap(membership_command) {
            return Err(rollback(error.context(format!(
                "promoting the replacement control-plane member; recovery export is at {}",
                export.dir.display()
            ))));
        }
    }
    if request.uninstall_after_migrate {
        if let Err(uninstall_error) = service::uninstall_source(source) {
            let host_path_error = export.restore_host_paths().err();
            let cni_path_error = export.restore_k3s_cni_paths().err();
            bail!(
                "source uninstall failed ({uninstall_error:#}); persistent-path restore error={host_path_error:#?}; CNI-path restore error={cni_path_error:#?}; recovery export retained at {}",
                export.dir.display()
            );
        }
        let host_path_error = export.restore_host_paths().err();
        let cni_path_error = export.restore_k3s_cni_paths().err();
        ensure!(
            host_path_error.is_none() && cni_path_error.is_none(),
            "post-uninstall restore failed: persistent-path error={host_path_error:#?}; CNI-path error={cni_path_error:#?}; recovery export retained at {}",
            export.dir.display()
        );
        let mut reconcile = bootstrap_command(source, disable_nodeproxy)?;
        prepare_migration_pki(&mut reconcile, source, &export)?;
        run_bootstrap(reconcile)
            .context("reconciling nodestore after source uninstall")?;
        wait_for_api(&target_api)
            .context("destination failed API readiness after K3s uninstall cleanup")?;
        wait_for_node(&target_api, &migrating_node_name)
            .context("replacement node failed readiness after K3s uninstall cleanup")?;
    }
    println!(
        "Migration completed and the destination API passed readiness checks. Export retained at {}",
        export.dir.display()
    );
    Ok(())
}

fn rollback_forward_migration(
    source: &detect::Installation,
    previous_service: service::PreviousServiceState,
    export: &transfer::Export,
    cause: anyhow::Error,
) -> anyhow::Error {
    let recovery = export.dir.display();
    if let Err(stop_error) = service::stop_nodestore_for_rollback(source) {
        return cause.context(format!(
            "stopping the partial nodestore stack failed ({stop_error:#}); source remains disabled; protected export retained at {recovery}"
        ));
    }
    let host_path_error = export.restore_host_paths().err();
    let cni_path_error = export.restore_k3s_cni_paths().err();
    if let Err(restore_error) = service::restore(source, previous_service) {
        return cause.context(format!(
            "source service restoration failed ({restore_error:#}); source remains disabled; partial nodestore services were stopped; PV restore error={host_path_error:#?}; CNI restore error={cni_path_error:#?}; protected export retained at {recovery}"
        ));
    }
    if host_path_error.is_some() || cni_path_error.is_some() {
        return cause.context(format!(
            "partial nodestore services were stopped and the source service was restored, but PV restore error={host_path_error:#?}; CNI restore error={cni_path_error:#?}; protected export retained at {recovery}"
        ));
    }
    cause.context(format!(
        "source service was restored after rollback; partial nodestore services were stopped; source PV and CNI data were restored; protected export retained at {recovery}"
    ))
}

fn rollback_forward_worker_migration(
    source: &detect::Installation,
    previous_service: service::PreviousServiceState,
    cause: anyhow::Error,
) -> anyhow::Error {
    if let Err(stop_error) = service::stop_nodestore_worker_for_rollback(source) {
        return cause.context(format!(
            "stopping the partial nodestore worker failed ({stop_error:#}); source remains disabled"
        ));
    }
    if let Err(restore_error) = service::restore(source, previous_service) {
        return cause.context(format!(
            "source worker restoration failed ({restore_error:#}); partial nodestore worker was stopped"
        ));
    }
    cause
        .context("partial nodestore worker was stopped and the original source worker was restored")
}

fn migrate_worker_to_nodestore(
    request: &request::MigrationRequest,
    source: &detect::Installation,
) -> Result<()> {
    ensure!(
        std::env::var_os("NODEBOOTSTRAP_JOIN_ENDPOINT").is_some(),
        "worker migration requires NODEBOOTSTRAP_JOIN_ENDPOINT for an existing nodestore cluster"
    );
    service::validate_disable_support(source)?;
    ensure!(
        is_root(),
        "run nodemigrate as root so it can preserve service state and replace the worker"
    );
    let name = node_name(source);
    let target_api = transfer::KubeApi::destination(request::Distribution::Nodestore)?;
    target_api.ready()?;
    let mut source_export = request
        .source_export
        .as_ref()
        .map(transfer::Export::load)
        .transpose()?;
    let local_node_state = if let Some(export) = source_export.as_ref() {
        Some(
            export
                .node_state(&name)
                .with_context(|| {
                    format!("protected source export has no scheduling metadata for worker {name}")
                })?
                .clone(),
        )
    } else {
        local_node_scheduling_state(transfer::KubeApi::source(source), &target_api, &name)
    };
    let local_node_labels = local_node_state.as_ref().map(|state| &state.labels);
    let existing_node = target_api.node_exists(&name)?;
    let replace_existing = replace_existing_node_requested()?;
    validate_destination_node_replacement(existing_node, replace_existing)
        .with_context(|| format!("destination already has node {name}"))?;
    let cilium_kube_proxy_replacement = source
        .cluster
        .as_ref()
        .is_some_and(|cluster| cluster.cni.as_deref() == Some("cilium"))
        && target_api.cilium_kube_proxy_replacement()?;
    let disable_nodeproxy = nodeproxy_should_be_disabled(
        cilium_kube_proxy_replacement,
        target_api.kube_proxy_daemonset_present()?,
    )?;
    let worker = replacement_worker_command(source, &name, disable_nodeproxy)?;
    if request.plan_only {
        let cni = source
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.cni.as_deref())
            .unwrap_or("external or undetected");
        let service_proxy = if cilium_kube_proxy_replacement {
            "Cilium eBPF; nodeproxy disabled"
        } else if disable_nodeproxy {
            "kube-proxy DaemonSet; nodeproxy disabled"
        } else {
            "nodeproxy"
        };
        println!(
            "Migration plan: {:?} worker -> existing nodestore cluster; node={name}; source CNI={cni}; target Service proxy={service_proxy}; replace-existing-node={existing_node}; source service '{}' will be disabled; cluster API objects are managed by the control-plane migration",
            request.from, source.service_name
        );
        return Ok(());
    }

    if let Some(export) = source_export.take() {
        source_export = Some(
            export
                .private_copy_for_node(&name)
                .with_context(|| format!("creating private protected export for worker {name}"))?,
        );
    }
    if let Some(export) = source_export.as_ref() {
        eprintln!(
            "nodemigrate: node-private protected source export copy at {}",
            export.dir.display()
        );
    }

    let previous_service = service::disable(source)?;
    let mut host_path_snapshot = match target_api.snapshot_host_paths(local_node_labels) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            if let Err(restore_error) = service::restore(source, previous_service) {
                bail!(
                    "snapshotting worker local volumes failed ({error:#}) and restoring the source service failed ({restore_error:#})"
                );
            }
            return Err(error)
                .context("worker local-volume snapshot failed; source service was restored");
        }
    };
    if request.uninstall_after_migrate {
        if let Err(error) = host_path_snapshot.snapshot_k3s_cni_paths(source) {
            if let Err(restore_error) = service::restore(source, previous_service) {
                bail!(
                    "snapshotting worker K3s CNI files failed ({error:#}) and restoring source service failed ({restore_error:#})"
                );
            }
            return Err(error)
                .context("worker K3s CNI snapshot failed; source service was restored");
        }
    }
    println!(
        "Worker local-volume recovery snapshot saved at {}",
        host_path_snapshot.recovery_directory().display()
    );
    let replacement_state = if existing_node {
        match remove_replaced_node(&target_api, &name, replace_existing) {
            Ok(state) => state,
            Err(error) => {
            if let Err(restore_error) = service::restore(source, previous_service) {
                bail!("removing stale destination node {name} failed ({error:#}) and restoring source service failed ({restore_error:#})");
            }
                return Err(error).context("removing the explicitly selected stale destination worker");
            }
        }
    } else {
        None
    }
    .or(local_node_state);
    if let Err(error) = run_bootstrap(worker) {
        return Err(rollback_forward_worker_migration(
            source,
            previous_service.clone(),
            error.context("installing the worker into the joined nodestore cluster"),
        ));
    }
    if let Err(error) = wait_for_node(&target_api, &name) {
        return Err(rollback_forward_worker_migration(
            source,
            previous_service.clone(),
            error.context(format!("replacement worker {name} did not become Ready")),
        ));
    }
    if source
        .cluster
        .as_ref()
        .is_some_and(|cluster| cluster.cni.as_deref() == Some("cilium"))
    {
        if let Err(error) = target_api.reset_cilium_agent_state(&name) {
            return Err(rollback_forward_worker_migration(
                source,
                previous_service.clone(),
                error.context("rebuilding destination worker Cilium host datapath state"),
            ));
        }
        if let Err(error) = wait_for_node(&target_api, &name) {
            return Err(rollback_forward_worker_migration(
                source,
                previous_service.clone(),
                error.context(format!("Cilium reset left replacement worker {name} unready")),
            ));
        }
    }
    if let Err(error) =
        restore_node_scheduling_state(&target_api, &name, replacement_state.as_ref())
    {
        return Err(rollback_forward_worker_migration(
            source,
            previous_service.clone(),
            error.context("restoring replacement worker labels and scheduling state"),
        ));
    }
    if let Some(export) = source_export.as_ref() {
        let repaired_owner_references =
            match target_api.restore_node_owner_references(export, &name) {
                Ok(count) => count,
                Err(error) => {
                    return Err(rollback_forward_worker_migration(
                        source,
                        previous_service.clone(),
                        error.context("restoring worker Node owner references"),
                    ));
                }
            };
        if repaired_owner_references > 0 {
            eprintln!(
                "nodemigrate: restored {repaired_owner_references} owner reference(s) to replacement Node {name}"
            );
        }
    }
    if request.uninstall_after_migrate {
        if let Err(uninstall_error) = service::uninstall_source(source) {
            let host_path_error = host_path_snapshot.restore().err();
            let cni_path_error = host_path_snapshot.restore_k3s_cni_paths().err();
            bail!(
                "source uninstall failed ({uninstall_error:#}); worker volume restore error={host_path_error:#?}; CNI-path restore error={cni_path_error:#?}; recovery snapshot retained at {}",
                host_path_snapshot.recovery_directory().display()
            );
        }
        let host_path_error = host_path_snapshot.restore().err();
        let cni_path_error = host_path_snapshot.restore_k3s_cni_paths().err();
        ensure!(
            host_path_error.is_none() && cni_path_error.is_none(),
            "post-uninstall worker restore failed: persistent-path error={host_path_error:#?}; CNI-path error={cni_path_error:#?}; recovery snapshot retained at {}",
            host_path_snapshot.recovery_directory().display()
        );
    }
    println!(
        "Worker {name} joined the nodestore cluster and is Ready. Cluster-wide API resources were not re-imported from this worker; local-volume recovery snapshot retained at {}",
        host_path_snapshot.recovery_directory().display()
    );
    Ok(())
}

fn validate_destination_node_replacement(existing_node: bool, replace: bool) -> Result<()> {
    ensure!(
        !existing_node || replace,
        "set NODEMIGRATE_REPLACE_NODE=true to replace the same-name destination node and require fresh registration"
    );
    Ok(())
}

fn remove_replaced_node(
    target_api: &transfer::KubeApi,
    node_name: &str,
    replace: bool,
) -> Result<Option<transfer::NodeSchedulingState>> {
    let state = target_api.node_scheduling_state(node_name)?;
    if state.is_some() {
        validate_destination_node_replacement(true, replace)
            .with_context(|| format!("destination already has node {node_name}"))?;
        let uid = state
            .as_ref()
            .and_then(|state| state.uid.as_deref())
            .context("destination Node has no UID; refusing an unconditional replacement delete")?;
        target_api.delete_node(node_name, uid)?;
    }
    Ok(state)
}

fn restore_node_scheduling_state(
    target_api: &transfer::KubeApi,
    node_name: &str,
    state: Option<&transfer::NodeSchedulingState>,
) -> Result<()> {
    if let Some(state) = state {
        target_api.restore_node_scheduling_state(node_name, state)?;
    }
    Ok(())
}

fn reverse_node_replacement_state(
    destination_node_existed_before_activation: bool,
    state_observed_after_activation: Option<transfer::NodeSchedulingState>,
    nodestore_node_state: Option<transfer::NodeSchedulingState>,
) -> Option<transfer::NodeSchedulingState> {
    if destination_node_existed_before_activation {
        state_observed_after_activation.or(nodestore_node_state)
    } else {
        // A same-name Node observed after activation may be the freshly
        // registered replacement. Its defaults must not replace the state
        // captured from nodestore before cutover.
        nodestore_node_state
    }
}

fn validate_skip_api_import(
    request: &request::MigrationRequest,
    source: &detect::Installation,
    joins_existing: bool,
) -> Result<()> {
    ensure!(
        !request.skip_api_import
            || (matches!(
                source.role,
                detect::NodeRole::ControlPlane | detect::NodeRole::Worker
            ) && joins_existing),
        "skip-api-import=true requires a source node joining an existing nodestore cluster; the first control plane must import cluster state"
    );
    Ok(())
}

fn replace_existing_node_requested() -> Result<bool> {
    match std::env::var("NODEMIGRATE_REPLACE_NODE") {
        Ok(value) if value == "true" => Ok(true),
        Ok(value) if value == "false" => Ok(false),
        Ok(_) => bail!("NODEMIGRATE_REPLACE_NODE must be true or false"),
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(error) => Err(error).context("reading NODEMIGRATE_REPLACE_NODE"),
    }
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
    if request.import_export.is_some() {
        return resume_export_import(request, source, target);
    }
    validate_reverse_control_plane_options(request, source, target)?;
    if source.role == detect::NodeRole::Worker {
        return migrate_worker_from_nodestore(request, source, target);
    }
    service::validate_disable_support(source)?;
    ensure!(
        is_root(),
        "run nodemigrate as root to control both service stacks"
    );
    let returning_node_name = node_name(target);
    let target_api = transfer::KubeApi::destination(request.to)?;
    let mut export = request
        .source_export
        .as_ref()
        .map(transfer::Export::load)
        .transpose()
        .with_context(|| {
            format!(
                "loading protected staged-migration export {}",
                request
                    .source_export
                    .as_deref()
                    .unwrap_or_else(|| std::path::Path::new("<missing>"))
                    .display()
            )
        })?;
    let source_api = if request.skip_api_export || export.is_some() {
        None
    } else {
        Some(transfer::KubeApi::source(source)?)
    };
    if let Some(source_api) = &source_api {
        source_api.ready()?;
    }
    if request.skip_api_export && !request.stage_target {
        target_api.ready().context(
            "skip-api-export requires the retained destination cluster to be Ready through NODEMIGRATE_DESTINATION_KUBECONFIG",
        )?;
    }
    let source_node_state = source_api
        .as_ref()
        .map(|api| api.node_scheduling_state(&returning_node_name))
        .transpose()?
        .flatten()
        .or_else(|| {
            export
                .as_ref()
                .and_then(|export| export.node_state(&returning_node_name).cloned())
        });
    let destination_node_exists = if request.skip_api_export && !request.stage_target {
        target_api.node_exists(&returning_node_name)?
    } else {
        false
    };
    let replace_existing_node = replace_existing_node_requested()?;
    let destination_may_retain_node =
        destination_node_exists || (!request.skip_api_export && source_node_state.is_some());
    validate_destination_node_replacement(destination_may_retain_node, replace_existing_node)
        .with_context(|| {
            format!(
                "returning control-plane node {returning_node_name} requires explicit fresh-registration confirmation"
            )
        })?;
    if request.plan_only {
        let cni = target
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.cni.as_deref())
            .unwrap_or("external or undetected");
        println!(
            "Migration plan: nodestore -> {:?}; retained target service '{}' will be enabled and started; target CNI={cni}; replace-existing-node={replace_existing_node}; source-api-export={}; staged-source-export={}; stage-target={}; uninstall-after-migrate={}",
            request.to,
            target.service_name,
            if request.skip_api_export {
                "skipped (destination already has cluster state)"
            } else {
                "enabled"
            },
            request
                .source_export
                .as_ref()
                .map_or("none", |_| "reused for offline node/PV recovery"),
            request.stage_target,
            request.uninstall_after_migrate
        );
        return Ok(());
    }
    if let Some(source_export) = export.take() {
        export = Some(
            source_export
                .private_copy_for_node(&returning_node_name)
                .with_context(|| {
                    format!("creating private staged export for node {returning_node_name}")
                })?,
        );
    }
    if export.is_none() {
        export = source_api
            .as_ref()
            .map(|source_api| source_api.export(source))
            .transpose()?;
    }
    let needs_host_path_snapshot = request.skip_api_export && !request.stage_target;
    let local_node_labels = if needs_host_path_snapshot {
        let name = node_name(target);
        target_api.node_labels(&name).ok()
    } else {
        None
    };
    if let Some(export) = &export {
        println!(
            "Protected API object export saved at {}",
            export.dir.display()
        );
    }
    let mut host_path_snapshot = None;
    let mut recovery_location = export
        .as_ref()
        .map(|export| export.dir.display().to_string())
        .unwrap_or_else(|| "local-volume snapshot pending".to_string());
    eprintln!("nodemigrate: stopping the nodestore service stack for return migration");
    let previous_service = disable_nodestore_stack(source).with_context(|| {
        format!("disabling nodestore failed; recovery data is at {recovery_location}")
    })?;
    eprintln!("nodemigrate: nodestore service stack is stopped");
    if needs_host_path_snapshot {
        match target_api.snapshot_host_paths(local_node_labels.as_ref()) {
            Ok(snapshot) => host_path_snapshot = Some(snapshot),
            Err(error) => {
                if let Err(restore_error) = service::restore(source, previous_service) {
                    bail!(
                        "snapshotting local persistent volumes failed ({error:#}); restoring nodestore also failed ({restore_error:#}); recovery data is at {recovery_location}"
                    );
                }
                return Err(error).context(format!(
                    "local persistent-volume snapshot failed; nodestore was restored; recovery data is at {recovery_location}"
                ));
            }
        }
    }
    if let Some(snapshot) = &host_path_snapshot {
        recovery_location = snapshot.recovery_directory().display().to_string();
        println!(
            "Local-volume recovery snapshot saved at {}",
            snapshot.recovery_directory().display()
        );
    }
    if let Some(export) = &mut export {
        eprintln!("nodemigrate: snapshotting local persistent-volume payloads");
        if let Err(error) = export.snapshot_host_paths_for_node(&returning_node_name) {
            if let Err(restore_error) = service::restore(source, previous_service) {
                bail!(
                    "snapshotting local persistent volumes failed ({error:#}); restoring nodestore also failed ({restore_error:#}); recovery data is at {recovery_location}"
                );
            }
            return Err(error).context(format!(
                "local persistent volume snapshot failed; nodestore was restored; recovery data is at {recovery_location}"
            ));
        }
        eprintln!("nodemigrate: local persistent-volume snapshot completed");
    }
    eprintln!(
        "nodemigrate: starting retained {} service",
        target.service_name
    );
    if let Err(error) = service::activate(target) {
        return Err(rollback_reverse_migration(
            source,
            target,
            previous_service,
            export.as_ref(),
            host_path_snapshot.as_ref(),
            error.context("starting the retained target failed"),
            &recovery_location,
        ));
    }
    eprintln!(
        "nodemigrate: retained {} start command returned",
        target.service_name
    );
    if request.stage_target {
        let recovery = export
            .as_ref()
            .map(|export| export.dir.display().to_string())
            .unwrap_or_else(|| "not created".to_string());
        println!(
            "Retained control plane staged: source nodestore services are disabled and '{}' is running. The destination API may remain unavailable until another retained control plane is started. API export retained at {recovery}",
            target.service_name
        );
        return Ok(());
    }
    eprintln!("nodemigrate: waiting for retained destination API readiness");
    if let Err(error) = wait_for_api(&target_api) {
        return Err(rollback_reverse_migration(
            source,
            target,
            previous_service,
            export.as_ref(),
            host_path_snapshot.as_ref(),
            error.context("retained destination API did not become ready"),
            &recovery_location,
        ));
    }
    eprintln!("nodemigrate: retained destination API is ready");
    if target
        .cluster
        .as_ref()
        .is_some_and(|cluster| cluster.cni.as_deref() == Some("cilium"))
    {
        eprintln!(
            "nodemigrate: rebuilding retained Cilium host datapath state for node {returning_node_name}"
        );
        if let Err(error) = target_api.reset_cilium_agent_state(&returning_node_name) {
            return Err(rollback_reverse_migration(
                source,
                target,
                previous_service,
                export.as_ref(),
                host_path_snapshot.as_ref(),
                error.context("rebuilding retained Cilium host datapath state"),
                &recovery_location,
            ));
        }
    }
    if let Some(export) = &export {
        eprintln!("nodemigrate: importing protected Kubernetes API export");
        if let Err(error) = target_api.import(export) {
            return Err(rollback_reverse_migration(
                source,
                target,
                previous_service,
                Some(export),
                host_path_snapshot.as_ref(),
                error.context("destination started but API import failed"),
                &recovery_location,
            ));
        }
        eprintln!("nodemigrate: protected Kubernetes API import completed");
    }
    let observed_replacement_state = if destination_node_exists || replace_existing_node {
        match remove_replaced_node(&target_api, &returning_node_name, replace_existing_node) {
            Ok(state) => state,
            Err(error) => {
                return Err(rollback_reverse_migration(
                    source,
                    target,
                    previous_service,
                    export.as_ref(),
                    host_path_snapshot.as_ref(),
                    error.context("preparing retained control-plane node"),
                    &recovery_location,
                ));
            }
        }
    } else {
        None
    };
    let node_was_replaced = observed_replacement_state.is_some();
    let previous_replacement_uid = observed_replacement_state
        .as_ref()
        .and_then(|state| state.uid.clone());
    let replacement_state = reverse_node_replacement_state(
        destination_node_exists,
        observed_replacement_state,
        source_node_state,
    );
    if node_was_replaced {
        eprintln!(
            "nodemigrate: restarting retained {} service to register the replacement node",
            target.service_name
        );
        if let Err(error) = service::restart(target) {
            return Err(rollback_reverse_migration(
                source,
                target,
                previous_service,
                export.as_ref(),
                host_path_snapshot.as_ref(),
                error.context("restarting the retained service after Node replacement"),
                &recovery_location,
            ));
        }
        eprintln!(
            "nodemigrate: retained {} restart command returned",
            target.service_name
        );
    }
    eprintln!("nodemigrate: waiting for returned node {returning_node_name} to become Ready");
    let returned_node_readiness = match previous_replacement_uid.as_deref() {
        Some(previous_uid) => {
            wait_for_replacement_node(&target_api, &returning_node_name, previous_uid)
        }
        None => wait_for_node(&target_api, &returning_node_name),
    };
    if let Err(error) = returned_node_readiness {
        return Err(rollback_reverse_migration(
            source,
            target,
            previous_service,
            export.as_ref(),
            host_path_snapshot.as_ref(),
            error.context("retained destination node did not become Ready"),
            &recovery_location,
        ));
    }
    eprintln!("nodemigrate: returned node {returning_node_name} is Ready");
    if let Err(error) = restore_node_scheduling_state(
        &target_api,
        &returning_node_name,
        replacement_state.as_ref(),
    ) {
        return Err(rollback_reverse_migration(
            source,
            target,
            previous_service,
            export.as_ref(),
            host_path_snapshot.as_ref(),
            error.context("restoring retained control-plane node labels and scheduling state"),
            &recovery_location,
        ));
    }
    if let Some(export) = export.as_ref() {
        let repaired_owner_references =
            match target_api.restore_node_owner_references(export, &returning_node_name) {
                Ok(count) => count,
                Err(error) => {
                    return Err(rollback_reverse_migration(
                        source,
                        target,
                        previous_service,
                        Some(export),
                        host_path_snapshot.as_ref(),
                        error.context("restoring retained control-plane Node owner references"),
                        &recovery_location,
                    ));
                }
            };
        if repaired_owner_references > 0 {
            eprintln!(
                "nodemigrate: restored {repaired_owner_references} owner reference(s) to replacement Node {returning_node_name}"
            );
        }
    }
    if request.uninstall_after_migrate {
        service::uninstall_source(source).with_context(|| {
            format!("source uninstall failed; recovery data is at {recovery_location}")
        })?;
        if let Some(export) = &export {
            export.restore_host_paths().with_context(|| {
                format!("restoring local persistent volumes failed; recovery data is at {recovery_location}")
            })?;
        }
        if let Some(snapshot) = &host_path_snapshot {
            snapshot.restore().with_context(|| {
                format!("restoring local persistent volumes failed; recovery data is at {recovery_location}")
            })?;
        }
    }
    let recovery = export
        .as_ref()
        .map(|export| export.dir.display().to_string())
        .or_else(|| {
            host_path_snapshot
                .as_ref()
                .map(|snapshot| snapshot.recovery_directory().display().to_string())
        })
        .unwrap_or_else(|| {
            "no new export (destination already held the imported state)".to_string()
        });
    println!(
        "Migration to {:?} completed. Recovery data: {recovery}",
        request.to
    );
    Ok(())
}

fn rollback_reverse_migration(
    source: &detect::Installation,
    target: &detect::Installation,
    previous_service: service::PreviousServiceState,
    export: Option<&transfer::Export>,
    host_path_snapshot: Option<&transfer::HostPathSnapshot>,
    cause: Error,
    recovery_location: &str,
) -> Error {
    rollback_reverse_migration_with(
        cause,
        recovery_location,
        || service::stop_reverse_migration_target(target),
        || {
            let mut failures = Vec::new();
            if let Some(export) = export {
                if let Err(error) = export.restore_host_paths() {
                    failures.push(format!("export host-path restore failed ({error:#})"));
                }
            }
            if let Some(snapshot) = host_path_snapshot {
                if let Err(error) = snapshot.restore() {
                    failures.push(format!("local-volume snapshot restore failed ({error:#})"));
                }
            }
            if failures.is_empty() {
                Ok(())
            } else {
                bail!("{}", failures.join("; "))
            }
        },
        || service::restore(source, previous_service),
    )
}

fn rollback_reverse_migration_with(
    cause: Error,
    recovery_location: &str,
    stop_target: impl FnOnce() -> Result<()>,
    restore_payload: impl FnOnce() -> Result<()>,
    restore_source: impl FnOnce() -> Result<()>,
) -> Error {
    if let Err(error) = stop_target() {
        return cause.context(format!(
            "rollback could not stop the retained target ({error:#}); nodestore remains stopped and recovery data is at {recovery_location}"
        ));
    }

    let payload_result = restore_payload();
    let source_result = restore_source();
    match (payload_result, source_result) {
        (Ok(()), Ok(())) => cause.context(format!(
            "retained target was stopped and nodestore was restored; recovery data is at {recovery_location}"
        )),
        (Err(payload_error), Ok(())) => cause.context(format!(
            "retained target was stopped and nodestore was restored, but local data restoration failed ({payload_error:#}); recovery data is at {recovery_location}"
        )),
        (Ok(()), Err(source_error)) => cause.context(format!(
            "retained target was stopped, but nodestore restoration failed ({source_error:#}); recovery data is at {recovery_location}"
        )),
        (Err(payload_error), Err(source_error)) => cause.context(format!(
            "retained target was stopped, but local data restoration failed ({payload_error:#}) and nodestore restoration failed ({source_error:#}); recovery data is at {recovery_location}"
        )),
    }
}

fn resume_export_import(
    request: &request::MigrationRequest,
    source: &detect::Installation,
    target: &detect::Installation,
) -> Result<()> {
    ensure!(
        source.role == detect::NodeRole::ControlPlane
            && target.role == detect::NodeRole::ControlPlane,
        "import-export requires nodestore and retained destination control-plane nodes"
    );
    let directory = request
        .import_export
        .as_ref()
        .context("import-export directory is required")?;
    let export = transfer::Export::load(directory)
        .with_context(|| format!("loading protected migration export {}", directory.display()))?;
    if request.plan_only {
        println!(
            "Migration plan: resume importing {} Kubernetes objects from {} into retained {:?} control plane '{}'",
            export.object_count(),
            export.dir.display(),
            request.to,
            target.service_name
        );
        return Ok(());
    }
    ensure!(
        is_root(),
        "run nodemigrate as root to import migration state into the retained control plane"
    );
    let target_api = transfer::KubeApi::destination(request.to)?;
    wait_for_api(&target_api).context(format!(
        "retained destination API is not ready; protected export remains at {}",
        export.dir.display()
    ))?;
    let control_plane_nodes = export.control_plane_node_names();
    ensure!(
        !control_plane_nodes.is_empty(),
        "protected export has no control-plane Node scheduling metadata; export retained at {}",
        export.dir.display()
    );
    let replace_existing_node = replace_existing_node_requested()?;
    for name in &control_plane_nodes {
        let scheduling_state = export
            .node_state(name)
            .with_context(|| {
                format!("protected export has no scheduling metadata for control-plane Node {name}")
            })?
            .clone();
        if target_api.node_exists(name)? {
            remove_replaced_node(&target_api, name, replace_existing_node)
                .with_context(|| format!("re-registering staged control-plane Node {name}"))?;
        }
        wait_for_node(&target_api, name)
            .with_context(|| format!("waiting for staged control-plane Node {name}"))?;
        restore_node_scheduling_state(&target_api, name, Some(&scheduling_state)).with_context(
            || format!("restoring staged control-plane Node {name} scheduling state"),
        )?;
    }
    target_api.import(&export).with_context(|| {
        format!(
            "resuming migration API import; protected export remains at {}",
            export.dir.display()
        )
    })?;
    for name in &control_plane_nodes {
        let repaired = target_api.restore_node_owner_references(&export, name)?;
        if repaired > 0 {
            eprintln!(
                "nodemigrate: restored {repaired} owner reference(s) to staged control-plane Node {name}"
            );
        }
    }
    println!(
        "Imported {} Kubernetes objects into retained {:?} cluster. Recovery export: {}",
        export.object_count(),
        request.to,
        export.dir.display()
    );
    Ok(())
}

fn validate_reverse_control_plane_options(
    request: &request::MigrationRequest,
    source: &detect::Installation,
    target: &detect::Installation,
) -> Result<()> {
    let requested = request.stage_target || request.skip_api_export;
    ensure!(
        !requested
            || (source.role == detect::NodeRole::ControlPlane
                && target.role == detect::NodeRole::ControlPlane),
        "stage-target and skip-api-export require both source and retained target to be control planes"
    );
    ensure!(
        !request.skip_api_export || !request.stage_target,
        "skip-api-export cannot be combined with stage-target"
    );
    Ok(())
}

fn local_node_scheduling_state(
    source_api: Result<transfer::KubeApi>,
    fallback_api: &transfer::KubeApi,
    node_name: &str,
) -> Option<transfer::NodeSchedulingState> {
    match source_api.and_then(|api| api.node_scheduling_state(node_name)) {
        Ok(Some(state)) => Some(state),
        source_result => match fallback_api.node_scheduling_state(node_name) {
            Ok(Some(state)) => Some(state),
            Err(fallback_error) => {
                tracing::warn!(
                    node = node_name,
                    source_result = ?source_result,
                    fallback_error = %fallback_error,
                    "could not read local Node scheduling state from source or destination API"
                );
                None
            }
            Ok(None) => {
                tracing::warn!(
                    node = node_name,
                    source_result = ?source_result,
                    "local Node scheduling state is absent from source and destination APIs"
                );
                None
            }
        },
    }
}

fn migrate_worker_from_nodestore(
    request: &request::MigrationRequest,
    source: &detect::Installation,
    target: &detect::Installation,
) -> Result<()> {
    ensure!(
        target.role == detect::NodeRole::Worker,
        "a nodestore worker must return to a retained worker installation"
    );
    service::validate_disable_support(source)?;
    service::validate_disable_support(target)?;
    ensure!(
        is_root(),
        "run nodemigrate as root to replace both worker services"
    );
    let name = node_name(target);
    let target_api = transfer::KubeApi::destination(request.to)?;
    target_api.ready()?;
    let local_node_state =
        local_node_scheduling_state(transfer::KubeApi::source(source), &target_api, &name);
    let local_node_labels = local_node_state.as_ref().map(|state| &state.labels);
    let existing_node = target_api.node_exists(&name)?;
    let replace_existing_node = replace_existing_node_requested()?;
    validate_destination_node_replacement(existing_node, replace_existing_node)
        .with_context(|| format!("destination already has node {name}"))?;
    if request.plan_only {
        println!(
            "Migration plan: nodestore worker -> {:?} worker; node={name}; retained target service '{}' will be enabled and started; cluster API objects are managed by the control-plane migration",
            request.to, target.service_name
        );
        return Ok(());
    }

    let previous_service = disable_nodestore_stack(source)
        .context("stopping nodestore worker services before returning to the retained cluster")?;
    let host_path_snapshot = match target_api.snapshot_host_paths(local_node_labels) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            if let Err(restore_error) = service::restore(source, previous_service) {
                bail!(
                    "snapshotting nodestore worker volumes failed ({error:#}) and restoring nodestore failed ({restore_error:#})"
                );
            }
            return Err(error)
                .context("nodestore worker volume snapshot failed; source services were restored");
        }
    };
    println!(
        "Worker local-volume recovery snapshot saved at {}",
        host_path_snapshot.recovery_directory().display()
    );
    let replacement_state = if existing_node {
        match remove_replaced_node(
            &target_api,
            &name,
            replace_existing_node,
        ) {
            Ok(state) => state,
            Err(error) => {
            if let Err(restore_error) = service::restore(source, previous_service) {
                bail!("removing stale destination node {name} failed ({error:#}) and restoring nodestore worker services failed ({restore_error:#})");
            }
                return Err(error).context("removing the explicitly selected stale destination worker");
            }
        }
    } else {
        None
    }
    .or(local_node_state);
    let recovery_location = host_path_snapshot
        .recovery_directory()
        .display()
        .to_string();
    if let Err(error) = service::activate(target) {
        return Err(rollback_reverse_migration(
            source,
            target,
            previous_service,
            None,
            Some(&host_path_snapshot),
            error.context("starting the retained worker service"),
            &recovery_location,
        ));
    }
    if let Err(error) = wait_for_node(&target_api, &name) {
        return Err(rollback_reverse_migration(
            source,
            target,
            previous_service,
            None,
            Some(&host_path_snapshot),
            error.context(format!("retained worker {name} did not become Ready")),
            &recovery_location,
        ));
    }
    if target
        .cluster
        .as_ref()
        .is_some_and(|cluster| cluster.cni.as_deref() == Some("cilium"))
    {
        if let Err(error) = target_api.reset_cilium_agent_state(&name) {
            return Err(rollback_reverse_migration(
                source,
                target,
                previous_service,
                None,
                Some(&host_path_snapshot),
                error.context("rebuilding retained worker Cilium host datapath state"),
                &recovery_location,
            ));
        }
        if let Err(error) = wait_for_node(&target_api, &name) {
            return Err(rollback_reverse_migration(
                source,
                target,
                previous_service,
                None,
                Some(&host_path_snapshot),
                error.context(format!("Cilium reset left retained worker {name} unready")),
                &recovery_location,
            ));
        }
    }
    if let Err(error) =
        restore_node_scheduling_state(&target_api, &name, replacement_state.as_ref())
    {
        return Err(rollback_reverse_migration(
            source,
            target,
            previous_service,
            None,
            Some(&host_path_snapshot),
            error.context("restoring retained worker labels and scheduling state"),
            &recovery_location,
        ));
    }
    if request.uninstall_after_migrate {
        service::uninstall_source(source).with_context(|| {
            format!(
                "nodestore worker uninstall failed; worker volume snapshot retained at {}",
                host_path_snapshot.recovery_directory().display()
            )
        })?;
        host_path_snapshot.restore().with_context(|| {
            format!(
                "restoring worker local volumes failed; recovery snapshot is at {}",
                host_path_snapshot.recovery_directory().display()
            )
        })?;
    }
    println!(
        "Worker {name} returned to the retained {:?} installation and is Ready. Cluster-wide API resources were not re-imported from this worker; local-volume recovery snapshot retained at {}",
        request.to,
        host_path_snapshot.recovery_directory().display()
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

fn disable_nodestore_stack(source: &detect::Installation) -> Result<service::PreviousServiceState> {
    ensure!(
        source.distribution == request::Distribution::Nodestore,
        "the source stack shutdown helper is only valid when returning from nodestore"
    );
    service::disable(source).context("stopping the nodestore service and runtime stack")
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
    let started = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(300);
    let mut attempt = 0;
    while started.elapsed() < timeout {
        attempt += 1;
        match target.ready() {
            Ok(()) => return Ok(()),
            Err(error) => {
                eprintln!(
                    "nodemigrate: destination API readiness probe {attempt} failed: {error:#}"
                );
                last_error = Some(error);
            }
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(5).min(remaining));
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

fn wait_for_replacement_node(
    target: &transfer::KubeApi,
    name: &str,
    previous_uid: &str,
) -> Result<()> {
    let mut last_error = None;
    for _ in 0..60 {
        match target
            .ready()
            .and_then(|()| target.replacement_node_ready(name, previous_uid))
        {
            Ok(true) => return Ok(()),
            Ok(false) => {
                last_error = Some(anyhow::anyhow!(
                    "node {name} has not registered as a Ready replacement for UID {previous_uid}"
                ));
            }
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    Err(last_error
        .unwrap_or_else(|| anyhow::anyhow!("node {name} did not become a Ready replacement")))
    .context("waiting for fresh registration of the replacement node")
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
    std::env::var("NODEMIGRATE_NODE_NAME")
        .ok()
        .filter(|name| !name.is_empty())
        .or_else(|| {
            installation
                .cluster
                .as_ref()
                .and_then(|cluster| cluster.node_name.as_deref())
                .map(str::to_owned)
        })
        .or_else(|| std::env::var("NODELET_NODE_NAME").ok())
        .unwrap_or_else(hostname)
}

fn bootstrap_command(
    installation: &detect::Installation,
    disable_nodeproxy: bool,
) -> Result<Command> {
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
    append_nodeproxy_mode(&mut args, disable_nodeproxy);
    if let Some(node_name) = &config.node_name {
        args.push(format!("--node-name={node_name}"));
    }
    let source_api_server_name = transfer::KubeApi::source_api_server_name(installation)?;
    bootstrap_command_with_config(args, config, source_api_server_name)
}

fn prepare_migration_pki(
    command: &mut Command,
    source: &detect::Installation,
    export: &transfer::Export,
) -> Result<()> {
    if std::env::var("NODEBOOTSTRAP_JOIN_ENDPOINT").is_ok_and(|endpoint| !endpoint.is_empty()) {
        return Ok(());
    }
    let preserved_dir = export.dir.join("migration-pki");
    std::fs::create_dir_all(&preserved_dir)
        .with_context(|| format!("creating protected migration PKI directory {}", preserved_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&preserved_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting migration PKI directory {}", preserved_dir.display()))?;
    }

    let source_pki = source_api_pki_paths(source)?;
    let serving_ca_cert = preserve_pki_file(&source_pki.serving_ca_cert, &preserved_dir.join("ca.crt"))?;
    let serving_ca_key = preserve_pki_file(&source_pki.serving_ca_key, &preserved_dir.join("ca.key"))?;
    command
        .env("NODEBOOTSTRAP_MIGRATION_CA_CERT_FILE", serving_ca_cert)
        .env("NODEBOOTSTRAP_MIGRATION_CA_KEY_FILE", serving_ca_key);
    if let Some(client_ca) = source_pki.client_ca {
        let client_ca = preserve_pki_file(&client_ca, &preserved_dir.join("client-ca.crt"))?;
        command.env("NODEBOOTSTRAP_MIGRATION_CLIENT_CA_FILE", client_ca);
    }
    Ok(())
}

struct SourceApiPkiPaths {
    serving_ca_cert: PathBuf,
    serving_ca_key: PathBuf,
    client_ca: Option<PathBuf>,
}

fn source_api_pki_paths(source: &detect::Installation) -> Result<SourceApiPkiPaths> {
    match source.distribution {
        request::Distribution::Kubernetes => Ok(SourceApiPkiPaths {
            serving_ca_cert: PathBuf::from("/etc/kubernetes/pki/ca.crt"),
            serving_ca_key: PathBuf::from("/etc/kubernetes/pki/ca.key"),
            client_ca: None,
        }),
        request::Distribution::K3s => {
            let data_dir = source
                .cluster
                .as_ref()
                .context("K3s source has no detected data directory")?
                .data_dir
                .clone();
            let tls_dir = data_dir.join("server/tls");
            Ok(SourceApiPkiPaths {
                serving_ca_cert: tls_dir.join("server-ca.crt"),
                serving_ca_key: tls_dir.join("server-ca.key"),
                client_ca: Some(tls_dir.join("client-ca.crt")),
            })
        }
        request::Distribution::Nodestore => {
            anyhow::bail!("cannot derive source API PKI from a nodestore installation")
        }
    }
}

fn preserve_pki_file(source: &std::path::Path, destination: &std::path::Path) -> Result<PathBuf> {
    if destination.exists() {
        if !source.exists() {
            return Ok(destination.to_path_buf());
        }
        let source_bytes = std::fs::read(source)
            .with_context(|| format!("reading source PKI file {}", source.display()))?;
        let destination_bytes = std::fs::read(destination)
            .with_context(|| format!("reading retained PKI file {}", destination.display()))?;
        ensure!(
            source_bytes == destination_bytes,
            "protected migration PKI already contains different data at {}; refusing to replace it",
            destination.display()
        );
        return Ok(destination.to_path_buf());
    }
    let contents = std::fs::read(source)
        .with_context(|| format!("reading source PKI file {}", source.display()))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(destination)
        .with_context(|| format!("creating protected migration PKI file {}", destination.display()))?;
    use std::io::Write;
    file.write_all(&contents)
        .with_context(|| format!("writing protected migration PKI file {}", destination.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing protected migration PKI file {}", destination.display()))?;
    Ok(destination.to_path_buf())
}

fn replacement_worker_command(
    installation: &detect::Installation,
    node_name: &str,
    disable_nodeproxy: bool,
) -> Result<Command> {
    let config = installation
        .cluster
        .as_ref()
        .context("source cluster config was not detected")?;
    eprintln!(
        "nodemigrate: replacement worker Service router: {}",
        if disable_nodeproxy {
            "existing cluster proxy"
        } else {
            "nodeproxy"
        }
    );
    let kubeconfig = std::env::var_os("NODEBOOTSTRAP_WORKER_KUBECONFIG")
        .or_else(|| std::env::var_os("NODEMIGRATE_DESTINATION_KUBECONFIG"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("NODEBOOTSTRAP_KUBECONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/etc/nodebootstrap"))
                .join("admin.kubeconfig")
        });
    let args = replacement_worker_args(config, &kubeconfig, node_name, disable_nodeproxy);
    bootstrap_command_with_config(args, config, None)
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

fn promote_member_command() -> Result<Command> {
    let endpoint = std::env::var("NODEBOOTSTRAP_JOIN_ENDPOINT")
        .context("control-plane promotion requires NODEBOOTSTRAP_JOIN_ENDPOINT")?;
    let peer_url = std::env::var("NODEBOOTSTRAP_PEER_URL")
        .context("control-plane promotion requires NODEBOOTSTRAP_PEER_URL")?;
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
        "promote-member".to_string(),
    ]);
    Ok(command)
}

fn replacement_worker_args(
    config: &detect::ClusterConfig,
    kubeconfig: &std::path::Path,
    node_name: &str,
    disable_nodeproxy: bool,
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
    append_nodeproxy_mode(&mut args, disable_nodeproxy);
    args
}

fn nodeproxy_should_be_disabled(
    cilium_kube_proxy_replacement: bool,
    kube_proxy_present: bool,
) -> Result<bool> {
    ensure!(
        !(cilium_kube_proxy_replacement && kube_proxy_present),
        "source cluster has both Cilium kube-proxy replacement and a kube-proxy DaemonSet; resolve competing Service proxies before migration"
    );
    Ok(cilium_kube_proxy_replacement || kube_proxy_present)
}

fn append_nodeproxy_mode(args: &mut Vec<String>, disable_nodeproxy: bool) {
    if disable_nodeproxy {
        args.push("--proxy=none".to_string());
    }
}

fn bootstrap_command_with_config(
    mut args: Vec<String>,
    config: &detect::ClusterConfig,
    source_api_server_name: Option<String>,
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
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .context("reading mount table to detect active kubelet CSI staging paths")?;
    let csi_staging_root = migration_csi_staging_root(
        std::env::var_os("NODEMIGRATE_CSI_STAGING_ROOT").map(PathBuf::from),
        &mountinfo,
    )?;
    command
        .args(args)
        .env("NODEBOOTSTRAP_IPV4_CLUSTER_CIDR", ipv4_cluster_cidr)
        .env("NODEBOOTSTRAP_IPV6_CLUSTER_CIDR", ipv6_cluster_cidr)
        .env("NODELET_CSI_STAGING_ROOT", csi_staging_root);
    if let Some(name) = source_api_server_name {
        command.env("NODEBOOTSTRAP_APISERVER_EXTRA_SANS", name);
    }
    if config.cni.as_deref() != Some("flannel")
        || config.flannel_backend.as_deref().unwrap_or("vxlan") != "vxlan"
    {
        apply_cni_runtime_paths(
            &mut command,
            config.cni_conf_dir.as_deref(),
            config.cni_bin_dir.as_deref(),
        )?;
    }
    if let Some(address) = cluster_dns_ipv4 {
        command.env("NODEBOOTSTRAP_CLUSTER_DNS_IP", address);
    }
    if let Some(address) = cluster_dns_ipv6 {
        command.env("NODEBOOTSTRAP_CLUSTER_DNS_IP6", address);
    }
    Ok(command)
}

fn migration_csi_staging_root(configured: Option<PathBuf>, mountinfo: &str) -> Result<PathBuf> {
    let detected = detect_csi_staging_root(mountinfo)?;
    if let (Some(configured), Some(detected)) = (&configured, &detected) {
        ensure!(
            configured == detected,
            "NODEMIGRATE_CSI_STAGING_ROOT {} differs from active CSI staging root {}; refusing a cutover that would request duplicate staging",
            configured.display(),
            detected.display()
        );
    }
    let root = configured
        .or(detected)
        .unwrap_or_else(|| PathBuf::from("/var/lib/kubelet/plugins/kubernetes.io/csi"));
    ensure!(
        root.is_absolute(),
        "NODEMIGRATE_CSI_STAGING_ROOT must be an absolute path"
    );
    Ok(root)
}

fn detect_csi_staging_root(mountinfo: &str) -> Result<Option<PathBuf>> {
    let mut detected: Option<PathBuf> = None;
    for line in mountinfo.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let Some(mountpoint) = fields.get(4) else {
            continue;
        };
        let mountpoint = mountpoint
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        let path = PathBuf::from(mountpoint);
        let components: Vec<_> = path
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .collect();
        for index in 0..components.len().saturating_sub(5) {
            if components[index..index + 3] != ["plugins", "kubernetes.io", "csi"]
                || components[index + 5] != "globalmount"
                || components[index + 4].len() != 64
                || !components[index + 4]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                continue;
            }
            let mut root = PathBuf::from("/");
            for component in &components[1..index + 3] {
                root.push(component);
            }
            if let Some(previous) = &detected {
                ensure!(
                    previous == &root,
                    "active CSI staging mounts use multiple roots ({} and {}); refusing migration",
                    previous.display(),
                    root.display()
                );
            } else {
                detected = Some(root);
            }
        }
    }
    Ok(detected)
}

fn apply_cni_runtime_paths(
    command: &mut Command,
    conf_dir: Option<&std::path::Path>,
    bin_dir: Option<&std::path::Path>,
) -> Result<()> {
    match (conf_dir, bin_dir) {
        (Some(conf_dir), Some(bin_dir)) => {
            ensure!(
                conf_dir.is_absolute(),
                "CNI config directory must be absolute"
            );
            ensure!(
                bin_dir.is_absolute(),
                "CNI binary directory must be absolute"
            );
            command
                .env("NODEBOOTSTRAP_CNI_CONF_DIR", conf_dir)
                .env("NODEBOOTSTRAP_CNI_BIN_DIR", bin_dir);
        }
        (None, None) => {}
        _ => bail!("detected CNI config and binary directories must be provided together"),
    }
    Ok(())
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
         \x20 nodemigrate to=kubernetes from=nodestore import-export=/protected/export/path\n\
         \n\
         Later control-plane nodes joining an already migrated nodestore cluster\n\
         may use skip-api-import=true; the first control plane must import state.\n\
         For reverse multi-control-plane migration, stage-target=true starts\n\
         retained control planes without waiting for API quorum. Later staged\n\
         nodes can reuse the first protected export with source-export=/path to\n\
         recover local persistent data without contacting either API. After\n\
         target quorum returns, run import-export=/path once to import cluster\n\
         state. Use skip-api-export=true for later nodes after target API\n\
         readiness.\n\
         \n\
         `inspect` reports detected local Kubernetes installations. Migration\n\
         exports Kubernetes API objects into a protected recovery directory,\n\
         disables the source service, and keeps the export. Source uninstall\n\
         happens only with uninstall-after-migrate=true.\n\
         \n\
         Active CSI staging roots are detected from host mount state. If the\n\
         source uses a nonstandard CSI root and has no staged volumes, set\n\
         NODEMIGRATE_CSI_STAGING_ROOT to that absolute plugin directory."
    );
}

#[cfg(test)]
mod tests {
    use super::{
        append_nodeproxy_mode, apply_cni_runtime_paths, confirm_migration, detect_csi_staging_root,
        migration_csi_staging_root, nodeproxy_should_be_disabled, replacement_worker_args,
        source_api_pki_paths, preserve_pki_file,
        reverse_node_replacement_state, rollback_reverse_migration_with,
        validate_destination_node_replacement, validate_reverse_control_plane_options,
        validate_skip_api_import,
    };
    use crate::transfer::NodeSchedulingState;
    use crate::{
        detect::{ClusterConfig, Installation, K3sDatastore, NodeRole},
        request::{Distribution, MigrationRequest},
    };
    use std::collections::HashMap;
    use std::{
        fs,
        io::Cursor,
        path::{Path, PathBuf},
        process::Command,
    };

    #[test]
    fn source_api_pki_paths_select_distribution_specific_roots() {
        let kubernetes = Installation {
            distribution: Distribution::Kubernetes,
            role: NodeRole::ControlPlane,
            runtime_endpoint: None,
            service_manager: None,
            service_name: "kubelet".to_string(),
            service_file: None,
            binary: None,
            config_files: Vec::new(),
            cluster: Some(cluster(None, None)),
        };
        let paths = source_api_pki_paths(&kubernetes).unwrap();
        assert_eq!(paths.serving_ca_cert, PathBuf::from("/etc/kubernetes/pki/ca.crt"));
        assert_eq!(paths.serving_ca_key, PathBuf::from("/etc/kubernetes/pki/ca.key"));
        assert!(paths.client_ca.is_none());

        let mut k3s_cluster = cluster(None, None);
        k3s_cluster.data_dir = PathBuf::from("/srv/k3s-data");
        let k3s = Installation {
            distribution: Distribution::K3s,
            role: NodeRole::ControlPlane,
            runtime_endpoint: None,
            service_manager: None,
            service_name: "k3s".to_string(),
            service_file: None,
            binary: None,
            config_files: Vec::new(),
            cluster: Some(k3s_cluster),
        };
        let paths = source_api_pki_paths(&k3s).unwrap();
        assert_eq!(paths.serving_ca_cert, PathBuf::from("/srv/k3s-data/server/tls/server-ca.crt"));
        assert_eq!(paths.serving_ca_key, PathBuf::from("/srv/k3s-data/server/tls/server-ca.key"));
        assert_eq!(paths.client_ca, Some(PathBuf::from("/srv/k3s-data/server/tls/client-ca.crt")));
    }

    #[test]
    fn migration_ca_is_copied_into_private_export_and_existing_copy_is_checked() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source-ca.key");
        let saved = directory.path().join("export/migration-pki/ca.key");
        fs::create_dir_all(saved.parent().unwrap()).unwrap();
        fs::write(&source, "private source key").unwrap();

        preserve_pki_file(&source, &saved).unwrap();
        assert_eq!(fs::read(&saved).unwrap(), b"private source key");
        assert_eq!(preserve_pki_file(&source, &saved).unwrap(), saved);

        fs::write(&source, "changed key").unwrap();
        let error = preserve_pki_file(&source, &saved).unwrap_err();
        assert!(error.to_string().contains("refusing to replace it"));
        fs::remove_file(&source).unwrap();
        assert_eq!(preserve_pki_file(&source, &saved).unwrap(), saved);
    }

    #[test]
    fn reverse_migration_rollback_stops_target_before_restoring_data_and_source() {
        use std::cell::RefCell;

        let actions = RefCell::new(Vec::new());
        let error = rollback_reverse_migration_with(
            anyhow::anyhow!("API import failed"),
            "/var/lib/nodemigrate/exports/recovery",
            || {
                actions.borrow_mut().push("stop-target");
                Ok(())
            },
            || {
                actions.borrow_mut().push("restore-data");
                Ok(())
            },
            || {
                actions.borrow_mut().push("restore-source");
                Ok(())
            },
        );

        assert_eq!(
            *actions.borrow(),
            ["stop-target", "restore-data", "restore-source"]
        );
        let message = format!("{error:#}");
        assert!(message.contains("API import failed"));
        assert!(message.contains("nodestore was restored"));
        assert!(message.contains("/var/lib/nodemigrate/exports/recovery"));
    }

    #[test]
    fn reverse_migration_rollback_keeps_source_stopped_if_target_cannot_stop() {
        use std::cell::Cell;

        let source_restored = Cell::new(false);
        let error = rollback_reverse_migration_with(
            anyhow::anyhow!("API import failed"),
            "/var/lib/nodemigrate/exports/recovery",
            || Err(anyhow::anyhow!("target API port is still bound")),
            || Ok(()),
            || {
                source_restored.set(true);
                Ok(())
            },
        );

        assert!(!source_restored.get());
        let message = format!("{error:#}");
        assert!(message.contains("target API port is still bound"));
        assert!(message.contains("nodestore remains stopped"));
    }

    #[test]
    fn reverse_migration_rollback_restores_source_even_if_data_restore_fails() {
        use std::cell::Cell;

        let source_restored = Cell::new(false);
        let error = rollback_reverse_migration_with(
            anyhow::anyhow!("API import failed"),
            "/var/lib/nodemigrate/exports/recovery",
            || Ok(()),
            || Err(anyhow::anyhow!("host path is read-only")),
            || {
                source_restored.set(true);
                Ok(())
            },
        );

        assert!(source_restored.get());
        let message = format!("{error:#}");
        assert!(message.contains("nodestore was restored"));
        assert!(message.contains("host path is read-only"));
    }

    #[test]
    fn interactive_migration_requires_exact_yes() {
        let mut input = Cursor::new(b"Yes\n".to_vec());
        let mut output = Vec::new();

        let error = confirm_migration(true, &mut input, &mut output).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("confirmation must be exactly 'yes'")
        );
        let warning = String::from_utf8(output).unwrap();
        assert!(warning.contains("⚠️⚠️⚠️⚠️⚠️"));
        assert!(warning.contains("HIGH PROBABILITY OF DATA LOSS"));
        assert!(warning.contains("EXTERNAL BACKUP TOOLS"));
    }

    #[test]
    fn interactive_migration_accepts_exact_yes() {
        let mut input = Cursor::new(b"yes\n".to_vec());
        let mut output = Vec::new();

        confirm_migration(true, &mut input, &mut output).unwrap();

        let warning = String::from_utf8(output).unwrap();
        assert!(warning.contains("Type exactly \"yes\" to continue:"));
    }

    #[test]
    fn noninteractive_migration_logs_warning_without_prompting() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        confirm_migration(false, &mut input, &mut output).unwrap();

        let warning = String::from_utf8(output).unwrap();
        assert!(warning.contains("⚠️⚠️⚠️⚠️⚠️"));
        assert!(warning.contains("HIGH PROBABILITY OF DATA LOSS"));
        assert!(!warning.contains("Type exactly"));
    }

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
            cni_conf_dir: None,
            cni_bin_dir: None,
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
            false,
        );
        assert!(args.iter().any(|arg| arg == "--worker"));
        assert!(
            args.iter()
                .any(|arg| arg == "--kubeconfig=/etc/nodebootstrap/admin.kubeconfig")
        );
        assert!(args.iter().any(|arg| arg == "--node-name=old-node"));
        assert!(args.iter().any(|arg| arg == "--cni=none"));
        assert!(
            args.iter()
                .any(|arg| arg == "--cluster-domain=cluster.example")
        );
    }

    #[test]
    fn joined_replacement_preserves_explicit_flannel_setup() {
        let config = cluster(Some("flannel"), Some("vxlan"));
        let args =
            replacement_worker_args(&config, Path::new("/tmp/admin.kubeconfig"), "node", false);
        assert!(args.iter().any(|arg| arg == "--cni=flannel"));
    }

    #[test]
    fn joined_replacement_does_not_replace_non_vxlan_or_external_cni() {
        let config = cluster(Some("flannel"), Some("wireguard-native"));
        let args =
            replacement_worker_args(&config, Path::new("/tmp/admin.kubeconfig"), "node", false);
        assert!(args.iter().any(|arg| arg == "--cni=none"));
    }

    #[test]
    fn joined_cilium_worker_disables_nodeproxy_when_cilium_replaces_kube_proxy() {
        let config = cluster(Some("cilium"), None);
        let args =
            replacement_worker_args(&config, Path::new("/tmp/admin.kubeconfig"), "node", true);
        assert!(args.iter().any(|arg| arg == "--proxy=none"));
    }

    #[test]
    fn joined_cilium_worker_keeps_nodeproxy_when_cilium_kpr_is_disabled() {
        let config = cluster(Some("cilium"), None);
        let args =
            replacement_worker_args(&config, Path::new("/tmp/admin.kubeconfig"), "node", false);
        assert!(!args.iter().any(|arg| arg == "--proxy=none"));
    }

    #[test]
    fn source_kube_proxy_owns_routing_when_cilium_kpr_is_disabled() {
        assert!(nodeproxy_should_be_disabled(false, true).unwrap());
        assert!(!nodeproxy_should_be_disabled(false, false).unwrap());
        assert!(nodeproxy_should_be_disabled(true, false).unwrap());
        assert!(nodeproxy_should_be_disabled(true, true).is_err());
    }

    #[test]
    fn initial_cilium_bootstrap_disables_nodeproxy_when_kpr_is_enabled() {
        let mut args = vec!["--release".to_string()];

        append_nodeproxy_mode(&mut args, true);

        assert!(args.iter().any(|arg| arg == "--proxy=none"));
    }

    #[test]
    fn initial_cilium_bootstrap_keeps_nodeproxy_when_kpr_is_disabled() {
        let mut args = vec!["--release".to_string()];

        append_nodeproxy_mode(&mut args, false);

        assert!(!args.iter().any(|arg| arg == "--proxy=none"));
    }

    #[test]
    fn forwards_external_cni_runtime_directories_to_nodebootstrap() {
        let mut command = Command::new("nodebootstrap");
        apply_cni_runtime_paths(
            &mut command,
            Some(Path::new("/var/lib/rancher/k3s/agent/etc/cni/net.d")),
            Some(Path::new("/var/lib/rancher/k3s/data/current/bin")),
        )
        .unwrap();

        let env: Vec<_> = command.get_envs().collect();
        assert!(env.contains(&(
            std::ffi::OsStr::new("NODEBOOTSTRAP_CNI_CONF_DIR"),
            Some(std::ffi::OsStr::new(
                "/var/lib/rancher/k3s/agent/etc/cni/net.d"
            ))
        )));
        assert!(env.contains(&(
            std::ffi::OsStr::new("NODEBOOTSTRAP_CNI_BIN_DIR"),
            Some(std::ffi::OsStr::new(
                "/var/lib/rancher/k3s/data/current/bin"
            ))
        )));
    }

    #[test]
    fn rejects_incomplete_or_relative_cni_runtime_directories() {
        let mut command = Command::new("nodebootstrap");
        assert!(apply_cni_runtime_paths(&mut command, Some(Path::new("relative")), None).is_err());
        assert!(
            apply_cni_runtime_paths(
                &mut command,
                Some(Path::new("relative")),
                Some(Path::new("/opt/cni/bin"))
            )
            .is_err()
        );
    }

    #[test]
    fn migration_uses_kubelet_csi_staging_root_and_accepts_an_explicit_root() {
        assert_eq!(
            migration_csi_staging_root(None, "").unwrap(),
            Path::new("/var/lib/kubelet/plugins/kubernetes.io/csi")
        );
        assert_eq!(
            migration_csi_staging_root(
                Some(PathBuf::from("/srv/kubelet/plugins/kubernetes.io/csi")),
                ""
            )
            .unwrap(),
            Path::new("/srv/kubelet/plugins/kubernetes.io/csi")
        );
        assert!(migration_csi_staging_root(Some(PathBuf::from("relative/csi")), "").is_err());
    }

    #[test]
    fn migration_detects_active_kubelet_csi_staging_root() {
        let root = "/srv/kubelet/plugins/kubernetes.io/csi";
        let mountinfo = format!(
            "36 25 0:32 / {root}/hostpath.csi.k8s.io/{}/globalmount rw,nosuid - ext4 /dev/sdb rw\n",
            "a".repeat(64)
        );
        assert_eq!(
            detect_csi_staging_root(&mountinfo).unwrap(),
            Some(PathBuf::from(root))
        );
        assert_eq!(
            migration_csi_staging_root(None, &mountinfo).unwrap(),
            PathBuf::from(root)
        );
        assert!(
            migration_csi_staging_root(
                Some(PathBuf::from("/var/lib/kubelet/plugins/kubernetes.io/csi")),
                &mountinfo
            )
            .is_err()
        );
    }

    #[test]
    fn existing_destination_node_requires_explicit_replacement() {
        assert!(validate_destination_node_replacement(true, false).is_err());
        assert!(validate_destination_node_replacement(true, true).is_ok());
        assert!(validate_destination_node_replacement(false, false).is_ok());
    }

    #[test]
    fn reverse_replacement_ignores_state_from_a_node_registered_after_activation() {
        let nodestore_state = NodeSchedulingState {
            uid: Some("nodestore-node-uid".to_string()),
            labels: HashMap::from([("operator.example/pool".to_string(), "blue".to_string())]),
            annotations: HashMap::from([(
                "operator.example/state".to_string(),
                "kept".to_string(),
            )]),
            taints: vec![serde_json::json!({"key": "reserved", "effect": "NoSchedule"})],
            unschedulable: Some(true),
        };
        let newly_registered_state = NodeSchedulingState {
            labels: HashMap::from([("kubernetes.io/hostname".to_string(), "node-a".to_string())]),
            ..NodeSchedulingState::default()
        };

        assert_eq!(
            reverse_node_replacement_state(
                false,
                Some(newly_registered_state),
                Some(nodestore_state.clone()),
            ),
            Some(nodestore_state)
        );
    }

    #[test]
    fn skip_api_import_requires_existing_cluster_join() {
        let request = MigrationRequest::parse(&[
            "to=nodestore".to_string(),
            "from=kubernetes".to_string(),
            "skip-api-import=true".to_string(),
        ])
        .unwrap();
        let source = Installation {
            distribution: Distribution::Kubernetes,
            role: NodeRole::ControlPlane,
            runtime_endpoint: None,
            service_manager: None,
            service_name: "kubelet".to_string(),
            service_file: None,
            binary: None,
            config_files: Vec::new(),
            cluster: None,
        };

        assert!(validate_skip_api_import(&request, &source, true).is_ok());
        assert!(validate_skip_api_import(&request, &source, false).is_err());

        let mut worker = source;
        worker.role = NodeRole::Worker;
        assert!(validate_skip_api_import(&request, &worker, true).is_ok());
        assert!(validate_skip_api_import(&request, &worker, false).is_err());
    }

    #[test]
    fn reverse_staging_requires_control_planes_at_both_ends() {
        let request = MigrationRequest::parse(&[
            "to=kubernetes".to_string(),
            "from=nodestore".to_string(),
            "stage-target=true".to_string(),
        ])
        .unwrap();
        let control_plane = Installation {
            distribution: Distribution::Nodestore,
            role: NodeRole::ControlPlane,
            runtime_endpoint: None,
            service_manager: None,
            service_name: "nodestore".to_string(),
            service_file: None,
            binary: None,
            config_files: Vec::new(),
            cluster: None,
        };
        let target = Installation {
            distribution: Distribution::Kubernetes,
            service_name: "kubelet".to_string(),
            ..control_plane.clone()
        };

        assert!(validate_reverse_control_plane_options(&request, &control_plane, &target).is_ok());

        let mut worker = control_plane.clone();
        worker.role = NodeRole::Worker;
        assert!(validate_reverse_control_plane_options(&request, &worker, &target).is_err());

        let mut worker_target = target;
        worker_target.role = NodeRole::Worker;
        assert!(
            validate_reverse_control_plane_options(&request, &control_plane, &worker_target)
                .is_err()
        );
    }
}
