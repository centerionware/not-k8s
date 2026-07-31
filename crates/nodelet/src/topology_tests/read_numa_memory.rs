use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir() -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nodelet-topology-mem-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_meminfo(root: &Path, id: u32, mem_total_kb: u64) {
    let node_dir = root.join(format!("node{id}"));
    std::fs::create_dir_all(&node_dir).unwrap();
    let content = format!(
        "Node {id} MemTotal:       {mem_total_kb} kB\nNode {id} MemFree:         1000 kB\nNode {id} MemUsed:        {mem_total_kb} kB\n"
    );
    std::fs::write(node_dir.join("meminfo"), content).unwrap();
}

#[test]
fn parse_node_mem_total_reads_the_first_memtotal_line() {
    let content = "Node 0 MemTotal:       23498076 kB\nNode 0 MemFree:         1197068 kB\n";
    assert_eq!(parse_node_mem_total(content), Some(23498076 * 1024));
}

#[test]
fn parse_node_mem_total_returns_none_for_garbage() {
    assert_eq!(parse_node_mem_total("nothing useful here"), None);
}

#[test]
fn nonexistent_root_returns_an_empty_map() {
    let root = std::env::temp_dir().join("nodelet-topology-mem-test-does-not-exist");
    assert!(read_numa_memory(&root).is_empty());
}

#[test]
fn single_numa_node_memory() {
    let root = scratch_dir();
    write_meminfo(&root, 0, 8_000_000);
    let mem = read_numa_memory(&root);
    assert_eq!(mem.get(&0), Some(&(8_000_000 * 1024)));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn multiple_numa_nodes_memory() {
    let root = scratch_dir();
    write_meminfo(&root, 0, 8_000_000);
    write_meminfo(&root, 1, 4_000_000);
    let mem = read_numa_memory(&root);
    assert_eq!(mem.len(), 2);
    assert_eq!(mem.get(&0), Some(&(8_000_000 * 1024)));
    assert_eq!(mem.get(&1), Some(&(4_000_000 * 1024)));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn real_sys_devices_system_node_parses_on_this_host_if_present() {
    let mem = read_numa_memory(Path::new("/sys/devices/system/node"));
    for bytes in mem.values() {
        assert!(*bytes > 0);
    }
}
