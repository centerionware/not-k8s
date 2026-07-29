//! End-to-end smoke test of the CRI runtime against a real containerd.
//!
//! Drives the exact code path the node agent uses — `CriRuntime::ensure_pod`
//! (RunPodSandbox → PullImage → CreateContainer → StartContainer), then
//! `status`, then `remove_pod` — against a live CRI socket. No apiserver needed.
//!
//! Usage (must reach the socket; usually root):
//!   sudo ./target/debug/examples/cri_smoke [SOCKET] [IMAGE]
//! Defaults: unix:///run/containerd/containerd.sock  docker.io/library/busybox:latest
//!
//! Uses a hostNetwork pod so no CNI is required.

use anyhow::{bail, Context, Result};
use k8s_openapi::api::core::v1::Pod;
use nodelet::runtime::{Phase, PodRuntime};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let mut args = std::env::args().skip(1);
    let socket = args.next().unwrap_or_else(|| "unix:///run/containerd/containerd.sock".to_string());
    let image = args.next().unwrap_or_else(|| "docker.io/library/busybox:latest".to_string());

    println!("== connecting to {socket}");
    let rt = nodelet::runtime::cri::CriRuntime::connect(&socket)
        .await
        .context("connect to containerd")?;

    // Drain the runtime's event channel (the event-driven status path). With
    // containerd < 1.7 this is fed by the native Events/Subscribe fallback.
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    if let Some(mut ev_rx) = rt.take_event_rx() {
        let sink = events.clone();
        tokio::spawn(async move {
            while let Some(key) = ev_rx.recv().await {
                println!("   [event] runtime signaled change for {key}");
                sink.lock().unwrap().push(key);
            }
        });
    }

    let ns = "default";
    let name = "cri-smoke";
    let pod: Pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": name, "namespace": ns, "uid": "cri-smoke-uid-0001" },
        "spec": {
            "hostNetwork": true,
            "containers": [{
                "name": "app",
                "image": image,
                "command": ["sleep", "3600"]
            }]
        }
    }))?;

    println!("== ensure_pod (create sandbox + run container)");
    let st = rt.ensure_pod(&pod).await.context("ensure_pod")?;
    println!("   initial phase: {}", st.phase.as_str());

    // Poll status until the container is Running (it must really start under runc).
    println!("== waiting for Running...");
    let mut final_status = st;
    let mut became_running = false;
    for i in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Some(s) = rt.status(ns, name).await.context("status")? {
            println!(
                "   [{i:>2}s] phase={} containers={} ip={:?}",
                s.phase.as_str(),
                s.containers.len(),
                s.pod_ip
            );
            final_status = s;
            if final_status.phase == Phase::Running
                && final_status.containers.iter().any(|c| c.running)
            {
                became_running = true;
                break;
            }
        }
    }

    println!("== final status");
    for c in &final_status.containers {
        println!(
            "   container name={} image={} running={} id={:?}",
            c.name, c.image, c.running, c.container_id
        );
    }

    // Independent verification via the real CRI: ensure_pod is idempotent.
    println!("== ensure_pod again (must be idempotent, no duplicate container)");
    let again = rt.ensure_pod(&pod).await.context("ensure_pod #2")?;
    let count_after = again.containers.len();
    println!("   container count after 2nd ensure: {count_after}");

    // Give the event firehose a moment to deliver task-start events before teardown.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let event_keys = events.lock().unwrap().clone();
    let saw_our_pod = event_keys.iter().any(|k| k == &format!("{ns}/{name}"));
    println!(
        "== event channel: {} event(s), saw our pod = {saw_our_pod}",
        event_keys.len()
    );

    println!("== remove_pod (teardown sandbox + container)");
    rt.remove_pod(ns, name).await.context("remove_pod")?;
    let gone = rt.status(ns, name).await.context("status after remove")?;
    println!("   status after remove: {}", if gone.is_none() { "None (gone)" } else { "STILL PRESENT" });

    // Verdict.
    let mut ok = true;
    if !became_running {
        eprintln!("FAIL: container never reached Running");
        ok = false;
    }
    if count_after != 1 {
        eprintln!("FAIL: expected exactly 1 container after idempotent re-ensure, got {count_after}");
        ok = false;
    }
    if gone.is_some() {
        eprintln!("FAIL: pod still present after remove_pod");
        ok = false;
    }
    if !saw_our_pod {
        eprintln!("FAIL: event channel never delivered an event for our pod (event-driven path broken)");
        ok = false;
    }

    if ok {
        println!("\nPASS: CRI runtime validated end-to-end against containerd");
        Ok(())
    } else {
        bail!("CRI smoke test failed");
    }
}
