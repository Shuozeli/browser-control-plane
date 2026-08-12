use std::collections::HashMap;

use bcp_proto::browsercontrol::v1::global_controller_client::GlobalControllerClient;
use bcp_proto::browsercontrol::v1::machine_controller_client::MachineControllerClient;
use bcp_proto::browsercontrol::v1::{
    AccountPlatform, AcquireBrowserRequest, BrowserLease, BrowserRoute, CaptureScreenshotRequest,
    EnsureBrowserRequest, EvaluateRequest, GetRouteRequest, GetSnapshotRequest,
    InstallLeaseRequest, LeaseContext, ListControlPlaneEventsRequest, ListMachinesRequest,
    LookupBrowserConnectionRequest, PrintPdfRequest, QuarantineProfileRequest, ReleaseLeaseRequest,
    ReleaseQuarantineRequest, RunScriptRequest,
};
use base64::Engine;
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
    /// Acquire an exclusive lease for an account and print the lease + route.
    Acquire {
        #[arg(long)]
        platform: String,
        #[arg(long)]
        account_id: String,
        #[arg(long, default_value_t = 300)]
        ttl_seconds: i64,
    },
    /// Release a held lease.
    Release {
        #[arg(long)]
        lease_id: String,
        #[arg(long)]
        fencing_token: String,
    },
    /// Look up the current route for a held lease.
    Route {
        #[arg(long)]
        lease_id: String,
        #[arg(long)]
        fencing_token: String,
    },
    /// Acquire, take an accessibility snapshot through the machine controller,
    /// then release.
    Snapshot {
        #[arg(long)]
        platform: String,
        #[arg(long)]
        account_id: String,
    },
    /// Acquire, evaluate JavaScript in the browser, then release.
    Eval {
        #[arg(long)]
        platform: String,
        #[arg(long)]
        account_id: String,
        #[arg(long)]
        expression: String,
    },
    /// Acquire, run a YAML browser script (streaming per-step JSON), then release.
    RunScript {
        #[arg(long)]
        platform: String,
        #[arg(long)]
        account_id: String,
        /// Path to a YAML script file.
        #[arg(long)]
        file: std::path::PathBuf,
    },
    /// Directly install a lease on a machine controller (agent) without going
    /// through the global controller. Useful for manual/raw-CDP-proxy access.
    Install {
        /// Machine controller (agent) gRPC endpoint, e.g. http://host:7100.
        #[arg(long)]
        agent: String,
        #[arg(long)]
        lease_id: String,
        #[arg(long)]
        profile_id: String,
        #[arg(long)]
        fencing_token: String,
        /// Lease lifetime in seconds (agent rejects the lease past this).
        #[arg(long, default_value_t = 600)]
        ttl_seconds: i64,
    },
    /// Acquire, capture a screenshot to a file, then release.
    Screenshot {
        #[arg(long)]
        platform: String,
        #[arg(long)]
        account_id: String,
        /// Output image file.
        #[arg(long)]
        out: std::path::PathBuf,
        /// Image format: png (default), jpeg, or webp.
        #[arg(long, default_value = "png")]
        format: String,
        /// Capture the full scrollable page instead of just the viewport.
        #[arg(long, default_value_t = false)]
        full_page: bool,
    },
    /// Acquire, print the page to a PDF file, then release.
    Pdf {
        #[arg(long)]
        platform: String,
        #[arg(long)]
        account_id: String,
        /// Output PDF file.
        #[arg(long)]
        out: std::path::PathBuf,
    },
    /// Quarantine a profile so it is no longer selected for acquire.
    Quarantine {
        #[arg(long)]
        profile_id: String,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Release a profile from quarantine.
    Unquarantine {
        #[arg(long)]
        profile_id: String,
    },
    /// List recent structured control-plane events (browser audit log).
    Events {
        #[arg(long, default_value = "")]
        machine_id: String,
        #[arg(long, default_value_t = 50)]
        limit: i32,
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
        Command::Acquire {
            platform,
            account_id,
            ttl_seconds,
        } => {
            let mut client = GlobalControllerClient::connect(args.controller).await?;
            let (lease, route) = acquire(&mut client, &platform, &account_id, ttl_seconds).await?;
            print_lease(&lease, &route);
        }
        Command::Release {
            lease_id,
            fencing_token,
        } => {
            let mut client = GlobalControllerClient::connect(args.controller).await?;
            let released = client
                .release_lease(ReleaseLeaseRequest {
                    lease_id,
                    fencing_token,
                })
                .await?
                .into_inner()
                .released;
            println!("released={released}");
        }
        Command::Route {
            lease_id,
            fencing_token,
        } => {
            let mut client = GlobalControllerClient::connect(args.controller).await?;
            let route = client
                .get_route(GetRouteRequest {
                    lease_id,
                    fencing_token,
                })
                .await?
                .into_inner()
                .route
                .ok_or_else(|| anyhow::anyhow!("get_route response did not include route"))?;
            println!("machine_id={}", route.machine_id);
            println!("agent_grpc_addr={}", route.agent_grpc_addr);
            println!("profile_id={}", route.profile_id);
        }
        Command::Snapshot {
            platform,
            account_id,
        } => {
            let mut controller = GlobalControllerClient::connect(args.controller).await?;
            let (lease, route) = acquire(&mut controller, &platform, &account_id, 120).await?;
            let context = lease_context(&lease);
            let mut agent = MachineControllerClient::connect(route.agent_grpc_addr.clone()).await?;
            agent
                .install_lease(InstallLeaseRequest {
                    lease: Some(context.clone()),
                })
                .await?;
            agent
                .ensure_browser(EnsureBrowserRequest {
                    lease: Some(context.clone()),
                })
                .await?;
            let snapshot = agent
                .get_snapshot(GetSnapshotRequest {
                    lease: Some(context),
                })
                .await?
                .into_inner();
            for node in snapshot.nodes {
                println!("[{}] {} {:?}", node.r#ref, node.role, node.name);
            }
            release(&mut controller, &lease).await;
        }
        Command::Eval {
            platform,
            account_id,
            expression,
        } => {
            let mut controller = GlobalControllerClient::connect(args.controller).await?;
            let (lease, route) = acquire(&mut controller, &platform, &account_id, 120).await?;
            let context = lease_context(&lease);
            let mut agent = MachineControllerClient::connect(route.agent_grpc_addr.clone()).await?;
            agent
                .install_lease(InstallLeaseRequest {
                    lease: Some(context.clone()),
                })
                .await?;
            agent
                .ensure_browser(EnsureBrowserRequest {
                    lease: Some(context.clone()),
                })
                .await?;
            let result = agent
                .evaluate(EvaluateRequest {
                    lease: Some(context),
                    expression,
                })
                .await?
                .into_inner();
            println!("{}", result.json_result);
            release(&mut controller, &lease).await;
        }
        Command::RunScript {
            platform,
            account_id,
            file,
        } => {
            let yaml = std::fs::read_to_string(&file)?;
            let mut controller = GlobalControllerClient::connect(args.controller).await?;
            let (lease, route) = acquire(&mut controller, &platform, &account_id, 300).await?;
            let context = lease_context(&lease);
            let mut agent = MachineControllerClient::connect(route.agent_grpc_addr.clone()).await?;
            agent
                .install_lease(InstallLeaseRequest {
                    lease: Some(context.clone()),
                })
                .await?;
            agent
                .ensure_browser(EnsureBrowserRequest {
                    lease: Some(context.clone()),
                })
                .await?;
            let mut stream = agent
                .run_script(RunScriptRequest {
                    lease: Some(context),
                    yaml,
                    params: HashMap::new(),
                })
                .await?
                .into_inner();
            while let Some(line) = stream.message().await? {
                println!("{}", line.json_line);
            }
            release(&mut controller, &lease).await;
        }
        Command::Install {
            agent,
            lease_id,
            profile_id,
            fencing_token,
            ttl_seconds,
        } => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|delta| delta.as_millis() as i64)
                .unwrap_or(0);
            let mut client = MachineControllerClient::connect(agent).await?;
            let response = client
                .install_lease(InstallLeaseRequest {
                    lease: Some(LeaseContext {
                        lease_id: lease_id.clone(),
                        profile_id,
                        fencing_token,
                        expires_at_unix_ms: now_ms + ttl_seconds * 1000,
                    }),
                })
                .await?
                .into_inner();
            println!("installed={} lease_id={}", response.installed, lease_id);
        }
        Command::Screenshot {
            platform,
            account_id,
            out,
            format,
            full_page,
        } => {
            let mut controller = GlobalControllerClient::connect(args.controller).await?;
            let (lease, route) = acquire(&mut controller, &platform, &account_id, 120).await?;
            let context = lease_context(&lease);
            let mut agent = MachineControllerClient::connect(route.agent_grpc_addr.clone()).await?;
            agent
                .install_lease(InstallLeaseRequest {
                    lease: Some(context.clone()),
                })
                .await?;
            agent
                .ensure_browser(EnsureBrowserRequest {
                    lease: Some(context.clone()),
                })
                .await?;
            let result = agent
                .capture_screenshot(CaptureScreenshotRequest {
                    lease: Some(context),
                    format,
                    full_page,
                })
                .await?
                .into_inner();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(result.base64_data.as_bytes())?;
            std::fs::write(&out, &bytes)?;
            println!(
                "wrote {}-byte {} screenshot to {}",
                bytes.len(),
                result.format,
                out.display()
            );
            release(&mut controller, &lease).await;
        }
        Command::Pdf {
            platform,
            account_id,
            out,
        } => {
            let mut controller = GlobalControllerClient::connect(args.controller).await?;
            let (lease, route) = acquire(&mut controller, &platform, &account_id, 120).await?;
            let context = lease_context(&lease);
            let mut agent = MachineControllerClient::connect(route.agent_grpc_addr.clone()).await?;
            agent
                .install_lease(InstallLeaseRequest {
                    lease: Some(context.clone()),
                })
                .await?;
            agent
                .ensure_browser(EnsureBrowserRequest {
                    lease: Some(context.clone()),
                })
                .await?;
            let result = agent
                .print_pdf(PrintPdfRequest {
                    lease: Some(context),
                })
                .await?
                .into_inner();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(result.base64_data.as_bytes())?;
            std::fs::write(&out, &bytes)?;
            println!("wrote {}-byte PDF to {}", bytes.len(), out.display());
            release(&mut controller, &lease).await;
        }
        Command::Quarantine { profile_id, reason } => {
            let mut client = GlobalControllerClient::connect(args.controller).await?;
            let profile = client
                .quarantine_profile(QuarantineProfileRequest { profile_id, reason })
                .await?
                .into_inner()
                .profile
                .ok_or_else(|| anyhow::anyhow!("quarantine response did not include profile"))?;
            println!(
                "profile_id={} status={}",
                profile.profile_id, profile.status
            );
        }
        Command::Unquarantine { profile_id } => {
            let mut client = GlobalControllerClient::connect(args.controller).await?;
            let profile = client
                .release_quarantine(ReleaseQuarantineRequest { profile_id })
                .await?
                .into_inner()
                .profile
                .ok_or_else(|| anyhow::anyhow!("unquarantine response did not include profile"))?;
            println!(
                "profile_id={} status={}",
                profile.profile_id, profile.status
            );
        }
        Command::Events { machine_id, limit } => {
            let mut client = GlobalControllerClient::connect(args.controller).await?;
            let events = client
                .list_control_plane_events(ListControlPlaneEventsRequest {
                    start_unix_ms: 0,
                    end_unix_ms: 0,
                    machine_id,
                    profile_id: String::new(),
                    limit,
                })
                .await?
                .into_inner();
            for event in events.events {
                println!(
                    "{} {} machine={} profile={} {}",
                    event.observed_at_unix_ms,
                    event.event_type,
                    event.machine_id,
                    event.profile_id,
                    event.message
                );
            }
        }
    }
    Ok(())
}

