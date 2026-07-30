use super::*;

#[test]
fn extracts_the_token_from_a_well_formed_header() {
    assert_eq!(extract_bearer_token(Some("Bearer abc123")), Some("abc123"));
}

#[test]
fn missing_header_returns_none() {
    assert_eq!(extract_bearer_token(None), None);
}

#[test]
fn wrong_scheme_returns_none() {
    assert_eq!(extract_bearer_token(Some("Basic abc123")), None);
}

#[test]
fn empty_token_returns_none() {
    assert_eq!(extract_bearer_token(Some("Bearer ")), None);
}

#[test]
fn case_sensitive_scheme_prefix() {
    // Real bearer auth headers use exactly "Bearer " — being lenient here
    // would be a spec deviation, not a robustness win.
    assert_eq!(extract_bearer_token(Some("bearer abc123")), None);
}
