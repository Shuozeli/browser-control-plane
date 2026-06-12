use std::net::SocketAddr;

use bcp_controller::ControllerService;
use bcp_proto::browsercontrol::v1::global_controller_server::GlobalControllerServer;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "bcp-controller", about = "Global browser fleet controller")]
struct Args {
    /// Address to bind. Defaults to TAILSCALE_IP:7000, then 0.0.0.0:7000.
    #[arg(long, env = "BCP_CONTROLLER_ADDR")]
    addr: Option<SocketAddr>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let addr = args.addr.unwrap_or_else(default_addr);
    tracing::info!(%addr, "starting global controller");

    tonic::transport::Server::builder()
        .add_service(GlobalControllerServer::new(ControllerService::default()))
        .serve(addr)
        .await?;

    Ok(())
}

fn default_addr() -> SocketAddr {
    let host = std::env::var("TAILSCALE_IP").unwrap_or_else(|_| "0.0.0.0".to_string());
    format!("{host}:7000")
        .parse()
        .expect("default controller address should parse")
}
