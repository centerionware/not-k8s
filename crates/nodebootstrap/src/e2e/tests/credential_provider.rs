use super::context::E2eContext;
use super::skip_test;
use anyhow::Result;

pub(super) async fn credential_provider_config_unset_by_default(
    _context: &E2eContext,
) -> Result<()> {
    if crate::config::Config::from_env()?.nodelet_runtime() != "cri" {
        return Err(skip_test("image credential provider checks require the CRI runtime"));
    }
    if std::env::var_os("NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG")
        .is_some_and(|value| !value.is_empty())
    {
        return Err(skip_test(
            "NODELET_IMAGE_CREDENTIAL_PROVIDER_CONFIG is set; the default-off case does not apply to this deployment",
        ));
    }
    Ok(())
}
