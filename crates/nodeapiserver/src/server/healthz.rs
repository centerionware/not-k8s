//! `/healthz`, `/readyz`, `/livez` — a faithful-but-scoped port of real
//! upstream's own `k8s.io/apiserver/pkg/server/healthz` package (fetched
//! and read directly): each path runs a small list of named checks and
//! renders real upstream's own exact response shape.
//!
//! **Checks ported**: `ping` (real upstream's own `PingHealthz` — always
//! passes; installed on every path, matching upstream's own "no checks
//! given -> install the ping check" default) on all three paths, plus
//! `storage` (this crate's own addition, `/readyz` only — a bounded live
//! read-only RPC against nodestore). Not ported: `log`
//! (klog-specific, meaningless for this crate's `tracing` output),
//! `informer-sync`, `shutdown` (no graceful-shutdown machinery in this
//! crate yet). The `?exclude=` query parameter is supported too: excluded
//! checks are reported as successful in verbose output, and unknown names
//! produce upstream-shaped warnings.
//!
//! **Response shape ported exactly** (`handleRootHealth`): no `verbose`
//! query param and every check passes -> bare `"ok"`. Any check fails ->
//! always-verbose `[+]<name> ok` / `[-]<name> failed: reason withheld`
//! per line (the real "don't leak the actual error to an unauthenticated
//! caller" posture) followed by `"<name> check failed"`, HTTP `500`. All
//! pass and `?verbose` is present -> the same per-line output followed by
//! `"<name> check passed\n"`, HTTP `200`. `Content-Type: text/plain;
//! charset=utf-8` and `X-Content-Type-Options: nosniff` on every
//! response, matching both real upstream's own success path and Go's
//! `http.Error` (used on its failure path) setting the same pair.

/// One named check's outcome — `name` mirrors real upstream's own
/// `HealthChecker.Name()`, `ok` its `Check(req) error` collapsed to a
/// boolean (this port never surfaces the real error text to the
/// response body, same as real upstream's own "reason withheld").
pub struct CheckResult {
    pub name: &'static str,
    pub ok: bool,
    /// Whether this check was skipped by the request's `exclude` parameter.
    pub excluded: bool,
}

/// Real upstream's own per-path check list, ported: `/healthz` and
/// `/livez` both just install the default `ping` check (upstream's own
/// `InstallHandler`/`InstallLivezHandler` are called with no explicit
/// checks in the common case); `/readyz` adds this crate's own `storage`
/// check. The caller supplies the result of its live probe. `excluded`
/// contains the requested check names, and the second
/// return value contains unknown names for the verbose warning output.
pub fn run_checks(path: &str, storage_connected: bool, excluded: &[String]) -> (Vec<CheckResult>, Vec<String>) {
    let mut checks = vec![CheckResult { name: "ping", ok: true, excluded: excluded.iter().any(|name| name == "ping") }];
    if path == "readyz" {
        checks.push(CheckResult { name: "storage", ok: storage_connected, excluded: excluded.iter().any(|name| name == "storage") });
    }
    let mut unknown = excluded
        .iter()
        .filter(|name| !checks.iter().any(|check| check.name == name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    unknown.sort();
    unknown.dedup();
    (checks, unknown)
}

/// Real upstream's own `handleRootHealth`, ported: the per-check
/// `individualCheckOutput` lines, then either the bare `"ok"`/verbose-
/// passed form or the always-verbose failed form. Returns `(status,
/// body)` — the pure half of the real handler, with header-setting left
/// to the caller (mirrors this crate's own pure-decision/real-I/O split
/// used throughout `admission`/`server`).
pub fn render(path: &str, checks: &[CheckResult], unknown_excluded: &[String], verbose: bool) -> (u16, String) {
    let mut individual = String::new();
    let mut any_failed = false;
    for check in checks {
        if check.excluded {
            individual.push_str(&format!("[+]{} excluded: ok\n", check.name));
        } else if check.ok {
            individual.push_str(&format!("[+]{} ok\n", check.name));
        } else {
            individual.push_str(&format!("[-]{} failed: reason withheld\n", check.name));
            any_failed = true;
        }
    }
    for name in unknown_excluded {
        individual.push_str(&format!("warn: some health checks cannot be excluded: no matches for {name:?}\n"));
    }
    if any_failed {
        return (500, format!("{individual}{path} check failed"));
    }
    if !verbose {
        return (200, "ok".to_string());
    }
    (200, format!("{individual}{path} check passed\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_checks_passing_without_verbose_is_bare_ok() {
        let (checks, unknown) = run_checks("healthz", true, &[]);
        let (status, body) = render("healthz", &checks, &unknown, false);
        assert_eq!(status, 200);
        assert_eq!(body, "ok");
    }

    #[test]
    fn all_checks_passing_with_verbose_lists_each_check() {
        let (checks, unknown) = run_checks("livez", true, &[]);
        let (status, body) = render("livez", &checks, &unknown, true);
        assert_eq!(status, 200);
        assert_eq!(body, "[+]ping ok\nlivez check passed\n");
    }

    #[test]
    fn readyz_adds_the_storage_check() {
        let (checks, unknown) = run_checks("readyz", true, &[]);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[1].name, "storage");
        assert!(unknown.is_empty());
    }

    #[test]
    fn healthz_and_livez_never_add_the_storage_check() {
        assert_eq!(run_checks("healthz", false, &[]).0.len(), 1);
        assert_eq!(run_checks("livez", false, &[]).0.len(), 1);
    }

    #[test]
    fn a_failed_check_is_always_verbose_and_withholds_the_reason() {
        let (checks, unknown) = run_checks("readyz", false, &[]);
        let (status, body) = render("readyz", &checks, &unknown, false);
        assert_eq!(status, 500);
        assert!(body.contains("[+]ping ok"));
        assert!(body.contains("[-]storage failed: reason withheld"));
        assert!(body.ends_with("readyz check failed"));
        // Real upstream never leaks the actual error text -- there is none
        // to leak here in the first place, but the message format itself
        // must not vary by verbose on failure.
        let (status_verbose, body_verbose) = render("readyz", &checks, &unknown, true);
        assert_eq!(status_verbose, 500);
        assert_eq!(body_verbose, body);
    }

    #[test]
    fn exclude_skips_a_named_check_and_warns_for_unknown_names() {
        let excluded = vec!["storage".to_string(), "missing".to_string(), "missing".to_string()];
        let (checks, unknown) = run_checks("readyz", false, &excluded);
        let (status, body) = render("readyz", &checks, &unknown, true);
        assert_eq!(status, 200);
        assert_eq!(unknown, ["missing"]);
        assert!(body.contains("[+]storage excluded: ok\n"));
        assert!(body.contains("warn: some health checks cannot be excluded: no matches for \"missing\"\n"));
        assert!(body.ends_with("readyz check passed\n"));
    }
}
