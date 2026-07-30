//! pod_service_account_name()/token_audiences(): the two pure pieces around
//! serviceAccountToken projected-volume minting (the actual TokenRequest
//! HTTP call needs a live apiserver and isn't unit-tested here).
use super::*;
use k8s_openapi::api::core::v1::PodSpec;

#[test]
fn unset_service_account_defaults_to_default() {
    let pod = Pod { spec: Some(PodSpec::default()), ..Default::default() };
    assert_eq!(pod_service_account_name(&pod), "default");
}

#[test]
fn empty_string_service_account_also_defaults() {
    let pod = Pod {
        spec: Some(PodSpec { service_account_name: Some(String::new()), ..Default::default() }),
        ..Default::default()
    };
    assert_eq!(pod_service_account_name(&pod), "default");
}

#[test]
fn explicit_service_account_is_used() {
    let pod = Pod {
        spec: Some(PodSpec { service_account_name: Some("my-sa".to_string()), ..Default::default() }),
        ..Default::default()
    };
    assert_eq!(pod_service_account_name(&pod), "my-sa");
}

#[test]
fn no_spec_at_all_defaults_to_default() {
    assert_eq!(pod_service_account_name(&Pod::default()), "default");
}

#[test]
fn no_audience_produces_an_empty_vec() {
    assert_eq!(token_audiences(None), Vec::<String>::new());
}

#[test]
fn empty_audience_string_also_produces_an_empty_vec() {
    assert_eq!(token_audiences(Some("")), Vec::<String>::new());
}

#[test]
fn explicit_audience_produces_a_single_element_vec() {
    assert_eq!(token_audiences(Some("api")), vec!["api".to_string()]);
}
