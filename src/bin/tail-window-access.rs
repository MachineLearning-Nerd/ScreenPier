use clap::Parser;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tail_window_access::server::run(tail_window_access::server::Config::parse()).await
}
