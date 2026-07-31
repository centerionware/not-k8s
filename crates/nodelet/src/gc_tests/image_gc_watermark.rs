//! disk_usage_percent()/should_start_image_gc()/images_to_reclaim_space()
//! (round 70; found in round 69's fresh gap re-audit) — real kubelet's
//! image-GC watermark policy: unreferenced images are left alone until
//! disk usage crosses the high threshold, then removed
//! oldest-unreferenced-first (only once old enough) until usage drops to
//! the low threshold or nothing eligible remains.
use super::*;
use std::collections::HashMap;

fn image(id: &str, size_bytes: u64) -> ImageRef {
    ImageRef { id: id.to_string(), size_bytes, ..Default::default() }
}

// --- disk_usage_percent() ---

#[test]
fn half_full_disk_is_50_percent() {
    assert_eq!(disk_usage_percent(1000, 500), 50);
}

#[test]
fn empty_disk_is_0_percent() {
    assert_eq!(disk_usage_percent(1000, 1000), 0);
}

#[test]
fn full_disk_is_100_percent() {
    assert_eq!(disk_usage_percent(1000, 0), 100);
}

#[test]
fn zero_total_bytes_is_0_percent_not_a_panic() {
    assert_eq!(disk_usage_percent(0, 0), 0);
}

#[test]
fn available_exceeding_total_clamps_to_0_percent_not_a_panic() {
    // Shouldn't happen in practice, but statvfs quirks/races are real.
    assert_eq!(disk_usage_percent(1000, 2000), 0);
}

// --- should_start_image_gc() ---

#[test]
fn usage_at_or_above_the_high_threshold_triggers_gc() {
    assert!(should_start_image_gc(85, 85));
    assert!(should_start_image_gc(90, 85));
}

#[test]
fn usage_below_the_high_threshold_does_not_trigger() {
    assert!(!should_start_image_gc(84, 85));
}

// --- images_to_reclaim_space() ---

#[test]
fn an_image_younger_than_min_age_is_not_eligible() {
    let candidates = vec![image("a", 100)];
    let since = HashMap::from([("a".to_string(), 100)]);
    // now=150, age=50s, min_age=120s -> not eligible yet.
    let out = images_to_reclaim_space(&candidates, &since, 150, 120, 1000, 200, 80);
    assert!(out.is_empty());
}

#[test]
fn an_image_missing_from_the_tracking_map_is_treated_as_not_yet_eligible() {
    let candidates = vec![image("a", 100)];
    let since = HashMap::new();
    let out = images_to_reclaim_space(&candidates, &since, 1000, 120, 1000, 200, 80);
    assert!(out.is_empty());
}

#[test]
fn removal_stops_once_usage_drops_to_the_low_threshold() {
    // total=1000, available=200 -> 80% used, already at the low
    // threshold, so nothing needs removing even though there's an
    // eligible candidate.
    let candidates = vec![image("a", 500)];
    let since = HashMap::from([("a".to_string(), 0)]);
    let out = images_to_reclaim_space(&candidates, &since, 1000, 120, 1000, 200, 80);
    assert!(out.is_empty());
}

#[test]
fn one_image_is_removed_when_that_alone_reaches_the_low_threshold() {
    // total=1000, available=100 -> 90% used. Removing a 200-byte image
    // brings available to 300 -> 70% used, under the 80% low threshold.
    let candidates = vec![image("a", 200)];
    let since = HashMap::from([("a".to_string(), 0)]);
    let out = images_to_reclaim_space(&candidates, &since, 1000, 120, 1000, 100, 80);
    assert_eq!(out, vec!["a".to_string()]);
}

#[test]
fn oldest_unreferenced_image_is_removed_first() {
    // total=1000, available=100 -> 90% used. Both eligible; "old" is
    // unreferenced longer, so it goes first even though it's smaller.
    let candidates = vec![image("new", 50), image("old", 10)];
    let since = HashMap::from([("new".to_string(), 900), ("old".to_string(), 0)]);
    let out = images_to_reclaim_space(&candidates, &since, 1000, 120, 1000, 100, 5);
    assert_eq!(out[0], "old");
}

#[test]
fn removal_continues_across_multiple_images_until_the_threshold_is_met() {
    // total=1000, available=50 -> 95% used. Neither image alone reaches
    // the 80% low threshold (need available >= 200); both together do.
    let candidates = vec![image("a", 100), image("b", 100)];
    let since = HashMap::from([("a".to_string(), 0), ("b".to_string(), 1)]);
    let out = images_to_reclaim_space(&candidates, &since, 1000, 120, 1000, 50, 80);
    assert_eq!(out.len(), 2);
}

#[test]
fn no_eligible_candidates_removes_nothing_even_under_pressure() {
    let candidates = vec![image("a", 500)];
    let since = HashMap::from([("a".to_string(), 999)]); // too young
    let out = images_to_reclaim_space(&candidates, &since, 1000, 120, 1000, 10, 80);
    assert!(out.is_empty());
}

#[test]
fn empty_candidates_removes_nothing() {
    let out = images_to_reclaim_space(&[], &HashMap::new(), 1000, 120, 1000, 10, 80);
    assert!(out.is_empty());
}
