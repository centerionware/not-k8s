use super::*;

fn opts() -> LogOptions {
    LogOptions::default()
}

#[test]
fn simple_full_lines_pass_through_content_only() {
    let lines = ["2026-01-01T00:00:00.000000000Z stdout F line one", "2026-01-01T00:00:00.100000000Z stdout F line two"];
    let out = render_log_lines(&lines, &opts());
    assert_eq!(out, "line one\nline two\n");
}

#[test]
fn partial_lines_are_stitched_back_together() {
    let lines = [
        "2026-01-01T00:00:00.000000000Z stdout P hello ",
        "2026-01-01T00:00:00.100000000Z stdout P world",
        "2026-01-01T00:00:00.200000000Z stdout F !",
    ];
    let out = render_log_lines(&lines, &opts());
    assert_eq!(out, "hello world!\n");
}

#[test]
fn a_trailing_unclosed_partial_line_is_still_emitted() {
    // Still being written when read — real kubelet surfaces it too rather
    // than waiting forever for a closing F record.
    let lines = ["2026-01-01T00:00:00.000000000Z stdout P still going"];
    let out = render_log_lines(&lines, &opts());
    assert_eq!(out, "still going\n");
}

#[test]
fn malformed_lines_are_skipped_not_fatal() {
    let lines = ["not a valid cri log line", "2026-01-01T00:00:00.000000000Z stdout F real line"];
    let out = render_log_lines(&lines, &opts());
    assert_eq!(out, "real line\n");
}

#[test]
fn tail_lines_keeps_only_the_last_n() {
    let lines = [
        "2026-01-01T00:00:00.000000000Z stdout F one",
        "2026-01-01T00:00:00.100000000Z stdout F two",
        "2026-01-01T00:00:00.200000000Z stdout F three",
    ];
    let out = render_log_lines(&lines, &LogOptions { tail_lines: Some(2), ..opts() });
    assert_eq!(out, "two\nthree\n");
}

#[test]
fn tail_lines_larger_than_available_keeps_everything() {
    let lines = ["2026-01-01T00:00:00.000000000Z stdout F only"];
    let out = render_log_lines(&lines, &LogOptions { tail_lines: Some(50), ..opts() });
    assert_eq!(out, "only\n");
}

#[test]
fn since_filters_out_earlier_timestamps() {
    let lines = [
        "2026-01-01T00:00:00.000000000Z stdout F too-early",
        "2026-01-01T00:00:05.000000000Z stdout F right-on-time",
        "2026-01-01T00:00:10.000000000Z stdout F later",
    ];
    let out = render_log_lines(&lines, &LogOptions { since: Some("2026-01-01T00:00:05.000000000Z".to_string()), ..opts() });
    assert_eq!(out, "right-on-time\nlater\n");
}

#[test]
fn timestamps_option_prefixes_each_line() {
    let lines = ["2026-01-01T00:00:00.000000000Z stdout F hi"];
    let out = render_log_lines(&lines, &LogOptions { timestamps: true, ..opts() });
    assert_eq!(out, "2026-01-01T00:00:00.000000000Z hi\n");
}

#[test]
fn empty_input_produces_empty_output() {
    assert_eq!(render_log_lines(&[], &opts()), "");
}

#[test]
fn stdout_and_stderr_are_both_included_by_default_interleaved_in_file_order() {
    let lines = ["2026-01-01T00:00:00.000000000Z stdout F out-line", "2026-01-01T00:00:00.100000000Z stderr F err-line"];
    let out = render_log_lines(&lines, &opts());
    assert_eq!(out, "out-line\nerr-line\n");
}

#[test]
fn previous_log_path_appends_dot_one() {
    assert_eq!(previous_log_path("/var/log/pods/ns_name_uid/app_0.log"), "/var/log/pods/ns_name_uid/app_0.log.1");
}

#[test]
fn resolve_log_path_returns_the_live_path_when_not_previous() {
    assert_eq!(resolve_log_path("/var/log/x.log", false), "/var/log/x.log");
}

#[test]
fn resolve_log_path_returns_the_rotated_path_when_previous() {
    assert_eq!(resolve_log_path("/var/log/x.log", true), "/var/log/x.log.1");
}
