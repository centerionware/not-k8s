fn main() -> anyhow::Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .map_err(|_| {
                anyhow::anyhow!("another rustls crypto provider was installed during startup")
            })?;
    }
    nodemigrate::run(std::env::args().skip(1))
}
