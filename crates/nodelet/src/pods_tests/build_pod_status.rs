//! bps(): the fix for the self-feedback reconcile loop that
//! was hammering the apiserver (commit c1dea26). Stamping every condition
//! with now() unconditionally made every patch_status a real diff, which
//! re-triggered the watch, which re-triggered reconcile(), forever. These
//! tests pin down that an unchanged condition must carry its old timestamp
//! forward, and a genuinely changed one must get a new one.
use super::*;
use crate::probes;
use crate::runtime::{ContainerRuntimeStatus, Phase, RuntimeStatus};
use http::{Request, Response};
use kube::client::Body;
use std::convert::Infallible;
use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
use tower::service_fn;

/// build_pod_status() with a default health map, i.e. "no probe supervisor
/// tracking anything" — matches every test in this file, which is about
/// phase/condition/timestamp bookkeeping, not probe-driven readiness (see
/// probes_tests/supervisor.rs for that).
fn bps(host_ip: &str, rt: &RuntimeStatus, prev: Option<&PodStatus>) -> PodStatus {
    bps_with_gates(host_ip, rt, prev, &[])
}

fn bps_with_gates(host_ip: &str, rt: &RuntimeStatus, prev: Option<&PodStatus>, readiness_gates: &[String]) -> PodStatus {
    build_pod_status(host_ip, "default", "app", rt, prev, readiness_gates, &probes::new_health_map(), crate::eviction::QosClass::BestEffort, None)
}

fn bps_with_health(host_ip: &str, rt: &RuntimeStatus, health: &probes::HealthMap) -> PodStatus {
    build_pod_status(host_ip, "default", "app", rt, None, &[], health, crate::eviction::QosClass::BestEffort, None)
}

/// Directly seed the health map's entry for one container — `set_health()`
/// is private to `probes`, but `HealthMap`'s inner map and `ContainerHealth`'s
/// fields are all public, so this is just as real as going through it.
fn seed_health(health: &probes::HealthMap, container: &str, ready: bool, started: bool) {
    health
        .lock()
        .unwrap()
        .entry(crate::runtime::pod_key("default", "app"))
        .or_default()
        .insert(container.to_string(), probes::ContainerHealth { ready, started });
}

fn running_status() -> RuntimeStatus {
    RuntimeStatus {
        phase: Phase::Running,
        message: None,
        started_at: None,
        pod_ip: Some("10.42.0.2".to_string()),
        containers: vec![ContainerRuntimeStatus {
            name: "app".to_string(),
            image: "busybox".to_string(),
            ready: true,
            running: true,
            container_id: Some("abc123".to_string()),
            restart_count: 0,
            ..Default::default()
        }],
        init_containers: Vec::new(),
        ephemeral_containers: Vec::new(),
        initialized: true,
    }
}

fn pending_status() -> RuntimeStatus {
    RuntimeStatus {
        phase: Phase::Pending,
        message: None,
        started_at: None,
        pod_ip: None,
        containers: vec![ContainerRuntimeStatus {
            name: "app".to_string(),
            image: "busybox".to_string(),
            ready: false,
            running: false,
            container_id: None,
            restart_count: 0,
            ..Default::default()
        }],
        init_containers: Vec::new(),
        ephemeral_containers: Vec::new(),
        initialized: true,
    }
}

fn fixed_time(seconds_ago: i64) -> Time {
    Time(k8s_openapi::jiff::Timestamp::now() - k8s_openapi::jiff::Span::new().seconds(seconds_ago))
}

#[test]
fn no_prev_status_stamps_fresh_timestamps() {
    let status = bps("10.0.0.1", &running_status(), None);
    for c in status.conditions.as_ref().unwrap() {
        assert!(c.last_transition_time.is_some());
    }
}

