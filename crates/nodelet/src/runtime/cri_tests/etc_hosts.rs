//! write_etc_hosts(): before this, hostAliases had no effect at all —
//! nothing generated a hosts file or mounted it into the container.
use super::*;
use k8s_openapi::api::core::v1::HostAlias;

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nodelet-test-etchosts-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&dir);
    dir
}

#[test]
fn always_includes_localhost_entries() {
    let path = tmp("localhost");
    write_etc_hosts(&path, &[]).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("127.0.0.1\tlocalhost"));
    assert!(content.contains("::1\tlocalhost"));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn host_aliases_are_appended() {
    let path = tmp("aliases");
    let aliases = vec![HostAlias { ip: "10.0.0.5".to_string(), hostnames: Some(vec!["foo.local".to_string(), "bar.local".to_string()]) }];
    write_etc_hosts(&path, &aliases).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("10.0.0.5\tfoo.local bar.local"));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn alias_with_no_hostnames_is_skipped() {
    let path = tmp("no-hostnames");
    let aliases = vec![HostAlias { ip: "10.0.0.9".to_string(), hostnames: None }];
    write_etc_hosts(&path, &aliases).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(!content.contains("10.0.0.9"));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn multiple_aliases_each_get_their_own_line() {
    let path = tmp("multi");
    let aliases = vec![
        HostAlias { ip: "10.0.0.1".to_string(), hostnames: Some(vec!["a.local".to_string()]) },
        HostAlias { ip: "10.0.0.2".to_string(), hostnames: Some(vec!["b.local".to_string()]) },
    ];
    write_etc_hosts(&path, &aliases).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("10.0.0.1\ta.local"));
    assert!(content.contains("10.0.0.2\tb.local"));
    std::fs::remove_file(&path).unwrap();
}
