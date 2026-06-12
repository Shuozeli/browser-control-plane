use bcp_proto::browsercontrol::v1::global_controller_client::GlobalControllerClient;
use bcp_proto::browsercontrol::v1::{
    AccountPlatform, ListMachinesRequest, LookupBrowserConnectionRequest,
};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "bcp", about = "Browser control plane client")]
struct Args {
    /// Global controller endpoint.
    #[arg(long, env = "BCP_CONTROLLER", default_value = "http://127.0.0.1:7000")]
    controller: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List machines registered in the global controller.
    Machines,
    /// Lookup where an account's browser profile lives and how to connect.
    Lookup {
        /// Platform/site name: youtube, x, douyin, tiktok, reddit, zhihu, weibo.
        #[arg(long)]
        platform: String,

        /// Account id registered in the control plane.
        #[arg(long)]
        account_id: String,

        /// Include leased, launching, broken, or quarantined profiles.
        #[arg(long)]
        include_unavailable: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Machines => {
            let mut client = GlobalControllerClient::connect(args.controller).await?;
            let response = client
                .list_machines(ListMachinesRequest {
                    label_selector: Default::default(),
                })
                .await?
                .into_inner();
            println!("{}", response.machines.len());
        }
        Command::Lookup {
            platform,
            account_id,
            include_unavailable,
        } => {
            let mut client = GlobalControllerClient::connect(args.controller).await?;
            let response = client
                .lookup_browser_connection(LookupBrowserConnectionRequest {
                    platform: parse_platform(&platform)?,
                    account_id,
                    label_selector: Default::default(),
                    include_unavailable,
                })
                .await?
                .into_inner();
            let binding = response
                .binding
                .ok_or_else(|| anyhow::anyhow!("lookup response did not include binding"))?;
            let route = response
                .route_hint
                .ok_or_else(|| anyhow::anyhow!("lookup response did not include route hint"))?;

            println!("account_id={}", binding.account_id);
            println!("platform={}", binding.platform);
            println!("handle={}", binding.handle);
            println!("profile_id={}", binding.profile_id);
            println!("machine_id={}", route.machine_id);
            println!("agent_grpc_addr={}", route.agent_grpc_addr);
            println!("available={}", response.available);
            println!("connection_state={}", response.connection_state);
            if !response.active_lease_id.is_empty() {
                println!("active_lease_id={}", response.active_lease_id);
                println!(
                    "active_lease_expires_at_unix_ms={}",
                    response.active_lease_expires_at_unix_ms
                );
            }
        }
    }
    Ok(())
}

fn parse_platform(value: &str) -> anyhow::Result<i32> {
    let platform = match value.to_ascii_lowercase().as_str() {
        "youtube" | "yt" => AccountPlatform::Youtube,
        "x" | "twitter" => AccountPlatform::X,
        "douyin" => AccountPlatform::Douyin,
        "tiktok" => AccountPlatform::Tiktok,
        "reddit" => AccountPlatform::Reddit,
        "zhihu" => AccountPlatform::Zhihu,
        "weibo" => AccountPlatform::Weibo,
        _ => anyhow::bail!("unsupported platform: {value}"),
    };
    Ok(platform as i32)
}