#[test]
fn unchanged_condition_preserves_prior_timestamp() {
    // This is THE regression test for the self-feedback loop: calling this
    // twice with the same underlying RuntimeStatus must produce identical
    // condition timestamps, or every "unchanged" reconcile still generates
    // a real diff against the stored object.
    let old_time = fixed_time(300);
    let prev = PodStatus {
        conditions: Some(vec![PodCondition {
            type_: "Ready".to_string(),
            status: "True".to_string(),
            last_transition_time: Some(old_time.clone()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let status = bps("10.0.0.1", &running_status(), Some(&prev));
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True"); // still running -> still True
    assert_eq!(ready.last_transition_time, Some(old_time));
}

#[test]
fn full_status_payload_is_stable_on_an_unchanged_reconcile() {
    // A condition timestamp is only one possible source of churn. Compare the
    // complete serialized status that write_status() sends: if this changes on
    // an unchanged runtime state, every watch Apply can become another PATCH.
    let runtime = running_status();
    let first = bps("10.0.0.1", &runtime, None);
    let second = bps("10.0.0.1", &runtime, Some(&first));

    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&second).unwrap(),
        "unchanged runtime state must produce a byte-equivalent status payload"
    );
}

fn counting_client(patches: Arc<AtomicUsize>) -> Client {
    let response = Arc::new(
        serde_json::to_vec(&serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "app", "namespace": "default"}
        }))
        .unwrap(),
    );
    let service = service_fn(move |request: Request<Body>| {
        let patches = patches.clone();
        let response = response.clone();
        async move {
            if request.method() == http::Method::PATCH && request.uri().path().ends_with("/status") {
                patches.fetch_add(1, Ordering::Relaxed);
            }
            Ok::<_, Infallible>(Response::new(Body::from(response.as_ref().clone())))
        }
    });
    Client::new(service, "default")
}

#[tokio::test]
async fn unchanged_status_skips_the_http_patch() {
    let patches = Arc::new(AtomicUsize::new(0));
    let client = counting_client(patches.clone());
    let runtime = running_status();
    let previous = bps("10.0.0.1", &runtime, None);

    write_status(
        &client,
        "10.0.0.1",
        "default",
        "app",
        &runtime,
        Some(&previous),
        &[],
        &probes::new_health_map(),
        crate::eviction::QosClass::BestEffort,
        None,
    )
    .await
    .unwrap();

    assert_eq!(patches.load(Ordering::Relaxed), 0);
}

#[test]
fn server_owned_status_fields_do_not_force_a_patch() {
    let runtime = running_status();
    let mut previous = bps("10.0.0.1", &runtime, None);
    // The scheduler (not nodelet) fills this field during preemption, and
    // build_pod_status() never touches it. A server-owned field must not
    // make every later reconcile look dirty. (Round 55: qosClass moved
    // from "server-owned, nodelet leaves alone" to "nodelet's own,
    // computed every reconcile" — see the qos_class tests below for that
    // field's coverage now.)
    previous.nominated_node_name = Some("other-node".to_string());
    let desired = bps("10.0.0.1", &runtime, Some(&previous));

    assert!(!status_patch_changes(Some(&previous), &desired));
}

#[tokio::test]
async fn changed_status_still_sends_the_http_patch() {
    let patches = Arc::new(AtomicUsize::new(0));
    let client = counting_client(patches.clone());
    let previous = bps("10.0.0.1", &pending_status(), None);

    write_status(
        &client,
        "10.0.0.1",
        "default",
        "app",
        &running_status(),
        Some(&previous),
        &[],
        &probes::new_health_map(),
        crate::eviction::QosClass::BestEffort,
        None,
    )
    .await
    .unwrap();

    assert_eq!(patches.load(Ordering::Relaxed), 1);
}