async fn acquire(
    controller: &mut GlobalControllerClient<tonic::transport::Channel>,
    platform: &str,
    account_id: &str,
    ttl_seconds: i64,
) -> anyhow::Result<(BrowserLease, BrowserRoute)> {
    let response = controller
        .acquire_browser(AcquireBrowserRequest {
            client_id: "bcp-cli".to_string(),
            purpose: "cli".to_string(),
            platform: parse_platform(platform)?,
            account_id: account_id.to_string(),
            label_selector: HashMap::new(),
            ttl_seconds,
        })
        .await?
        .into_inner();
    let lease = response
        .lease
        .ok_or_else(|| anyhow::anyhow!("acquire response did not include a lease"))?;
    let route = response
        .route
        .ok_or_else(|| anyhow::anyhow!("acquire response did not include a route"))?;
    Ok((lease, route))
}

async fn release(
    controller: &mut GlobalControllerClient<tonic::transport::Channel>,
    lease: &BrowserLease,
) {
    let _ = controller
        .release_lease(ReleaseLeaseRequest {
            lease_id: lease.lease_id.clone(),
            fencing_token: lease.fencing_token.clone(),
        })
        .await;
}

fn lease_context(lease: &BrowserLease) -> LeaseContext {
    LeaseContext {
        lease_id: lease.lease_id.clone(),
        profile_id: lease.profile_id.clone(),
        fencing_token: lease.fencing_token.clone(),
        expires_at_unix_ms: lease.expires_at_unix_ms,
    }
}

fn print_lease(lease: &BrowserLease, route: &BrowserRoute) {
    println!("lease_id={}", lease.lease_id);
    println!("fencing_token={}", lease.fencing_token);
    println!("profile_id={}", lease.profile_id);
    println!("machine_id={}", route.machine_id);
    println!("agent_grpc_addr={}", route.agent_grpc_addr);
    println!("expires_at_unix_ms={}", lease.expires_at_unix_ms);
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
        "wsj" => AccountPlatform::Wsj,
        "hn" | "hacker-news" | "hackernews" => AccountPlatform::HackerNews,
        _ => anyhow::bail!("unsupported platform: {value}"),
    };
    Ok(platform as i32)
}
