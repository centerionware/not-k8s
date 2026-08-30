//! Kubernetes audit-policy matching.
//!
//! This is the metadata-selection half of `audit.k8s.io/v1`: rules are
//! evaluated in order and the first matching rule wins. `None` suppresses
//! the event, while `Metadata`, `Request`, and `RequestResponse` select the
//! corresponding event level; the listener supplies bounded decoded objects
//! for the latter two when their bodies are supported.
//! Stage filtering follows the policy's global and first-matching-rule
//! `omitStages` values.

use crate::server::path::RequestInfo;
use serde::Deserialize;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    None,
    Metadata,
    Request,
    RequestResponse,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Metadata => "Metadata",
            Self::Request => "Request",
            Self::RequestResponse => "RequestResponse",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub level: Level,
    pub omit_response_complete: bool,
}

#[derive(Clone, Debug)]
pub struct AuditPolicy {
    rules: Vec<Rule>,
    omit_stages: Vec<String>,
}

#[derive(Clone, Debug)]
struct Rule {
    level: Level,
    users: Vec<String>,
    user_groups: Vec<String>,
    verbs: Vec<String>,
    resources: Vec<GroupResources>,
    namespaces: Vec<String>,
    non_resource_urls: Vec<String>,
    omit_stages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct RawPolicy {
    #[serde(rename = "apiVersion")]
    api_version: Option<String>,
    kind: Option<String>,
    #[serde(default)]
    rules: Vec<RawRule>,
    #[serde(rename = "omitStages", default)]
    omit_stages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct RawRule {
    level: String,
    #[serde(default)]
    users: Vec<String>,
    #[serde(rename = "userGroups", default)]
    user_groups: Vec<String>,
    #[serde(default)]
    verbs: Vec<String>,
    #[serde(default)]
    resources: Vec<RawGroupResources>,
    #[serde(default)]
    namespaces: Vec<String>,
    #[serde(rename = "nonResourceURLs", default)]
    non_resource_urls: Vec<String>,
    #[serde(rename = "omitStages", default)]
    omit_stages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct RawGroupResources {
    #[serde(default)]
    group: String,
    #[serde(default)]
    resources: Vec<String>,
    #[serde(rename = "resourceNames", default)]
    resource_names: Vec<String>,
}

#[derive(Clone, Debug)]
struct GroupResources {
    group: String,
    resources: Vec<String>,
    resource_names: Vec<String>,
}

impl AuditPolicy {
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let yaml = std::fs::read_to_string(path)
            .map_err(|error| format!("reading {}: {error}", path.display()))?;
        let raw: RawPolicy = serde_yaml::from_str(&yaml)
            .map_err(|error| format!("parsing {}: {error}", path.display()))?;
        if let Some(version) = raw.api_version.as_deref() {
            if version != "audit.k8s.io/v1" {
                return Err(format!(
                    "{} has unsupported apiVersion {version:?}",
                    path.display()
                ));
            }
        }
        if let Some(kind) = raw.kind.as_deref() {
            if kind != "Policy" {
                return Err(format!("{} has unsupported kind {kind:?}", path.display()));
            }
        }
        let rules = raw
            .rules
            .into_iter()
            .map(|rule| {
                let level = match rule.level.as_str() {
                    "None" => Level::None,
                    "Metadata" => Level::Metadata,
                    "Request" => Level::Request,
                    "RequestResponse" => Level::RequestResponse,
                    other => return Err(format!("unsupported audit level {other:?}")),
                };
                if !rule.resources.is_empty() && !rule.non_resource_urls.is_empty() {
                    return Err(
                        "an audit rule cannot specify both resources and nonResourceURLs"
                            .to_string(),
                    );
                }
                Ok(Rule {
                    level,
                    users: rule.users,
                    user_groups: rule.user_groups,
                    verbs: rule.verbs,
                    resources: rule
                        .resources
                        .into_iter()
                        .map(|resource| GroupResources {
                            group: resource.group,
                            resources: resource.resources,
                            resource_names: resource.resource_names,
                        })
                        .collect(),
                    namespaces: rule.namespaces,
                    non_resource_urls: rule.non_resource_urls,
                    omit_stages: rule.omit_stages,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            rules,
            omit_stages: raw.omit_stages,
        })
    }

    /// Select the first matching rule. A policy with no matching rule uses
    /// upstream's default `None` level.
    pub fn decide(&self, info: &RequestInfo, user: &str, groups: &[String]) -> Decision {
        let rule = self
            .rules
            .iter()
            .find(|rule| rule_matches(rule, info, user, groups));
        let Some(rule) = rule else {
            return Decision {
                level: Level::None,
                omit_response_complete: true,
            };
        };
        Decision {
            level: rule.level,
            omit_response_complete: self
                .omit_stages
                .iter()
                .any(|stage| stage == "ResponseComplete")
                || rule
                    .omit_stages
                    .iter()
                    .any(|stage| stage == "ResponseComplete"),
        }
    }

    pub fn should_emit_stage(
        &self,
        info: &RequestInfo,
        user: &str,
        groups: &[String],
        stage: &str,
    ) -> bool {
        let Some(rule) = self.rules.iter().find(|rule| rule_matches(rule, info, user, groups)) else {
            return false;
        };
        !matches!(rule.level, Level::None)
            && !self.omit_stages.iter().any(|omitted| omitted == stage)
            && !rule.omit_stages.iter().any(|omitted| omitted == stage)
    }

    pub fn should_emit_response_complete(
        &self,
        info: &RequestInfo,
        user: &str,
        groups: &[String],
    ) -> bool {
        self.should_emit_stage(info, user, groups, "ResponseComplete")
    }
}

fn rule_matches(rule: &Rule, info: &RequestInfo, user: &str, groups: &[String]) -> bool {
    if !matches_string(user, &rule.users)
        || (!rule.user_groups.is_empty()
            && !groups
                .iter()
                .any(|group| matches_string(group, &rule.user_groups)))
        || !matches_string(&info.verb, &rule.verbs)
        || (!rule.namespaces.is_empty()
            && !rule
                .namespaces
                .iter()
                .any(|namespace| namespace == "*" || namespace == &info.namespace))
    {
        return false;
    }
    if !rule.resources.is_empty() {
        return info.is_resource_request
            && rule
                .resources
                .iter()
                .any(|resource| resource_matches(resource, info));
    }
    if !rule.non_resource_urls.is_empty() {
        return !info.is_resource_request
            && rule
                .non_resource_urls
                .iter()
                .any(|pattern| url_matches(pattern, &info.path));
    }
    true
}

fn resource_matches(rule: &GroupResources, info: &RequestInfo) -> bool {
    if rule.group != "*" && rule.group != info.api_group {
        return false;
    }
    if !rule.resource_names.is_empty()
        && !rule
            .resource_names
            .iter()
            .any(|name| name == "*" || name == &info.name)
    {
        return false;
    }
    let target = if info.subresource.is_empty() {
        info.resource.clone()
    } else {
        format!("{}/{}", info.resource, info.subresource)
    };
    rule.resources.is_empty()
        || rule.resources.iter().any(|pattern| {
            resource_pattern_matches(pattern, &target, &info.resource, &info.subresource)
        })
}

fn resource_pattern_matches(
    pattern: &str,
    target: &str,
    resource: &str,
    subresource: &str,
) -> bool {
    pattern == "*"
        || pattern == target
        || (pattern
            .strip_suffix("/*")
            .is_some_and(|base| base == "*" || (base == resource && !subresource.is_empty())))
        || (pattern
            .strip_prefix("*/")
            .is_some_and(|suffix| suffix == subresource && !subresource.is_empty()))
}

fn matches_string(value: &str, values: &[String]) -> bool {
    values.is_empty()
        || values
            .iter()
            .any(|candidate| candidate == "*" || candidate == value)
}

fn url_matches(pattern: &str, path: &str) -> bool {
    if pattern == "*" || pattern == path {
        return true;
    }
    pattern
        .strip_suffix('*')
        .is_some_and(|prefix| path.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn info(path: &str) -> RequestInfo {
        crate::server::path::parse("GET", path, "")
    }

    fn policy(yaml: &str) -> AuditPolicy {
        let path = std::env::temp_dir().join(format!(
            "nodeapiserver-audit-policy-{}.yaml",
            uuid::Uuid::new_v4()
        ));
        fs::write(&path, yaml).unwrap();
        let parsed = AuditPolicy::from_file(&path).unwrap();
        fs::remove_file(path).unwrap();
        parsed
    }

    #[test]
    fn first_matching_rule_selects_none_or_metadata() {
        let policy = policy(
            "apiVersion: audit.k8s.io/v1\nkind: Policy\nrules:\n- level: None\n  verbs: [get]\n  resources:\n  - group: \"\"\n    resources: [secrets]\n- level: Metadata\n",
        );
        let groups = vec!["system:authenticated".to_string()];
        assert!(!policy.should_emit_response_complete(
            &info("/api/v1/namespaces/default/secrets/s1"),
            "alice",
            &groups
        ));
        assert!(policy.should_emit_response_complete(
            &info("/api/v1/namespaces/default/pods/p1"),
            "alice",
            &groups
        ));
    }

    #[test]
    fn user_group_and_non_resource_wildcards_match() {
        let policy = policy(
            "apiVersion: audit.k8s.io/v1\nkind: Policy\nrules:\n- level: None\n  userGroups: [system:authenticated]\n  nonResourceURLs: [/healthz*]\n- level: Metadata\n",
        );
        let groups = vec!["system:authenticated".to_string()];
        assert!(!policy.should_emit_response_complete(&info("/healthz/ready"), "alice", &groups));
        assert!(policy.should_emit_response_complete(&info("/version"), "alice", &groups));
    }

    #[test]
    fn policy_omit_stages_suppresses_the_existing_response_complete_event() {
        let policy = policy("apiVersion: audit.k8s.io/v1\nomitStages: [ResponseComplete]\nrules:\n- level: Metadata\n");
        assert!(!policy.should_emit_response_complete(&info("/version"), "alice", &[]));
    }

    #[test]
    fn policy_can_select_request_received_and_response_started_stages() {
        let policy = policy("apiVersion: audit.k8s.io/v1\nrules:\n- level: Metadata\n  omitStages: [ResponseStarted]\n");
        assert!(policy.should_emit_stage(&info("/version"), "alice", &[], "RequestReceived"));
        assert!(!policy.should_emit_stage(&info("/version"), "alice", &[], "ResponseStarted"));
        assert!(policy.should_emit_stage(&info("/version"), "alice", &[], "ResponseComplete"));
    }

    #[test]
    fn invalid_rule_level_is_rejected() {
        let path = std::env::temp_dir().join(format!(
            "nodeapiserver-audit-policy-invalid-{}.yaml",
            uuid::Uuid::new_v4()
        ));
        fs::write(&path, "rules:\n- level: Bogus\n").unwrap();
        let error = AuditPolicy::from_file(&path).unwrap_err();
        fs::remove_file(path).unwrap();
        assert!(error.contains("unsupported audit level"));
    }
}
