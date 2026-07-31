//! parse_go_duration(): the narrow "12h"/"15m"/"90s" subset actually
//! used by CredentialProviderResponse.cacheDuration/defaultCacheDuration
//! in every real-world config upstream documents.
use super::*;

#[test]
fn parses_hours() {
    assert_eq!(parse_go_duration("12h"), Some(Duration::from_secs(12 * 3600)));
}

#[test]
fn parses_minutes() {
    assert_eq!(parse_go_duration("15m"), Some(Duration::from_secs(15 * 60)));
}

#[test]
fn parses_seconds() {
    assert_eq!(parse_go_duration("90s"), Some(Duration::from_secs(90)));
}

#[test]
fn trims_surrounding_whitespace() {
    assert_eq!(parse_go_duration("  5m  "), Some(Duration::from_secs(300)));
}

#[test]
fn compound_forms_are_not_supported_and_return_none() {
    // Not a general Go-duration parser -- "1h30m" isn't one of the
    // suffixes this checks, so it falls through to None rather than
    // misparsing it as something else.
    assert_eq!(parse_go_duration("1h30m"), None);
}

#[test]
fn garbage_returns_none_not_a_panic() {
    assert_eq!(parse_go_duration("not-a-duration"), None);
    assert_eq!(parse_go_duration(""), None);
    assert_eq!(parse_go_duration("h"), None);
}
