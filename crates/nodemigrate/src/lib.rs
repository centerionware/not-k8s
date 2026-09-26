pub mod detect;
pub mod request;
pub mod service;
pub mod transfer;

use std::{
    io::{self, BufRead, IsTerminal, Write},
    path::PathBuf,
    process::Command,
};

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
        .map(|path| transfer::Export::load(path))
        .transpose()?;
    let source_api = if export.is_some() {
        None
    } else {
        Some(transfer::KubeApi::source(source)?)
    };
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
    let bootstrap = bootstrap_command(source)?;
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
        println!("Migration plan: {:?} -> nodestore; source nodes={source_nodes}; destination={}; source CNI={cni}{replacement}; replace-existing-node={replace_existing_node}; cluster-api-import={}; {}source service will be disabled; uninstall-after-migrate={}", request.from, if joins_existing { "existing cluster" } else { "new cluster" }, if request.skip_api_import { "skipped (state imported by an earlier control plane)" } else { "enabled" }, if joins_existing { "install node agent on joined node; " } else { "" }, request.uninstall_after_migrate);
        return Ok(());
    }

    if let Some(api) = &source_api {
        export = Some(api.export(source)?);
    }
    let mut export = export.context("source API export was not prepared")?;
    println!(
        "Protected API object export saved at {}",
        export.dir.display()
    );
    // Stopping the K3s service also stops its embedded containerd. Tear down
    // K3s pod sandboxes while that CRI endpoint is still available; otherwise
    // cleanup would run after `/run/k3s/containerd/containerd.sock` disappears
    // and source containers could remain bound to host sockets and mounts.
    let mut source_cilium_identity = if source.distribution == request::Distribution::K3s {
        service::stop_source_pod_sandboxes(source)
            .context("stopping K3s pod sandboxes before disabling the source service")?
    } else {
        service::SourceCiliumIdentity::default()
    };
    let previous_service = service::disable(source)?;
    if source.distribution == request::Distribution::Kubernetes {
        if let Err(error) = service::stop_upstream_static_pods(source) {
            if let Err(restore_error) = service::restore(source, previous_service) {
                bail!("stopping upstream static pods failed ({error:#}) and restoring the source service failed ({restore_error:#})");
            }
            return Err(error)
                .context("stopping source Kubernetes static pods; source service was restored");
        }
        let sandbox_cleanup = service::stop_source_pod_sandboxes(source);
        match sandbox_cleanup {
            Ok(identity) => source_cilium_identity = identity,
            Err(error) => {
                if let Err(restore_error) = service::restore(source, previous_service) {
                    bail!("stopping source pod sandboxes failed ({error:#}) and restoring the source service failed ({restore_error:#})");
                }
                return Err(error).context(
                    "stopping source pod sandboxes before migration; source service was restored",
                );
            }
        }
    }
    if let Err(error) = service::stop_orphaned_cilium_processes(source, &source_cilium_identity) {
        if let Err(restore_error) = service::restore(source, previous_service) {
            bail!("stopping orphaned source Cilium processes failed ({error:#}) and restoring the source service failed ({restore_error:#})");
        }
        return Err(error)
            .context("stopping orphaned source Cilium processes; source service was restored");
    }
    let snapshot_result = if request.source_export.is_some() {
        export.snapshot_host_paths_for_node(&migrating_node_name)
    } else {
        export.snapshot_host_paths()
    };
    if let Err(error) = snapshot_result {
        if let Err(restore_error) = service::restore(source, previous_service) {
            bail!("snapshotting local persistent volumes failed ({error:#}); restoring the source service also failed ({restore_error:#})");
        }
        return Err(error)
            .context("local persistent volume snapshot failed; original service was restored");
    }
    if request.uninstall_after_migrate {
        if let Err(error) = export.snapshot_k3s_cni_paths(source) {
            if let Err(restore_error) = service::restore(source, previous_service) {
                bail!("snapshotting K3s CNI files failed ({error:#}); restoring the source service also failed ({restore_error:#})");
            }
            return Err(error).context("K3s CNI snapshot failed; original service was restored");
        }
    }
    if let Err(error) = run_bootstrap(bootstrap) {
        return Err(rollback_forward_migration(
            source,
            previous_service,
            &export,
            error.context("nodestore bootstrap failed"),
        ));
    }

    if let Err(error) = wait_for_api(&target_api) {
        return Err(rollback_forward_migration(
            source,
            previous_service,
            &export,
            error.context("destination did not become ready"),
        ));
    }
    if !request.skip_api_import {
        if let Err(error) = target_api.import(&export) {
            return Err(rollback_forward_migration(
                source,
                previous_service,
                &export,
                error
                    .context("destination bootstrap succeeded but Kubernetes object import failed"),
            ));
        }
    }
    let replacement_state = if destination_node_exists {
        remove_replaced_node(&target_api, &migrating_node_name, replace_existing_node)
            .with_context(|| {
                format!(
                    "preparing destination control-plane node; export retained at {}",
                    export.dir.display()
                )
            })?
            .or(source_node_state)
    } else {
        source_node_state
    };
    if joins_existing {
        let worker = replacement_worker_command(source, &migrating_node_name)?;
        run_bootstrap(worker).with_context(|| format!(
            "installing the node agent on the joined replacement node; source remains disabled and the protected export is at {}",
            export.dir.display()
        ))?;
    }
    wait_for_node(&target_api, &migrating_node_name)?;
    restore_node_scheduling_state(
        &target_api,
        &migrating_node_name,
        replacement_state.as_ref(),
    )
    .context("restoring destination control-plane node labels and scheduling state")?;
    if joins_existing {
        if let Some(old_member_id) = replacement_member_id {
            run_bootstrap(replace_member_command(&old_member_id.to_string())?).with_context(|| format!(
                "promoting this Ready replacement before retiring old nodestore member {old_member_id}; source remains disabled and recovery export is at {}",
                export.dir.display()
            ))?;
        }
    }
    if request.uninstall_after_migrate {
        if let Err(uninstall_error) = service::uninstall_source(source) {
            let host_path_error = export.restore_host_paths().err();
            let cni_path_error = export.restore_k3s_cni_paths().err();
            bail!("source uninstall failed ({uninstall_error:#}); persistent-path restore error={host_path_error:#?}; CNI-path restore error={cni_path_error:#?}; recovery export retained at {}", export.dir.display());
        }
        let host_path_error = export.restore_host_paths().err();
        let cni_path_error = export.restore_k3s_cni_paths().err();
        ensure!(
            host_path_error.is_none() && cni_path_error.is_none(),
            "post-uninstall restore failed: persistent-path error={host_path_error:#?}; CNI-path error={cni_path_error:#?}; recovery export retained at {}",
            export.dir.display()
        );
        run_bootstrap(bootstrap_command(source)?)
            .context("reconciling nodestore after source uninstall")?;
        wait_for_api(&target_api)
            .context("destination failed API readiness after K3s uninstall cleanup")?;
        wait_for_node(&target_api, &migrating_node_name)
            .context("replacement node failed readiness after K3s uninstall cleanup")?;
    }
    println!("Migration completed and the destination API passed readiness checks. Export retained at {}", export.dir.display());
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
    if let Err(restore_error) = service::restore(source, previous_service) {
        return cause.context(format!(
            "source service restoration failed ({restore_error:#}); source remains disabled; partial nodestore services were stopped; protected export retained at {recovery}"
        ));
    }
    cause.context(format!(
        "source service was restored after rollback; partial nodestore services were stopped; protected export retained at {recovery}"
    ))
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
    let local_node_state = if let Some(path) = request.source_export.as_ref() {
        let export = transfer::Export::load(path)?;
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
    let worker = replacement_worker_command(source, &name)?;
    if request.plan_only {
        let cni = source
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.cni.as_deref())
            .unwrap_or("external or undetected");
        println!("Migration plan: {:?} worker -> existing nodestore cluster; node={name}; source CNI={cni}; replace-existing-node={existing_node}; source service '{}' will be disabled; cluster API objects are managed by the control-plane migration", request.from, source.service_name);
        return Ok(());
    }

    let mut host_path_snapshot = target_api.snapshot_host_paths(local_node_labels)?;
    if request.uninstall_after_migrate {
        host_path_snapshot.snapshot_k3s_cni_paths(source)?;
    }
    println!(
        "Worker local-volume recovery snapshot saved at {}",
        host_path_snapshot.recovery_directory().display()
    );
    let source_cilium_identity = if source.distribution == request::Distribution::K3s {
        service::stop_source_pod_sandboxes(source)
            .context("stopping K3s worker pod sandboxes before disabling the source service")?
    } else {
        service::SourceCiliumIdentity::default()
    };
    let previous_service = service::disable(source)?;
    let source_cilium_identity = if source.distribution == request::Distribution::Kubernetes {
        match service::stop_source_pod_sandboxes(source) {
            Ok(identity) => identity,
            Err(error) => {
                if let Err(restore_error) = service::restore(source, previous_service) {
                    bail!("stopping source worker pod sandboxes failed ({error:#}) and restoring the source service failed ({restore_error:#})");
                }
                return Err(error).context(
                    "stopping source worker pod sandboxes before migration; source service was restored",
                );
            }
        }
    } else {
        source_cilium_identity
    };
    if let Err(error) = service::stop_orphaned_cilium_processes(source, &source_cilium_identity) {
        if let Err(restore_error) = service::restore(source, previous_service) {
            bail!("stopping orphaned source worker Cilium processes failed ({error:#}) and restoring the source service failed ({restore_error:#})");
        }
        return Err(error).context(
            "stopping orphaned source worker Cilium processes; source service was restored",
        );
    }
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
        if let Err(restore_error) = service::restore(source, previous_service) {
            bail!("worker bootstrap failed ({error:#}) and restoring source service failed ({restore_error:#})");
        }
        return Err(error).context("installing the worker into the joined nodestore cluster");
    }
    wait_for_node(&target_api, &name).context(format!(
        "replacement worker {name} did not become Ready; source remains disabled"
    ))?;
    restore_node_scheduling_state(&target_api, &name, replacement_state.as_ref())
        .context("restoring replacement worker labels and scheduling state")?;
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
    println!("Worker {name} joined the nodestore cluster and is Ready. Cluster-wide API resources were not re-imported from this worker; local-volume recovery snapshot retained at {}", host_path_snapshot.recovery_directory().display());
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
    let target_api = transfer::KubeApi::destination(request.to)?;
    let source_api = if request.skip_api_export {
        None
    } else {
        Some(transfer::KubeApi::source(source)?)
    };
    if let Some(source_api) = &source_api {
        source_api.ready()?;
    }
    if request.skip_api_export {
        target_api.ready().context(
            "skip-api-export requires the retained destination cluster to be Ready through NODEMIGRATE_DESTINATION_KUBECONFIG",
        )?;
    }
    let returning_node_name = node_name(target);
    let source_node_state = source_api
        .as_ref()
        .map(|api| api.node_scheduling_state(&returning_node_name))
        .transpose()?
        .flatten();
    let destination_node_exists = if request.skip_api_export {
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
        println!("Migration plan: nodestore -> {:?}; retained target service '{}' will be enabled and started; target CNI={cni}; replace-existing-node={replace_existing_node}; source-api-export={}; stage-target={}; uninstall-after-migrate={}", request.to, target.service_name, if request.skip_api_export { "skipped (destination already has cluster state)" } else { "enabled" }, request.stage_target, request.uninstall_after_migrate);
        return Ok(());
    }
    let mut export = source_api
        .as_ref()
        .map(|source_api| source_api.export(source))
        .transpose()?;
    let host_path_snapshot = if request.skip_api_export {
        let name = node_name(target);
        let local_node_labels = target_api.node_labels(&name).ok();
        Some(target_api.snapshot_host_paths(local_node_labels.as_ref())?)
    } else {
        None
    };
    if let Some(export) = &export {
        println!(
            "Protected API object export saved at {}",
            export.dir.display()
        );
    }
    if let Some(snapshot) = &host_path_snapshot {
        println!(
            "Local-volume recovery snapshot saved at {}",
            snapshot.recovery_directory().display()
        );
    }
    let recovery_location = export
        .as_ref()
        .map(|export| export.dir.display().to_string())
        .or_else(|| {
            host_path_snapshot
                .as_ref()
                .map(|snapshot| snapshot.recovery_directory().display().to_string())
        })
        .unwrap_or_else(|| "no new export was created".to_string());
    let previous_service = service::disable(source).with_context(|| {
        format!("disabling nodestore failed; recovery data is at {recovery_location}")
    })?;
    if let Some(export) = &mut export {
        if let Err(error) = export.snapshot_host_paths() {
            if let Err(restore_error) = service::restore(source, previous_service) {
                bail!("snapshotting local persistent volumes failed ({error:#}); restoring nodestore also failed ({restore_error:#}); recovery data is at {recovery_location}");
            }
            return Err(error).context(format!(
                "local persistent volume snapshot failed; nodestore was restored; recovery data is at {recovery_location}"
            ));
        }
    }
    if let Err(error) = service::activate(target) {
        if let Err(restore_error) = service::restore(source, previous_service) {
            bail!("starting the retained target failed ({error:#}); restoring nodestore also failed ({restore_error:#}); recovery data is at {recovery_location}");
        }
        return Err(error).context(format!(
            "starting the retained target failed; nodestore was restored; recovery data is at {recovery_location}"
        ));
    }
    if request.stage_target {
        let recovery = export
            .as_ref()
            .map(|export| export.dir.display().to_string())
            .unwrap_or_else(|| "not created".to_string());
        println!("Retained control plane staged: source nodestore services are disabled and '{}' is running. The destination API may remain unavailable until another retained control plane is started. API export retained at {recovery}", target.service_name);
        return Ok(());
    }
    wait_for_api(&target_api).context(format!(
        "retained destination API did not become ready; nodestore remains stopped and recovery data is at {recovery_location}"
    ))?;
    if let Some(export) = &export {
        if let Err(error) = target_api.import(export) {
            bail!("destination started but API import failed: {error:#}; nodestore remains stopped and export is at {}", export.dir.display());
        }
    }
    let observed_replacement_state = if destination_node_exists || replace_existing_node {
        remove_replaced_node(
            &target_api,
            &returning_node_name,
            replace_existing_node,
        )
        .with_context(|| format!("preparing retained control-plane node; nodestore remains stopped and recovery data is at {recovery_location}"))?
    } else {
        None
    };
    let replacement_state = reverse_node_replacement_state(
        destination_node_exists,
        observed_replacement_state,
        source_node_state,
    );
    wait_for_node(&target_api, &returning_node_name).context(format!(
        "retained destination node did not become Ready; recovery data is at {recovery_location}"
    ))?;
    restore_node_scheduling_state(
        &target_api,
        &returning_node_name,
        replacement_state.as_ref(),
    )
    .context("restoring retained control-plane node labels and scheduling state")?;
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
    target_api.import(&export).with_context(|| {
        format!(
            "resuming migration API import; protected export remains at {}",
            export.dir.display()
        )
    })?;
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
        println!("Migration plan: nodestore worker -> {:?} worker; node={name}; retained target service '{}' will be enabled and started; cluster API objects are managed by the control-plane migration", request.to, target.service_name);
        return Ok(());
    }

    let host_path_snapshot = target_api.snapshot_host_paths(local_node_labels)?;
    println!(
        "Worker local-volume recovery snapshot saved at {}",
        host_path_snapshot.recovery_directory().display()
    );
    let previous_service = service::disable(source)?;
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
    if let Err(error) = service::activate(target) {
        if let Err(restore_error) = service::restore(source, previous_service) {
            bail!("starting the retained worker service failed ({error:#}) and restoring nodestore services failed ({restore_error:#})");
        }
        return Err(error).context("starting the retained Kubernetes worker service");
    }
    wait_for_node(&target_api, &name).context(format!(
        "retained worker {name} did not become Ready; nodestore worker remains disabled"
    ))?;
    restore_node_scheduling_state(&target_api, &name, replacement_state.as_ref())
        .context("restoring retained worker labels and scheduling state")?;
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
    println!("Worker {name} returned to the retained {:?} installation and is Ready. Cluster-wide API resources were not re-imported from this worker; local-volume recovery snapshot retained at {}", request.to, host_path_snapshot.recovery_directory().display());
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
                || !components[index + 4].bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                continue;
            }
            let mut root = PathBuf::from("/");
            for component in &components[1..index] {
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
         an early retained control plane without waiting for API quorum; use\n\
         import-export=/path after retained control-plane quorum returns to\n\
         import the staged API state. Use skip-api-export=true for later nodes\n\
         after target state has been imported.\n\
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
        apply_cni_runtime_paths, confirm_migration, detect_csi_staging_root,
        migration_csi_staging_root, replacement_worker_args,
        reverse_node_replacement_state, validate_destination_node_replacement,
        validate_reverse_control_plane_options, validate_skip_api_import,
    };
    use crate::transfer::NodeSchedulingState;
    use crate::{
        detect::{ClusterConfig, Installation, K3sDatastore, NodeRole},
        request::{Distribution, MigrationRequest},
    };
    use std::collections::HashMap;
    use std::{io::Cursor, path::{Path, PathBuf}, process::Command};

    #[test]
    fn interactive_migration_requires_exact_yes() {
        let mut input = Cursor::new(b"Yes\n".to_vec());
        let mut output = Vec::new();

        let error = confirm_migration(true, &mut input, &mut output).unwrap_err();

        assert!(error
            .to_string()
            .contains("confirmation must be exactly 'yes'"));
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
        assert!(apply_cni_runtime_paths(
            &mut command,
            Some(Path::new("relative")),
            Some(Path::new("/opt/cni/bin"))
        )
        .is_err());
    }

    #[test]
    fn migration_uses_kubelet_csi_staging_root_and_accepts_an_explicit_root() {
        assert_eq!(
            migration_csi_staging_root(None, "").unwrap(),
            Path::new("/var/lib/kubelet/plugins/kubernetes.io/csi")
        );
        assert_eq!(
            migration_csi_staging_root(Some(PathBuf::from(
                "/srv/kubelet/plugins/kubernetes.io/csi"
            )), "")
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
        assert!(migration_csi_staging_root(
            Some(PathBuf::from("/var/lib/kubelet/plugins/kubernetes.io/csi")),
            &mountinfo
        )
        .is_err());
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
