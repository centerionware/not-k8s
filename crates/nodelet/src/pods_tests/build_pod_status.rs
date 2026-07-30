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
    build_pod_status(host_ip, "default", "app", rt, prev, &probes::new_health_map())
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
        }],
        init_containers: Vec::new(),
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
        }],
        init_containers: Vec::new(),
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

    write_status(&client, "10.0.0.1", "default", "app", &runtime, Some(&previous), &probes::new_health_map())
        .await
        .unwrap();

    assert_eq!(patches.load(Ordering::Relaxed), 0);
}

#[test]
fn server_owned_status_fields_do_not_force_a_patch() {
    let runtime = running_status();
    let mut previous = bps("10.0.0.1", &runtime, None);
    // The apiserver fills this field, but nodelet deliberately does not own
    // it. A server-owned field must not make every later reconcile look dirty.
    previous.qos_class = Some("Burstable".to_string());
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
        &probes::new_health_map(),
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
    }];
    let status = bps("10.0.0.1", &rt, None);
    let init_statuses = status.init_container_statuses.unwrap();
    let waiting = init_statuses[0].state.as_ref().unwrap().waiting.as_ref().unwrap();
    assert_eq!(waiting.reason.as_deref(), Some("PodInitializing"));
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
