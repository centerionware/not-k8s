fn main() -> anyhow::Result<()> {
    nodemigrate::run(std::env::args().skip(1))
}
