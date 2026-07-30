use super::*;

#[test]
fn parses_a_full_stdout_line() {
    let r = parse_log_line("2026-01-01T00:00:00.000000000Z stdout F hello world").unwrap();
    assert_eq!(r.timestamp, "2026-01-01T00:00:00.000000000Z");
    assert_eq!(r.stream, Stream::Stdout);
    assert!(!r.partial);
    assert_eq!(r.content, "hello world");
}

#[test]
fn parses_a_partial_stderr_line() {
    let r = parse_log_line("2026-01-01T00:00:00.000000000Z stderr P chunk-one").unwrap();
    assert_eq!(r.stream, Stream::Stderr);
    assert!(r.partial);
    assert_eq!(r.content, "chunk-one");
}

#[test]
fn content_containing_spaces_is_preserved_whole() {
    let r = parse_log_line("2026-01-01T00:00:00.000000000Z stdout F multiple  words  here").unwrap();
    assert_eq!(r.content, "multiple  words  here");
}

#[test]
fn empty_content_is_valid() {
    let r = parse_log_line("2026-01-01T00:00:00.000000000Z stdout F").unwrap();
    assert_eq!(r.content, "");
}

#[test]
fn unknown_stream_name_is_rejected() {
    assert!(parse_log_line("2026-01-01T00:00:00.000000000Z stdin F hello").is_none());
}

#[test]
fn unknown_tag_is_rejected() {
    assert!(parse_log_line("2026-01-01T00:00:00.000000000Z stdout X hello").is_none());
}

#[test]
fn too_few_fields_is_rejected() {
    assert!(parse_log_line("2026-01-01T00:00:00.000000000Z stdout").is_none());
    assert!(parse_log_line("").is_none());
}
