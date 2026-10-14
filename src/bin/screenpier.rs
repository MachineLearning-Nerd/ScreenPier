use clap::Parser;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    screenpier::server::run(screenpier::server::Config::parse()).await
}
