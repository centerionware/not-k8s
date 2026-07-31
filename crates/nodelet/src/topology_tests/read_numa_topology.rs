use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir() -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nodelet-topology-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_node(root: &Path, id: u32, cpulist: &str) {
    let node_dir = root.join(format!("node{id}"));
    std::fs::create_dir_all(&node_dir).unwrap();
    std::fs::write(node_dir.join("cpulist"), cpulist).unwrap();
}

#[test]
fn nonexistent_root_returns_an_empty_map_not_an_error() {
    let root = std::env::temp_dir().join("nodelet-topology-test-does-not-exist");
    assert!(read_numa_topology(&root).is_empty());
}

#[test]
fn single_numa_node() {
    let root = scratch_dir();
    write_node(&root, 0, "0-3");
    let topo = read_numa_topology(&root);
    assert_eq!(topo.get(&0), Some(&[0, 1, 2, 3].into_iter().collect()));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn multiple_numa_nodes() {
    let root = scratch_dir();
    write_node(&root, 0, "0-3");
    write_node(&root, 1, "4-7");
    let topo = read_numa_topology(&root);
    assert_eq!(topo.len(), 2);
    assert_eq!(topo.get(&0), Some(&[0, 1, 2, 3].into_iter().collect()));
    assert_eq!(topo.get(&1), Some(&[4, 5, 6, 7].into_iter().collect()));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn non_node_directories_are_ignored() {
    let root = scratch_dir();
    write_node(&root, 0, "0-3");
    std::fs::create_dir_all(root.join("has_cpu")).unwrap(); // a real sibling entry under /sys/devices/system/node
    let topo = read_numa_topology(&root);
    assert_eq!(topo.len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn real_sys_devices_system_node_parses_on_this_host_if_present() {
    // Sanity check against the real path this ships against in
    // production, same pattern metrics.rs's real /proc/meminfo test uses
    // — best-effort, since this sandbox may or may not expose NUMA info.
    // A node entry, if any are found, must never own an empty CPU set —
    // that would mean the cpulist file existed but parsed to nothing.
    let topo = read_numa_topology(Path::new("/sys/devices/system/node"));
    for cpus in topo.values() {
        assert!(!cpus.is_empty());
    }
}
