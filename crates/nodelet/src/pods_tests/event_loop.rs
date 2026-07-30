//! Runtime-event channel behavior.
//!
//! A closed channel must park the runtime-event branch, and duplicate keys
//! must remain ordinary events rather than causing a hidden busy loop in the
//! receiver itself. These tests isolate the controller's event plumbing from
//! the apiserver and CRI so an event-source regression is obvious.

use super::*;
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;

#[tokio::test]
async fn runtime_event_yields_the_pod_key_unchanged() {
    let (tx, rx) = unbounded_channel();
    tx.send("kube-system/coredns-abc".to_string()).unwrap();

    let mut events = Some(rx);
    assert_eq!(next_event(&mut events).await, "kube-system/coredns-abc");
}

#[tokio::test]
async fn closed_runtime_event_channel_is_parked_instead_of_spinning() {
    let (tx, rx) = unbounded_channel::<String>();
    drop(tx);
    let mut events = Some(rx);

    // next_event() consumes the close, clears the receiver, then parks. If a
    // future change returned immediately here, the select! loop would spin at
    // 100% CPU even with no runtime events.
    let result = tokio::time::timeout(Duration::from_millis(50), next_event(&mut events)).await;
    assert!(
        result.is_err(),
        "closed runtime channel must not return repeatedly"
    );
    assert!(
        events.is_none(),
        "closed receiver should be removed from the select loop"
    );
}