#[test]
fn changed_condition_gets_a_fresh_timestamp_not_the_old_one() {
    let old_time = fixed_time(300);
    let prev = PodStatus {
        conditions: Some(vec![PodCondition {
            type_: "Ready".to_string(),
            status: "False".to_string(), // was NOT ready
            last_transition_time: Some(old_time.clone()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    // Now running -> Ready flips to True -> must be a *different* timestamp.
    let status = bps("10.0.0.1", &running_status(), Some(&prev));
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True");
    assert_ne!(ready.last_transition_time, Some(old_time));
}

#[test]
fn each_condition_type_is_matched_independently() {
    // Initialized/PodScheduled are always true; ContainersReady/Ready track
    // `running`. A prev status with only some condition types present must
    // not cross-contaminate types it has no entry for.
    let old_time = fixed_time(60);
    let prev = PodStatus {
        conditions: Some(vec![PodCondition {
            type_: "Initialized".to_string(),
            status: "True".to_string(),
            last_transition_time: Some(old_time.clone()),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let status = bps("10.0.0.1", &running_status(), Some(&prev));
    let conds = status.conditions.as_ref().unwrap();
    let initialized = conds.iter().find(|c| c.type_ == "Initialized").unwrap();
    assert_eq!(initialized.last_transition_time, Some(old_time));
    // Ready has no prior entry to match — must still get a value, just a fresh one.
    let ready = conds.iter().find(|c| c.type_ == "Ready").unwrap();
    assert!(ready.last_transition_time.is_some());
}

#[test]
fn container_creating_reason_set_when_not_running() {
    let status = bps("10.0.0.1", &pending_status(), None);
    let cs = &status.container_statuses.as_ref().unwrap()[0];
    let waiting = cs.state.as_ref().unwrap().waiting.as_ref().expect("expected waiting state");
    assert_eq!(waiting.reason.as_deref(), Some("ContainerCreating"));
    assert!(cs.state.as_ref().unwrap().running.is_none());
}

#[test]
fn running_container_has_running_state_not_waiting() {
    let status = bps("10.0.0.1", &running_status(), None);
    let cs = &status.container_statuses.as_ref().unwrap()[0];
    assert!(cs.state.as_ref().unwrap().running.is_some());
    assert!(cs.state.as_ref().unwrap().waiting.is_none());
}

#[test]
fn host_ip_is_always_set_from_the_argument() {
    let status = bps("192.168.1.50", &running_status(), None);
    assert_eq!(status.host_ip.as_deref(), Some("192.168.1.50"));
}

#[test]
fn host_ips_plural_mirrors_the_singular_host_ip() {
    // Round 56: real kubelet always sets hostIPs alongside hostIP, even
    // on a single-stack node.
    let status = bps("192.168.1.50", &running_status(), None);
    let host_ips = status.host_ips.as_ref().unwrap();
    assert_eq!(host_ips.len(), 1);
    assert_eq!(host_ips[0].ip, "192.168.1.50");
}

#[test]
fn pod_ip_and_pod_ips_are_populated_when_present() {
    let status = bps("10.0.0.1", &running_status(), None);
    assert_eq!(status.pod_ip.as_deref(), Some("10.42.0.2"));
    assert_eq!(status.pod_ips.as_ref().unwrap()[0].ip, "10.42.0.2");
}

#[test]
fn pod_ip_absent_when_runtime_has_none() {
    let status = bps("10.0.0.1", &pending_status(), None);
    assert!(status.pod_ip.is_none());
    assert!(status.pod_ips.is_none());
}

#[test]
fn phase_string_matches_runtime_phase() {
    let status = bps("10.0.0.1", &running_status(), None);
    assert_eq!(status.phase.as_deref(), Some("Running"));
    let status = bps("10.0.0.1", &pending_status(), None);
    assert_eq!(status.phase.as_deref(), Some("Pending"));
}

#[test]
fn restart_count_mirrors_the_runtime_status() {
    // The runtime (CriRuntime) is the source of truth for restart counts —
    // build_pod_status() must just carry the value through, not zero it out.
    let mut running = running_status();
    running.containers[0].restart_count = 3;
    let status = bps("10.0.0.1", &running, None);
    assert_eq!(status.container_statuses.as_ref().unwrap()[0].restart_count, 3);
}

#[test]
fn zero_restarts_reports_zero() {
    let status = bps("10.0.0.1", &running_status(), None);
    assert_eq!(status.container_statuses.as_ref().unwrap()[0].restart_count, 0);
}

#[test]
fn image_id_mirrors_the_runtime_status() {
    // Round 52: CRI's own Container.image_ref (a digested image
    // reference) must be carried through to containerStatuses[].imageID,
    // not hardcoded empty.
    let mut running = running_status();
    running.containers[0].image_id = "docker.io/library/busybox@sha256:deadbeef".to_string();
    let status = bps("10.0.0.1", &running, None);
    assert_eq!(status.container_statuses.as_ref().unwrap()[0].image_id, "docker.io/library/busybox@sha256:deadbeef");
}

#[test]
fn empty_image_id_is_reported_as_empty_not_fabricated() {
    let status = bps("10.0.0.1", &running_status(), None);
    assert_eq!(status.container_statuses.as_ref().unwrap()[0].image_id, "");
}

#[test]
fn qos_class_is_reported_using_the_real_api_strings() {
    // Round 55: PodStatus.qosClass was never set at all before this;
    // eviction::qos_class() already computed the value internally for
    // eviction ranking (round 7) — this just surfaces it.
    for qos in [crate::eviction::QosClass::BestEffort, crate::eviction::QosClass::Burstable, crate::eviction::QosClass::Guaranteed] {
        let status = build_pod_status("10.0.0.1", "default", "app", &running_status(), None, &[], &probes::new_health_map(), qos, None);
        assert_eq!(status.qos_class.as_deref(), Some(qos.as_str()));
    }
}

#[test]
fn initialized_condition_mirrors_runtime_status_by_default_true() {
    // No init containers -> initialized: true (set by every RuntimeStatus
    // constructor for a pod with none) -> Initialized condition True.
    let status = bps("10.0.0.1", &running_status(), None);
    let initialized = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Initialized").unwrap();
    assert_eq!(initialized.status, "True");
}

#[test]
fn initialized_false_while_waiting_on_init_containers() {
    let mut waiting = pending_status();
    waiting.initialized = false;
    waiting.message = Some("waiting for init containers to complete".to_string());
    let status = bps("10.0.0.1", &waiting, None);
    let initialized = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Initialized").unwrap();
    assert_eq!(initialized.status, "False");
}

#[test]
fn init_container_statuses_is_none_when_there_are_no_init_containers() {
    let status = bps("10.0.0.1", &running_status(), None);
    assert!(status.init_container_statuses.is_none());
}

#[test]
fn init_container_statuses_reflects_runtime_init_containers() {
    let mut rt = pending_status();
    rt.init_containers = vec![ContainerRuntimeStatus {
        name: "setup".to_string(),
        image: "alpine".to_string(),
        ready: false,
        running: true,
        container_id: Some("init-1".to_string()),
        restart_count: 0,
        ..Default::default()
    }];
    let status = bps("10.0.0.1", &rt, None);
    let init_statuses = status.init_container_statuses.unwrap();
    assert_eq!(init_statuses.len(), 1);
    assert_eq!(init_statuses[0].name, "setup");
    assert!(init_statuses[0].state.as_ref().unwrap().running.is_some());
}

#[test]
fn completed_init_container_reports_waiting_podinitializing_not_running() {
    // A "not running" init container in the snapshot is either not-yet-
    // created or already exited — either way it must not claim to be
    // ContainerRunning.
    let mut rt = pending_status();
    rt.init_containers = vec![ContainerRuntimeStatus {
        name: "setup".to_string(),
        image: "alpine".to_string(),
        ready: false,
        running: false,
        container_id: None,
        restart_count: 0,
        ..Default::default()
    }];
    let status = bps("10.0.0.1", &rt, None);
    let init_statuses = status.init_container_statuses.unwrap();
    let waiting = init_statuses[0].state.as_ref().unwrap().waiting.as_ref().unwrap();
    assert_eq!(waiting.reason.as_deref(), Some("PodInitializing"));
}

#[test]
fn ephemeral_container_statuses_is_none_when_there_are_none() {
    let status = bps("10.0.0.1", &running_status(), None);
    assert!(status.ephemeral_container_statuses.is_none());
}

#[test]
fn ephemeral_container_statuses_reflects_runtime_ephemeral_containers() {
    let mut rt = running_status();
    rt.ephemeral_containers = vec![ContainerRuntimeStatus {
        name: "debugger".to_string(),
        image: "busybox".to_string(),
        ready: true,
        running: true,
        container_id: Some("debug-1".to_string()),
        restart_count: 0,
        ..Default::default()
    }];
    let status = bps("10.0.0.1", &rt, None);
    let statuses = status.ephemeral_container_statuses.unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].name, "debugger");
    assert!(statuses[0].state.as_ref().unwrap().running.is_some());
}

#[test]
fn exited_ephemeral_container_reports_terminated_not_waiting() {
    // Unlike init containers, a finished debug session isn't "still
    // initializing" — it's done. Must not reuse the PodInitializing waiting
    // reason init containers get.
    let mut rt = running_status();
    rt.ephemeral_containers = vec![ContainerRuntimeStatus {
        name: "debugger".to_string(),
        image: "busybox".to_string(),
        ready: false,
        running: false,
        container_id: Some("debug-1".to_string()),
        restart_count: 0,
        ..Default::default()
    }];
    let status = bps("10.0.0.1", &rt, None);
    let statuses = status.ephemeral_container_statuses.unwrap();
    assert!(statuses[0].state.as_ref().unwrap().terminated.is_some());
    assert!(statuses[0].state.as_ref().unwrap().waiting.is_none());
}

#[test]
fn ephemeral_containers_do_not_affect_containers_ready_or_pod_ready() {
    // A debug container that never comes up must not flip ContainersReady/
    // Ready to false for the whole pod — real kubelet excludes ephemeral
    // containers from readiness aggregation entirely.
    let mut rt = running_status();
    rt.ephemeral_containers = vec![ContainerRuntimeStatus {
        name: "debugger".to_string(),
        image: "busybox".to_string(),
        ready: false,
        running: false,
        container_id: None,
        restart_count: 0,
        ..Default::default()
    }];
    let status = bps("10.0.0.1", &rt, None);
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True");
}

// --- native sidecar containers (round 36) ---

fn sidecar(name: &str, running: bool) -> ContainerRuntimeStatus {
    ContainerRuntimeStatus {
        name: name.to_string(),
        image: "envoy".to_string(),
        ready: running,
        running,
        container_id: running.then(|| format!("{name}-id")),
        restart_count: 0,
        is_restartable_sidecar: true,
        ..Default::default()
    }
}

#[test]
fn a_regular_init_containers_readiness_never_affects_pod_ready() {
    let mut rt = running_status();
    rt.init_containers = vec![ContainerRuntimeStatus { name: "setup".to_string(), running: false, ..Default::default() }];
    let status = bps("10.0.0.1", &rt, None);
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True");
}

#[test]
fn a_running_sidecar_with_no_probe_supervisor_keeps_pod_ready_true() {
    // No probe supervisor tracking it (default HealthMap) means healthy by
    // default — matches how app containers with no probes behave too.
    let mut rt = running_status();
    rt.init_containers = vec![sidecar("proxy", true)];
    let status = bps("10.0.0.1", &rt, None);
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True");
}

#[test]
fn a_sidecar_failing_its_readiness_probe_flips_pod_ready_to_false() {
    let mut rt = running_status();
    rt.init_containers = vec![sidecar("proxy", true)];
    let health = probes::new_health_map();
    seed_health(&health, "proxy", false, true); // running, but not ready
    let status = bps_with_health("10.0.0.1", &rt, &health);
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "False");
    // And the sidecar's own initContainerStatuses entry reflects it too.
    let init_statuses = status.init_container_statuses.unwrap();
    assert!(!init_statuses[0].ready);
}

#[test]
fn a_sidecar_not_yet_started_is_not_ready_even_if_the_container_is_running() {
    let mut rt = running_status();
    rt.init_containers = vec![sidecar("proxy", true)];
    let health = probes::new_health_map();
    seed_health(&health, "proxy", true, false); // startup probe still pending
    let status = bps_with_health("10.0.0.1", &rt, &health);
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "False");
}

#[test]
fn a_sidecar_passing_its_probe_keeps_pod_ready_true() {
    let mut rt = running_status();
    rt.init_containers = vec![sidecar("proxy", true)];
    let health = probes::new_health_map();
    seed_health(&health, "proxy", true, true);
    let status = bps_with_health("10.0.0.1", &rt, &health);
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True");
}

#[test]
fn container_ready_mirrors_running() {
    let status = bps("10.0.0.1", &running_status(), None);
    assert!(status.container_statuses.as_ref().unwrap()[0].ready);
    let status = bps("10.0.0.1", &pending_status(), None);
    assert!(!status.container_statuses.as_ref().unwrap()[0].ready);
}

#[test]
fn container_id_is_carried_through() {
    let status = bps("10.0.0.1", &running_status(), None);
    assert_eq!(status.container_statuses.as_ref().unwrap()[0].container_id.as_deref(), Some("abc123"));
}

// --- readinessGates ---

fn foreign_condition(type_: &str, status: &str) -> PodCondition {
    PodCondition { type_: type_.to_string(), status: status.to_string(), ..Default::default() }
}

#[test]
fn no_readiness_gates_leaves_ready_governed_by_containers_only() {
    let status = bps("10.0.0.1", &running_status(), None);
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True");
}

#[test]
fn a_satisfied_readiness_gate_leaves_ready_true() {
    let prev = PodStatus { conditions: Some(vec![foreign_condition("www.example.com/feature", "True")]), ..Default::default() };
    let status = bps_with_gates("10.0.0.1", &running_status(), Some(&prev), &["www.example.com/feature".to_string()]);
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "True");
    // ContainersReady is unaffected by gates — still True on its own terms.
    let containers_ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "ContainersReady").unwrap();
    assert_eq!(containers_ready.status, "True");
}

#[test]
fn an_unsatisfied_readiness_gate_flips_ready_to_false_even_with_containers_ready() {
    let prev = PodStatus { conditions: Some(vec![foreign_condition("www.example.com/feature", "False")]), ..Default::default() };
    let status = bps_with_gates("10.0.0.1", &running_status(), Some(&prev), &["www.example.com/feature".to_string()]);
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "False");
    let containers_ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "ContainersReady").unwrap();
    assert_eq!(containers_ready.status, "True");
}

#[test]
fn a_readiness_gate_with_no_matching_condition_at_all_is_not_satisfied() {
    // No controller has reported this condition yet — must not default to
    // ready, matching upstream ("missing" counts the same as "False").
    let status = bps_with_gates("10.0.0.1", &running_status(), None, &["www.example.com/feature".to_string()]);
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "False");
}

