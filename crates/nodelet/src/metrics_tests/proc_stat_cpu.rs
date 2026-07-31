use super::*;

const SAMPLE: &str = "\
cpu  100 20 30 5000 10 1 2 3 0 0
cpu0 50 10 15 2500 5 0 1 1 0 0
cpu1 50 10 15 2500 5 1 1 2 0 0
intr 12345 0 0 0
ctxt 98765
btime 1700000000
";

#[test]
fn sums_the_busy_fields_from_the_aggregate_cpu_line() {
    // user(100) + nice(20) + system(30) + irq(1) + softirq(2) + steal(3) = 156 ticks
    let secs = parse_proc_stat_cpu_line(SAMPLE).unwrap();
    assert_eq!(secs, 156.0 / 100.0);
}

#[test]
fn idle_and_iowait_are_excluded() {
    // If idle (5000) or iowait (10) leaked in, this would be enormous.
    let secs = parse_proc_stat_cpu_line(SAMPLE).unwrap();
    assert!(secs < 10.0);
}

#[test]
fn per_core_lines_are_not_used_only_the_aggregate() {
    // cpu0/cpu1 alone would sum to the same total by coincidence in this
    // fixture, so use a fixture where they'd clearly diverge if picked up.
    let text = "cpu  1 0 0 0 0 0 0 0 0 0\ncpu0 999 999 999 999 999 999 999 999 0 0\n";
    assert_eq!(parse_proc_stat_cpu_line(text).unwrap(), 0.01);
}

#[test]
fn missing_cpu_line_returns_none() {
    assert!(parse_proc_stat_cpu_line("intr 12345\nctxt 98765\n").is_none());
}

#[test]
fn empty_input_returns_none() {
    assert!(parse_proc_stat_cpu_line("").is_none());
}

#[test]
fn short_line_with_only_the_first_few_fields_still_parses() {
    // An older/minimal kernel might not report irq/softirq/steal at all —
    // those should be treated as 0, not fail the whole parse.
    let secs = parse_proc_stat_cpu_line("cpu  100 0 50 200\n").unwrap();
    assert_eq!(secs, 150.0 / 100.0);
}

#[test]
fn real_proc_stat_parses_on_this_host() {
    let secs = read_node_cpu_seconds().expect("/proc/stat should parse on Linux CI");
    assert!(secs >= 0.0);
}
