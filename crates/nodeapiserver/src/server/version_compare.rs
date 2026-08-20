//! `CompareKubeAwareVersionStrings` — a faithful port of
//! `staging/src/k8s.io/apimachinery/pkg/version/helpers.go` (`release-1.34`).
//! Sorts by maturity first (GA beats beta beats alpha), then major version,
//! then minor — the same order discovery's `preferredVersion` selection and
//! `kubectl`'s own client-side version picking both use.

use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum VersionType {
    Alpha,
    Beta,
    Ga,
}

/// `v1` -> `(Ga, 1, 0)`, `v1beta2` -> `(Beta, 1, 2)`, `v2alpha1` -> `(Alpha, 2, 1)`.
/// Tuple order is `(type, major, minor)`, matching upstream's own
/// comparison precedence exactly (type checked first, then major, then
/// minor) — **not** `(major, type, minor)`, which would let a higher major
/// version win over a more mature one (`v2alpha1` beating `v1` GA), the
/// opposite of upstream's documented "sorted based on GA/alpha/beta first
/// and then major and minor versions." `None` for anything that doesn't
/// match the `v<major>(alpha|beta<minor>)?` shape — matches upstream's own
/// regex (`^v([\d]+)(?:(alpha|beta)([\d]+))?$`) exactly, including that a
/// bare `v1alpha`/`v1beta` with no trailing digit does **not** match (the
/// minor version is mandatory once `alpha`/`beta` is present).
fn parse(v: &str) -> Option<(VersionType, u64, u64)> {
    let rest = v.strip_prefix('v')?;
    let digits_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    if digits_end == 0 {
        return None;
    }
    let major: u64 = rest[..digits_end].parse().ok()?;
    let suffix = &rest[digits_end..];
    if suffix.is_empty() {
        return Some((VersionType::Ga, major, 0));
    }
    let (vtype, minor_str) = if let Some(m) = suffix.strip_prefix("alpha") {
        (VersionType::Alpha, m)
    } else if let Some(m) = suffix.strip_prefix("beta") {
        (VersionType::Beta, m)
    } else {
        return None;
    };
    if minor_str.is_empty() || !minor_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let minor: u64 = minor_str.parse().ok()?;
    Some((vtype, major, minor))
}

/// `Ordering::Greater` means `a` is the more-preferred (higher-priority)
/// version — same direction upstream's own comparator returns (a positive
/// result means `v1` sorts after, i.e. wins over, `v1beta1`).
///
/// Two strings that both fail to parse fall back to a plain reverse
/// lexicographic compare, matching upstream's own `!ok1 && !ok2` branch
/// (`strings.Compare(v2, v1)` — note the swapped argument order, which is
/// what makes it *reverse* lexicographic) exactly rather than treating
/// that case as an error this crate has no error channel for yet.
pub fn compare_kube_aware_versions(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    match (parse(a), parse(b)) {
        (None, None) => b.cmp(a),
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(pa), Some(pb)) => pa.cmp(&pb),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upstream's own doc comment example, verbatim: "Versions will be
    /// sorted based on GA/alpha/beta first and then major and minor
    /// versions. e.g. v2, v1, v1beta2, v1beta1, v1alpha1."
    #[test]
    fn upstreams_own_documented_ordering_example() {
        let mut versions = vec!["v1alpha1", "v1beta1", "v1", "v1beta2", "v2"];
        versions.sort_by(|a, b| compare_kube_aware_versions(b, a));
        assert_eq!(versions, vec!["v2", "v1", "v1beta2", "v1beta1", "v1alpha1"]);
    }

    #[test]
    fn ga_beats_beta_beats_alpha_at_the_same_major_version() {
        assert_eq!(compare_kube_aware_versions("v1", "v1beta1"), Ordering::Greater);
        assert_eq!(compare_kube_aware_versions("v1beta1", "v1alpha1"), Ordering::Greater);
    }

    #[test]
    fn higher_major_version_wins_within_the_same_maturity() {
        assert_eq!(compare_kube_aware_versions("v2", "v1"), Ordering::Greater);
        assert_eq!(compare_kube_aware_versions("v2beta1", "v1beta1"), Ordering::Greater);
    }

    /// The bug a `(major, type, minor)` tuple order would produce: a higher
    /// major version at a *lower* maturity incorrectly outranking a lower
    /// major version at GA. Maturity must be compared before major version,
    /// exactly matching upstream's own precedence — caught by writing this
    /// cross-case explicitly rather than trusting the same-major-version
    /// examples in upstream's own doc comment to exercise it.
    #[test]
    fn maturity_beats_major_version_even_when_major_disagrees() {
        assert_eq!(compare_kube_aware_versions("v1", "v2alpha1"), Ordering::Greater, "GA v1 must beat alpha v2");
        assert_eq!(compare_kube_aware_versions("v1", "v9beta3"), Ordering::Greater, "GA v1 must beat beta v9");
    }

    #[test]
    fn higher_minor_version_wins_within_the_same_major_and_maturity() {
        assert_eq!(compare_kube_aware_versions("v1beta2", "v1beta1"), Ordering::Greater);
    }

    #[test]
    fn equal_strings_are_equal() {
        assert_eq!(compare_kube_aware_versions("v1", "v1"), Ordering::Equal);
    }

    #[test]
    fn unparseable_versions_fall_back_to_reverse_lexicographic() {
        // Matches upstream's strings.Compare(v2, v1) exactly: swapped
        // argument order versus the "normal" a.cmp(b).
        assert_eq!(compare_kube_aware_versions("zzz", "aaa"), "aaa".cmp("zzz"));
    }

    #[test]
    fn a_parseable_version_always_beats_an_unparseable_one() {
        assert_eq!(compare_kube_aware_versions("v1", "not-a-version"), Ordering::Greater);
        assert_eq!(compare_kube_aware_versions("not-a-version", "v1"), Ordering::Less);
    }

    #[test]
    fn alpha_or_beta_with_no_trailing_digit_does_not_parse() {
        // Matches the upstream regex exactly — the minor version is
        // mandatory once alpha/beta is present, not optional.
        assert_eq!(parse("v1alpha"), None);
        assert_eq!(parse("v1beta"), None);
        assert_eq!(parse("v1"), Some((VersionType::Ga, 1, 0)));
    }
}
