//! expand_command_arg(): `command`/`args`' `$(VAR)` expansion against a
//! container's own resolved env vars. Regression coverage for a bug found
//! live testing a real CSI driver — nodelet passed `$(CSI_ENDPOINT)` through
//! to `hostpathplugin` completely unexpanded, so its gRPC server tried to
//! bind a unix socket at the literal path `/$(CSI_ENDPOINT)` instead of
//! `unix:///csi/csi.sock`, never came up, and got killed and restarted by
//! its own liveness probe every ~10s forever.
use super::*;

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue { key: key.to_string(), value: value.as_bytes().to_vec() }
}

#[test]
fn no_references_passes_through_unchanged() {
    assert_eq!(expand_command_arg("--v=5", &[]), "--v=5");
}

#[test]
fn a_single_reference_is_substituted() {
    let envs = vec![kv("CSI_ENDPOINT", "unix:///csi/csi.sock")];
    assert_eq!(expand_command_arg("--endpoint=$(CSI_ENDPOINT)", &envs), "--endpoint=unix:///csi/csi.sock");
}

#[test]
fn multiple_references_in_one_string_are_all_substituted() {
    let envs = vec![kv("HOST", "10.0.0.5"), kv("PORT", "9898")];
    assert_eq!(expand_command_arg("http://$(HOST):$(PORT)/healthz", &envs), "http://10.0.0.5:9898/healthz");
}

#[test]
fn an_unresolved_reference_is_left_verbatim_not_dropped() {
    // Real kubelet's expandContainerCommandAndArgs() leaves an unknown
    // $(VAR) as literal text rather than failing the container — unlike
    // this file's sibling expand_sub_path_expr(), which fails the whole
    // volume mount instead. Must match kubelet's (lenient) behavior here.
    assert_eq!(expand_command_arg("--endpoint=$(NOT_SET)", &[]), "--endpoint=$(NOT_SET)");
}

#[test]
fn double_dollar_is_a_literal_dollar_not_a_reference() {
    let envs = vec![kv("FOO", "bar")];
    assert_eq!(expand_command_arg("price: $$5, not $(FOO)", &envs), "price: $5, not bar");
}

#[test]
fn unterminated_reference_is_kept_as_literal_text() {
    assert_eq!(expand_command_arg("--endpoint=$(CSI_ENDPOINT", &[]), "--endpoint=$(CSI_ENDPOINT");
}

#[test]
fn a_lone_dollar_with_no_paren_is_kept_literal() {
    assert_eq!(expand_command_arg("cost is $5", &[]), "cost is $5");
}
