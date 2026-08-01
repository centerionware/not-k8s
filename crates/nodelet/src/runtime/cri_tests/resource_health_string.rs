//! resource_health_string(): DevicePlugins::health_of()'s Option<bool> ->
//! the ResourceHealth.health API string (round 79; ResourceHealthStatus,
//! found in round 72's re-audit) -- matches upstream's own 3 documented
//! values exactly.
use super::*;

#[test]
fn healthy_maps_to_the_healthy_string() {
    assert_eq!(resource_health_string(Some(true)), "Healthy");
}

#[test]
fn unhealthy_maps_to_the_unhealthy_string() {
    assert_eq!(resource_health_string(Some(false)), "Unhealthy");
}

#[test]
fn none_maps_to_unknown_not_a_guess_either_way() {
    // A device plugin that deregistered (or a device ID it no longer
    // reports) must never be silently assumed healthy or unhealthy.
    assert_eq!(resource_health_string(None), "Unknown");
}
