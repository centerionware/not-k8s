//! CredentialProviders::load(): config-file parsing, including the "not
//! configured at all" and "malformed YAML" edge cases.
use super::*;

#[test]
fn empty_path_means_the_feature_is_off_not_an_error() {
    let result = CredentialProviders::load("", "/some/bin/dir").unwrap();
    assert!(result.is_none());
}

#[test]
fn nonexistent_path_is_an_error() {
    let result = CredentialProviders::load("/nonexistent/path/to/config.yaml", "/bin");
    assert!(result.is_err());
}

#[test]
fn malformed_yaml_is_an_error_not_a_panic() {
    let dir = std::env::temp_dir().join(format!("nodelet-credprov-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bad.yaml");
    std::fs::write(&path, "not: [valid, yaml: structure").unwrap();

    let result = CredentialProviders::load(path.to_str().unwrap(), "/bin");
    assert!(result.is_err());

    std::fs::remove_file(&path).ok();
}

#[test]
fn valid_config_parses_providers_and_first_match_finds_the_right_one() {
    let dir = std::env::temp_dir().join(format!("nodelet-credprov-test-ok-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("good.yaml");
    std::fs::write(
        &path,
        r#"
providers:
  - name: ecr-provider
    matchImages:
      - "*.dkr.ecr.*.amazonaws.com"
    defaultCacheDuration: "12h"
  - name: gcr-provider
    matchImages:
      - "gcr.io"
    tokenAttributes:
      serviceAccountTokenAudience: "gcr.io"
      requireServiceAccount: true
"#,
    )
    .unwrap();

    let cp = CredentialProviders::load(path.to_str().unwrap(), "/bin").unwrap().expect("Some(..) for a configured file");

    let m = cp.first_match("123456789012.dkr.ecr.us-east-1.amazonaws.com/repo:tag").unwrap();
    assert_eq!(m.name, "ecr-provider");
    assert!(m.token_attributes.is_none());

    let m = cp.first_match("gcr.io/project/image:tag").unwrap();
    assert_eq!(m.name, "gcr-provider");
    assert!(m.token_attributes.as_ref().unwrap().require_service_account);

    assert!(cp.first_match("quay.io/other/image:tag").is_none());

    std::fs::remove_file(&path).ok();
}
