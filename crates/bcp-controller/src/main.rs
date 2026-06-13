use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use bcp_controller::{ControllerService, web};
use bcp_core::id::UuidIdGenerator;
use bcp_core::network::StaticNetworkDirectory;
use bcp_core::time::SystemClock;
use bcp_proto::browsercontrol::v1::global_controller_server::GlobalControllerServer;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "bcp-controller", about = "Global browser fleet controller")]
struct Args {
    /// Address to bind. Defaults to TAILSCALE_IP:7000, then 0.0.0.0:7000.
    #[arg(long, env = "BCP_CONTROLLER_ADDR")]
    addr: Option<SocketAddr>,

    /// SQLite database path for controller state.
    #[arg(long, env = "BCP_CONTROLLER_DB")]
    db_path: Option<PathBuf>,

    /// HTTP web UI address. Defaults to TAILSCALE_IP:7080, then 0.0.0.0:7080.
    #[arg(long, env = "BCP_CONTROLLER_WEB_ADDR")]
    web_addr: Option<SocketAddr>,

    /// Disable the built-in HTTP web UI.
    #[arg(long, env = "BCP_CONTROLLER_DISABLE_WEB", default_value_t = false)]
    disable_web: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let addr = args.addr.unwrap_or_else(default_addr);
    let db_path = args.db_path.unwrap_or_else(default_db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    tracing::info!(%addr, db_path = %db_path.display(), "starting global controller");
    let service = ControllerService::new_with_sqlite(
        &db_path,
        Arc::new(SystemClock),
        Arc::new(UuidIdGenerator),
        Arc::new(StaticNetworkDirectory::default()),
    )?;

    let grpc = tonic::transport::Server::builder()
        .add_service(GlobalControllerServer::new(service.clone()))
        .serve(addr);

    if args.disable_web {
        grpc.await?;
    } else {
        let web_addr = args.web_addr.unwrap_or_else(default_web_addr);
        tokio::select! {
            result = grpc => result?,
            result = web::serve(web_addr, service) => result?,
        }
    }

    Ok(())
}

fn default_addr() -> SocketAddr {
    let host = std::env::var("TAILSCALE_IP").unwrap_or_else(|_| "0.0.0.0".to_string());
    format!("{host}:7000")
        .parse()
        .expect("default controller address should parse")
}

fn default_db_path() -> PathBuf {
    PathBuf::from(".bcp").join("controller.sqlite")
}

fn default_web_addr() -> SocketAddr {
    let host = std::env::var("TAILSCALE_IP").unwrap_or_else(|_| "0.0.0.0".to_string());
    format!("{host}:7080")
        .parse()
        .expect("default controller web address should parse")
}
