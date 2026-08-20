//! `/healthz`, `/readyz`, `/livez` — a faithful-but-scoped port of real
//! upstream's own `k8s.io/apiserver/pkg/server/healthz` package (fetched
//! and read directly): each path runs a small list of named checks and
//! renders real upstream's own exact response shape.
//!
//! **Checks ported**: `ping` (real upstream's own `PingHealthz` — always
//! passes; installed on every path, matching upstream's own "no checks
//! given -> install the ping check" default) on all three paths, plus
//! `storage` (this crate's own addition, `/readyz` only — whether the
//! `StorageClient` connection this listener opened at startup is
//! present). **Named simplification**: `storage` reflects the one
//! connection attempt made at listener startup, not a live per-request
//! round trip to nodestore the way real upstream's own etcd health
//! checker actually pings on each call — a coarser but real signal
//! (matches `server::listener`'s own doc comment: storage is
//! "best-effort, `None` on failure" at startup). Not ported: `log`
//! (klog-specific, meaningless for this crate's `tracing` output),
//! `informer-sync`, `shutdown` (no graceful-shutdown machinery in this
//! crate yet), and the `?exclude=` query param that lets a caller skip
//! named checks.
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
}

/// Real upstream's own per-path check list, ported: `/healthz` and
/// `/livez` both just install the default `ping` check (upstream's own
/// `InstallHandler`/`InstallLivezHandler` are called with no explicit
/// checks in the common case); `/readyz` adds this crate's own `storage`
/// check (see this module's own doc comment for what it actually
/// reflects).
pub fn run_checks(path: &str, storage_connected: bool) -> Vec<CheckResult> {
    let mut checks = vec![CheckResult { name: "ping", ok: true }];
    if path == "readyz" {
        checks.push(CheckResult { name: "storage", ok: storage_connected });
    }
    checks
}

/// Real upstream's own `handleRootHealth`, ported: the per-check
/// `individualCheckOutput` lines, then either the bare `"ok"`/verbose-
/// passed form or the always-verbose failed form. Returns `(status,
/// body)` — the pure half of the real handler, with header-setting left
/// to the caller (mirrors this crate's own pure-decision/real-I/O split
/// used throughout `admission`/`server`).
pub fn render(path: &str, checks: &[CheckResult], verbose: bool) -> (u16, String) {
    let mut individual = String::new();
    let mut any_failed = false;
    for check in checks {
        if check.ok {
            individual.push_str(&format!("[+]{} ok\n", check.name));
        } else {
            individual.push_str(&format!("[-]{} failed: reason withheld\n", check.name));
            any_failed = true;
        }
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
        let checks = run_checks("healthz", true);
        let (status, body) = render("healthz", &checks, false);
        assert_eq!(status, 200);
        assert_eq!(body, "ok");
    }

    #[test]
    fn all_checks_passing_with_verbose_lists_each_check() {
        let checks = run_checks("livez", true);
        let (status, body) = render("livez", &checks, true);
        assert_eq!(status, 200);
        assert_eq!(body, "[+]ping ok\nlivez check passed\n");
    }

    #[test]
    fn readyz_adds_the_storage_check() {
        let checks = run_checks("readyz", true);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[1].name, "storage");
    }

    #[test]
    fn healthz_and_livez_never_add_the_storage_check() {
        assert_eq!(run_checks("healthz", false).len(), 1);
        assert_eq!(run_checks("livez", false).len(), 1);
    }

    #[test]
    fn a_failed_check_is_always_verbose_and_withholds_the_reason() {
        let checks = run_checks("readyz", false);
        let (status, body) = render("readyz", &checks, false);
        assert_eq!(status, 500);
        assert!(body.contains("[+]ping ok"));
        assert!(body.contains("[-]storage failed: reason withheld"));
        assert!(body.ends_with("readyz check failed"));
        // Real upstream never leaks the actual error text -- there is none
        // to leak here in the first place, but the message format itself
        // must not vary by verbose on failure.
        let (status_verbose, body_verbose) = render("readyz", &checks, true);
        assert_eq!(status_verbose, 500);
        assert_eq!(body_verbose, body);
    }
}
