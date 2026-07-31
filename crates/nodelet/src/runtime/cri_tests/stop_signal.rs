//! stop_signal_cri()/stop_signal_k8s(): the pure translation behind
//! `lifecycle.stopSignal`/`containerStatuses[].stopSignal` (round 66; GA
//! 1.33, found in a fresh gap re-audit) — k8s's plain POSIX signal name
//! spelling <-> CRI's own `Signal` enum, which strips the shared
//! `SIGNAL_` prefix and can't spell `+`/`-` inside an identifier.
use super::*;

#[test]
fn a_plain_signal_name_translates_to_the_matching_cri_enum_value() {
    assert_eq!(stop_signal_cri("SIGTERM"), Some(v1::Signal::Sigterm as i32));
    assert_eq!(stop_signal_cri("SIGKILL"), Some(v1::Signal::Sigkill as i32));
    assert_eq!(stop_signal_cri("SIGHUP"), Some(v1::Signal::Sighup as i32));
}

#[test]
fn a_realtime_signal_with_a_literal_plus_translates_correctly() {
    assert_eq!(stop_signal_cri("SIGRTMIN+5"), Some(v1::Signal::Sigrtminplus5 as i32));
    assert_eq!(stop_signal_cri("SIGRTMAX-3"), Some(v1::Signal::Sigrtmaxminus3 as i32));
}

#[test]
fn an_already_word_spelled_realtime_signal_also_translates() {
    // Defensive: if the k8s API ever spelled these with words instead of
    // symbols, the replace() calls are no-ops and this still matches.
    assert_eq!(stop_signal_cri("SIGRTMINPLUS5"), Some(v1::Signal::Sigrtminplus5 as i32));
}

#[test]
fn an_unrecognized_signal_name_returns_none() {
    assert_eq!(stop_signal_cri("NOTASIGNAL"), None);
    assert_eq!(stop_signal_cri(""), None);
}

#[test]
fn stop_signal_k8s_is_the_exact_inverse_of_stop_signal_cri() {
    for name in ["SIGTERM", "SIGKILL", "SIGHUP", "SIGUSR1", "SIGRTMIN+5", "SIGRTMAX-3"] {
        let cri = stop_signal_cri(name).unwrap();
        assert_eq!(stop_signal_k8s(cri), Some(name.to_string()), "round-trip failed for {name}");
    }
}

#[test]
fn runtime_default_maps_to_none_not_a_sentinel_string() {
    assert_eq!(stop_signal_k8s(v1::Signal::RuntimeDefault as i32), None);
}

#[test]
fn an_out_of_range_cri_value_returns_none() {
    assert_eq!(stop_signal_k8s(9999), None);
}
