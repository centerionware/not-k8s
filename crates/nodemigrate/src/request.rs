use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};

use crate::detect::Installation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Distribution {
    K3s,
    Kubernetes,
    Nodestore,
}

impl Distribution {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "k3s" => Ok(Self::K3s),
            "k8s" | "kubernetes" => Ok(Self::Kubernetes),
            "nodestore" => Ok(Self::Nodestore),
            other => {
                bail!("unsupported distribution '{other}' (expected k3s, kubernetes, or nodestore)")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationRequest {
    pub to: Distribution,
    pub from: Distribution,
    #[serde(default)]
    pub uninstall_after_migrate: bool,
    #[serde(default)]
    pub plan_only: bool,
}

impl MigrationRequest {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut to = None;
        let mut from = None;
        let mut uninstall_after_migrate = false;
        let mut plan_only = false;

        for raw in args {
            let (key, value) = raw
                .trim_start_matches("--")
                .split_once('=')
                .unwrap_or((raw, "true"));
            match key {
                "to" => set_once(&mut to, Distribution::parse(value)?, "to")?,
                "from" => set_once(&mut from, Distribution::parse(value)?, "from")?,
                "uninstall-after-migrate" => uninstall_after_migrate = parse_bool(value, key)?,
                "plan-only" | "dry-run" => plan_only = parse_bool(value, key)?,
                other => bail!("unknown option '{other}' (expected to=, from=, uninstall-after-migrate=, or plan-only=)"),
            }
        }

        let to = to.ok_or_else(|| anyhow::anyhow!("missing required option to=<distribution>"))?;
        let from =
            from.ok_or_else(|| anyhow::anyhow!("missing required option from=<distribution>"))?;
        ensure!(
            to != from,
            "source and destination distributions must differ"
        );
        ensure!(
            matches!(
                (from, to),
                (
                    Distribution::K3s | Distribution::Kubernetes,
                    Distribution::Nodestore
                ) | (
                    Distribution::Nodestore,
                    Distribution::K3s | Distribution::Kubernetes
                )
            ),
            "migration between the two Kubernetes distributions is not supported directly"
        );

        Ok(Self {
            to,
            from,
            uninstall_after_migrate,
            plan_only,
        })
    }

    pub fn validate_source(&self, installation: &Installation) -> Result<()> {
        ensure!(
            installation.distribution == self.from,
            "requested source {:?} does not match detected local installation {:?}",
            self.from,
            installation.distribution
        );
        Ok(())
    }
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> Result<()> {
    ensure!(
        slot.is_none(),
        "option '{name}' was provided more than once"
    );
    *slot = Some(value);
    Ok(())
}

fn parse_bool(value: &str, name: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("{name} must be true or false"),
    }
}

#[cfg(test)]
mod tests {
    use super::{Distribution, MigrationRequest};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn accepts_the_documented_k3s_to_nodestore_command_without_uninstalling() {
        let request = MigrationRequest::parse(&args(&["to=nodestore", "from=k3s"])).unwrap();
        assert_eq!(request.to, Distribution::Nodestore);
        assert_eq!(request.from, Distribution::K3s);
        assert!(!request.uninstall_after_migrate);
    }

    #[test]
    fn accepts_explicit_source_uninstall_flag() {
        let request = MigrationRequest::parse(&args(&[
            "to=nodestore",
            "from=k3s",
            "uninstall-after-migrate=true",
        ]))
        .unwrap();
        assert!(request.uninstall_after_migrate);
    }

    #[test]
    fn accepts_kubernetes_alias_and_reverse_migration() {
        let request = MigrationRequest::parse(&args(&["to=k8s", "from=nodestore"])).unwrap();
        assert_eq!(request.to, Distribution::Kubernetes);
        assert_eq!(request.from, Distribution::Nodestore);
    }

    #[test]
    fn plan_only_does_not_disable_or_uninstall_the_source() {
        let request =
            MigrationRequest::parse(&args(&["to=nodestore", "from=k3s", "plan-only=true"]))
                .unwrap();
        assert!(request.plan_only);
        assert!(!request.uninstall_after_migrate);
    }

    #[test]
    fn rejects_duplicate_options_and_invalid_boolean_values() {
        assert!(
            MigrationRequest::parse(&args(&["to=nodestore", "to=nodestore", "from=k3s"])).is_err()
        );
        assert!(MigrationRequest::parse(&args(&[
            "to=nodestore",
            "from=k3s",
            "uninstall-after-migrate=yes",
        ]))
        .is_err());
    }
}
