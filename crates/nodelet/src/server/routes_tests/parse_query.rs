use super::*;

#[test]
fn parses_simple_key_value_pairs() {
    let pairs = parse_query("follow=true&tailLines=100");
    assert_eq!(pairs, vec![("follow".to_string(), "true".to_string()), ("tailLines".to_string(), "100".to_string())]);
}

#[test]
fn empty_query_produces_no_pairs() {
    assert!(parse_query("").is_empty());
}

#[test]
fn percent_decodes_values() {
    // A colon-containing sinceTime, as kubectl actually encodes it.
    let pairs = parse_query("sinceTime=2026-01-01T00%3A00%3A00Z");
    assert_eq!(query_value(&pairs, "sinceTime"), Some("2026-01-01T00:00:00Z"));
}

#[test]
fn repeated_keys_all_survive_in_order() {
    // kubectl encodes a multi-arg exec command as repeated `command=` pairs.
    let pairs = parse_query("command=sh&command=-c&command=echo+hi");
    assert_eq!(query_values(&pairs, "command"), vec!["sh", "-c", "echo hi"]);
}

#[test]
fn query_flag_recognizes_1_and_true() {
    let pairs = parse_query("a=1&b=true&c=0&d=false");
    assert!(query_flag(&pairs, "a"));
    assert!(query_flag(&pairs, "b"));
    assert!(!query_flag(&pairs, "c"));
    assert!(!query_flag(&pairs, "d"));
    assert!(!query_flag(&pairs, "missing"));
}

#[test]
fn query_value_returns_none_for_a_missing_key() {
    let pairs = parse_query("a=1");
    assert_eq!(query_value(&pairs, "b"), None);
}

#[test]
fn key_with_no_equals_sign_has_an_empty_value() {
    let pairs = parse_query("follow");
    assert_eq!(query_value(&pairs, "follow"), Some(""));
}