#[test]
fn every_readiness_gate_must_be_satisfied_not_just_one() {
    let prev = PodStatus {
        conditions: Some(vec![foreign_condition("gate-a", "True"), foreign_condition("gate-b", "False")]),
        ..Default::default()
    };
    let status = bps_with_gates("10.0.0.1", &running_status(), Some(&prev), &["gate-a".to_string(), "gate-b".to_string()]);
    let ready = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "Ready").unwrap();
    assert_eq!(ready.status, "False");
}

#[test]
fn a_foreign_condition_is_carried_forward_into_the_new_conditions_array() {
    // Regression test for the JSON-Merge-Patch array-replace hazard: if this
    // condition isn't copied forward, nodelet's own next status write would
    // silently delete whatever an external controller set — including the
    // very condition a readinessGate is trying to read.
    let prev = PodStatus { conditions: Some(vec![foreign_condition("www.example.com/feature", "True")]), ..Default::default() };
    let status = bps("10.0.0.1", &running_status(), Some(&prev));
    let carried = status.conditions.as_ref().unwrap().iter().find(|c| c.type_ == "www.example.com/feature");
    assert!(carried.is_some());
    assert_eq!(carried.unwrap().status, "True");
}

#[test]
fn status_patch_sends_only_nodelet_owned_conditions() {
    // The apiserver must merge this strategic patch with the current status;
    // sending a stale foreign condition here would reintroduce the race where
    // nodelet overwrites an external readiness-gate update.
    let prev = PodStatus { conditions: Some(vec![foreign_condition("www.example.com/feature", "True")]), ..Default::default() };
    let status = bps("10.0.0.1", &running_status(), Some(&prev));
    let patch = nodelet_owned_status_patch(&status);
    let conditions = patch.conditions.as_ref().unwrap();
    assert!(conditions.iter().all(|condition| OWNED_CONDITION_TYPES.contains(&condition.type_.as_str())));
    assert!(!conditions.iter().any(|condition| condition.type_ == "www.example.com/feature"));
}

