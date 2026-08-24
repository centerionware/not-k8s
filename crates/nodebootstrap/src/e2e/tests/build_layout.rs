use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

fn binary_dir() -> Result<PathBuf> {
    Ok(crate::config::Config::from_env()?.toolchain_dir().join("bin"))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return path.is_file()
            && std::fs::metadata(path)
                .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0);
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn combined_binary() -> Result<Option<PathBuf>> {
    let path = binary_dir()?.join("notk8s");
    Ok(is_executable(&path).then_some(path))
}

fn run_binary(path: &Path, args: &[&str]) -> Result<Output> {
    std::process::Command::new(path)
        .args(args)
        .output()
        .with_context(|| format!("running {} {:?}", path.display(), args))
}

fn component_names(output: &Output) -> Result<Vec<String>> {
    anyhow::ensure!(
        output.status.success(),
        "{} exited with {}: {}",
        "notk8s components",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(std::str::from_utf8(&output.stdout)
        .context("notk8s components output was not UTF-8")?
        .lines()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect())
}

pub(super) async fn combined_binary_contains_every_component(
    _context: &E2eContext,
) -> Result<()> {
    let Some(binary) = combined_binary()? else {
        return Err(skip_test(
            "no combined notk8s binary is installed; build with --layout=combined or --layout=both",
        ));
    };

    let components = component_names(&run_binary(&binary, &["components"])?)?;
    for component in crate::components::COMPONENTS {
        anyhow::ensure!(
            components.iter().any(|name| name == component.name),
            "notk8s components omitted installed component {}",
            component.name
        );
    }
    for name in &components {
        anyhow::ensure!(
            name == "nodebootstrap"
                || crate::components::COMPONENTS
                    .iter()
                    .any(|component| component.name == name),
            "notk8s contains unknown component {name}; update the component table and dispatch list together",
        );
    }

    let help = run_binary(&binary, &["--help"])?;
    anyhow::ensure!(help.status.success(), "notk8s --help failed");
    let help = String::from_utf8_lossy(&help.stdout);
    for name in &components {
        anyhow::ensure!(
            help.contains(name),
            "notk8s --help omitted component {name} listed by notk8s components",
        );
    }
    Ok(())
}

pub(super) async fn combined_binary_rejects_an_unknown_component(
    _context: &E2eContext,
) -> Result<()> {
    let Some(binary) = combined_binary()? else {
        return Err(skip_test(
            "no combined notk8s binary is installed; build with --layout=combined or --layout=both",
        ));
    };
    let output = run_binary(&binary, &["nodeproxyy"])?;
    anyhow::ensure!(
        !output.status.success(),
        "notk8s accepted an unknown component name"
    );
    Ok(())
}

pub(super) async fn installed_component_binaries_are_runnable_whatever_the_layout(
    _context: &E2eContext,
) -> Result<()> {
    let directory = binary_dir()?;
    let mut installed = 0;
    for component in crate::components::COMPONENTS {
        let path = directory.join(component.name);
        if !is_executable(&path) {
            continue;
        }
        installed += 1;
        if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?;
            anyhow::ensure!(
                target == Path::new("notk8s"),
                "{} is a symlink to {}, expected the relative combined binary notk8s",
                path.display(),
                target.display()
            );
            let combined = directory.join("notk8s");
            anyhow::ensure!(
                is_executable(&combined),
                "{} points at a missing or non-executable combined binary",
                path.display()
            );
            let components = component_names(&run_binary(&combined, &["components"])?)?;
            anyhow::ensure!(
                components.iter().any(|name| name == component.name),
                "combined binary behind {} does not contain {}",
                path.display(),
                component.name
            );
        }
    }
    if installed == 0 {
        return Err(skip_test(
            "no runtime component binaries are installed in the nodebootstrap toolchain directory",
        ));
    }
    Ok(())
}

pub(super) async fn a_failing_component_says_why_before_it_exits(
    _context: &E2eContext,
) -> Result<()> {
    let nodeproxy = binary_dir()?.join("nodeproxy");
    if !is_executable(&nodeproxy) {
        return Err(skip_test(
            "nodeproxy is not installed; --proxy=none intentionally omits this component",
        ));
    }

    let bad_kubeconfig = std::env::temp_dir().join(format!(
        "nodebootstrap-e2e-missing-kubeconfig-{}",
        std::process::id()
    ));
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(&nodeproxy)
            .env_remove("KUBERNETES_SERVICE_HOST")
            .env_remove("KUBERNETES_SERVICE_PORT")
            .env("KUBECONFIG", &bad_kubeconfig)
            .output(),
    )
    .await
    .context("waiting for nodeproxy to report its startup failure")??;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    anyhow::ensure!(
        !output.status.success(),
        "nodeproxy unexpectedly succeeded with an invalid kubeconfig"
    );
    anyhow::ensure!(
        !text.trim().is_empty(),
        "nodeproxy exited non-zero without explaining why it failed"
    );
    anyhow::ensure!(
        text.contains("kube client"),
        "nodeproxy failure did not identify kube-client startup: {text}"
    );
    Ok(())
}