#[test]
fn status_patch_replaces_pod_ips_after_runtime_recreation() {
    let status = bps("10.0.0.1", &running_status(), None);
    let patch = strategic_status_patch(&status);
    let pod_ips = patch.get("podIPs").and_then(serde_json::Value::as_array).unwrap();

    let directive = pod_ips
        .first()
        .and_then(serde_json::Value::as_object)
        .and_then(|item| item.get("$patch"));
    assert_eq!(directive.and_then(serde_json::Value::as_str), Some("replace"));
    let ip = pod_ips
        .get(1)
        .and_then(serde_json::Value::as_object)
        .and_then(|item| item.get("ip"))
        .and_then(serde_json::Value::as_str);
    assert_eq!(ip, Some("10.42.0.2"));
}

#[test]
fn nodelet_owned_condition_types_are_not_duplicated_from_prev() {
    // prev's own "Ready"/"ContainersReady"/etc. entries must not also get
    // copied into `foreign_conditions` alongside the freshly computed ones.
    let prev = PodStatus {
        conditions: Some(vec![foreign_condition("Ready", "False"), foreign_condition("Initialized", "True")]),
        ..Default::default()
    };
    let status = bps("10.0.0.1", &running_status(), Some(&prev));
    let ready_entries: Vec<_> = status.conditions.as_ref().unwrap().iter().filter(|c| c.type_ == "Ready").collect();
    assert_eq!(ready_entries.len(), 1);
}

#[test]
fn readiness_gate_types_extracts_condition_types_from_the_pod_spec() {
    use k8s_openapi::api::core::v1::{PodReadinessGate, PodSpec};
    let pod = Pod {
        spec: Some(PodSpec {
            readiness_gates: Some(vec![
                PodReadinessGate { condition_type: "gate-a".to_string() },
                PodReadinessGate { condition_type: "gate-b".to_string() },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(readiness_gate_types(&pod), vec!["gate-a".to_string(), "gate-b".to_string()]);
}

#[test]
fn readiness_gate_types_is_empty_when_the_pod_has_none() {
    assert!(readiness_gate_types(&Pod::default()).is_empty());
}

// --- container_state (round 24: terminated state / termination message) ---

fn exited_container(name: &str, exit_code: i32, reason: &str, message: &str) -> ContainerRuntimeStatus {
    ContainerRuntimeStatus {
        name: name.to_string(),
        running: false,
        exit_code: Some(exit_code),
        reason: reason.to_string(),
        termination_message: message.to_string(),
        ..Default::default()
    }
}

#[test]
fn a_container_that_has_never_run_is_waiting_not_terminated() {
    let c = ContainerRuntimeStatus { name: "app".to_string(), running: false, exit_code: None, ..Default::default() };
    let state = container_state(&c, None, "ContainerCreating");
    assert!(state.waiting.is_some());
    assert!(state.terminated.is_none());
}

#[test]
fn an_exited_container_reports_terminated_with_its_exit_code() {
    let c = exited_container("app", 137, "OOMKilled", "");
    let state = container_state(&c, None, "ContainerCreating");
    let terminated = state.terminated.expect("expected terminated state");
    assert_eq!(terminated.exit_code, 137);
    assert_eq!(terminated.reason.as_deref(), Some("OOMKilled"));
}

#[test]
fn a_running_container_is_running_even_if_it_has_a_stale_exit_code() {
    // exit_code carries a *last* exit across restarts (real kubelet's own
    // lastState concept) — a currently-running container must report
    // Running regardless of what exit_code says about its previous life.
    let c = ContainerRuntimeStatus { name: "app".to_string(), running: true, exit_code: Some(1), ..Default::default() };
    let state = container_state(&c, None, "ContainerCreating");
    assert!(state.running.is_some());
    assert!(state.terminated.is_none());
}

#[test]
fn termination_message_populates_the_terminated_states_message_field() {
    let c = exited_container("app", 1, "Error", "boom: disk full");
    let state = container_state(&c, None, "ContainerCreating");
    assert_eq!(state.terminated.unwrap().message.as_deref(), Some("boom: disk full"));
}

#[test]
fn an_empty_termination_message_leaves_the_message_field_unset() {
    let c = exited_container("app", 0, "Completed", "");
    let state = container_state(&c, None, "ContainerCreating");
    assert!(state.terminated.unwrap().message.is_none());
}

// --- container_state / last_container_state (round 75: CrashLoopBackOff + lastState) ---

#[test]
fn waiting_reason_override_takes_priority_over_the_default_reason() {
    // A backing-off container has exit_code deliberately left None (see
    // status.rs) so it falls into the Waiting branch at all -- this
    // proves the override reason wins there instead of the caller's
    // default "ContainerCreating".
    let c = ContainerRuntimeStatus {
        name: "app".to_string(),
        running: false,
        exit_code: None,
        waiting_reason_override: Some("CrashLoopBackOff".to_string()),
        ..Default::default()
    };
    let state = container_state(&c, None, "ContainerCreating");
    assert_eq!(state.waiting.unwrap().reason.as_deref(), Some("CrashLoopBackOff"));
}

#[test]
fn no_override_falls_back_to_the_callers_default_reason() {
    let c = ContainerRuntimeStatus { name: "app".to_string(), running: false, exit_code: None, ..Default::default() };
    let state = container_state(&c, None, "PodInitializing");
    assert_eq!(state.waiting.unwrap().reason.as_deref(), Some("PodInitializing"));
}

#[test]
fn last_container_state_with_no_history_is_an_empty_state() {
    let state = last_container_state(None);
    assert!(state.terminated.is_none());
    assert!(state.waiting.is_none());
    assert!(state.running.is_none());
}

#[test]
fn last_container_state_reports_the_captured_terminated_details() {
    let info = crate::runtime::TerminatedInfo {
        container_id: Some("containerd://abc123".to_string()),
        exit_code: 1,
        reason: "Error".to_string(),
        finished_at: None,
        message: "boom".to_string(),
    };
    let state = last_container_state(Some(&info));
    let terminated = state.terminated.expect("expected terminated lastState");
    assert_eq!(terminated.exit_code, 1);
    assert_eq!(terminated.reason.as_deref(), Some("Error"));
    assert_eq!(terminated.message.as_deref(), Some("boom"));
    assert_eq!(terminated.container_id.as_deref(), Some("containerd://abc123"));
}

#[test]
fn last_container_state_leaves_an_empty_reason_or_message_unset() {
    let info = crate::runtime::TerminatedInfo { container_id: None, exit_code: 0, reason: String::new(), finished_at: None, message: String::new() };
    let state = last_container_state(Some(&info));
    let terminated = state.terminated.unwrap();
    assert!(terminated.reason.is_none());
    assert!(terminated.message.is_none());
}

#[test]
fn build_pod_status_surfaces_last_terminated_into_container_statuses_last_state() {
    // End-to-end: a currently-Running container that has a recorded
    // last_terminated (from an earlier restart) must show BOTH its live
    // Running state AND lastState.terminated with the earlier exit's
    // details -- proving build_pod_status() actually wires
    // ContainerRuntimeStatus.last_terminated through, not just
    // container_state()/last_container_state() in isolation.
    let mut rt = running_status();
    rt.containers[0].last_terminated = Some(crate::runtime::TerminatedInfo {
        container_id: Some("containerd://old".to_string()),
        exit_code: 137,
        reason: "OOMKilled".to_string(),
        finished_at: None,
        message: String::new(),
    });
    let status = bps("10.0.0.1", &rt, None);
    let cs = &status.container_statuses.unwrap()[0];
    assert!(cs.state.as_ref().unwrap().running.is_some(), "current state should still be Running");
    let last_terminated = cs.last_state.as_ref().unwrap().terminated.as_ref().expect("expected lastState.terminated");
    assert_eq!(last_terminated.exit_code, 137);
    assert_eq!(last_terminated.reason.as_deref(), Some("OOMKilled"));
}

#[test]
fn build_pod_status_leaves_last_state_unset_with_no_recorded_history() {
    let status = bps("10.0.0.1", &running_status(), None);
    let cs = &status.container_statuses.unwrap()[0];
    assert!(cs.last_state.is_none());
}

// --- allocated_resources_status_field (round 79: ResourceHealthStatus) ---

#[test]
fn empty_entries_produce_no_allocated_resources_status_field() {
    assert!(allocated_resources_status_field(&[]).is_none());
}

#[test]
fn single_device_produces_one_resource_status_with_one_health_entry() {
    let entries = vec![("nvidia.com/gpu".to_string(), "gpu-0".to_string(), "Healthy".to_string())];
    let out = allocated_resources_status_field(&entries).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].name, "nvidia.com/gpu");
    let resources = out[0].resources.as_ref().unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].resource_id, "gpu-0");
    assert_eq!(resources[0].health.as_deref(), Some("Healthy"));
}

#[test]
fn multiple_devices_for_the_same_resource_group_under_one_resource_status() {
    let entries = vec![
        ("nvidia.com/gpu".to_string(), "gpu-0".to_string(), "Healthy".to_string()),
        ("nvidia.com/gpu".to_string(), "gpu-1".to_string(), "Unhealthy".to_string()),
    ];
    let out = allocated_resources_status_field(&entries).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].resources.as_ref().unwrap().len(), 2);
}

#[test]
fn different_resources_produce_separate_resource_status_entries() {
    let entries = vec![
        ("nvidia.com/gpu".to_string(), "gpu-0".to_string(), "Healthy".to_string()),
        ("example.com/fpga".to_string(), "fpga-0".to_string(), "Unknown".to_string()),
    ];
    let out = allocated_resources_status_field(&entries).unwrap();
    assert_eq!(out.len(), 2);
    assert!(out.iter().any(|r| r.name == "nvidia.com/gpu"));
    assert!(out.iter().any(|r| r.name == "example.com/fpga"));
}

#[test]
fn build_pod_status_surfaces_allocated_resources_status_into_container_statuses() {
    let mut rt = running_status();
    rt.containers[0].allocated_resources_status = vec![("nvidia.com/gpu".to_string(), "gpu-0".to_string(), "Healthy".to_string())];
    let status = bps("10.0.0.1", &rt, None);
    let cs = &status.container_statuses.unwrap()[0];
    let ars = cs.allocated_resources_status.as_ref().expect("expected allocatedResourcesStatus");
    assert_eq!(ars[0].name, "nvidia.com/gpu");
}
