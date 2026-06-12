use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use bcp_proto::browsercontrol::v1::global_controller_client::GlobalControllerClient;
use bcp_proto::browsercontrol::v1::machine_controller_client::MachineControllerClient;
use bcp_proto::browsercontrol::v1::upload_artifact_request::Part;
use bcp_proto::browsercontrol::v1::*;
use tokio::process::{Child, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::var("BCP_E2E_MODE").as_deref() {
        Ok("real-browser") => real_browser_main().await,
        Ok("sqlite-persistence") => sqlite_persistence_main().await,
        _ => recording_main().await,
    }
}

async fn recording_main() -> anyhow::Result<()> {
    let controller =
        std::env::var("BCP_CONTROLLER").unwrap_or_else(|_| "http://controller:7000".to_string());
    let mut global = connect_global(&controller).await?;

    register_machine(
        &mut global,
        "machine-a",
        "http://agent-a:7100",
        profile(
            "youtube-main",
            "machine-a",
            AccountPlatform::Youtube,
            "yt-main",
        ),
    )
    .await?;
    register_machine(
        &mut global,
        "machine-b",
        "http://agent-b:7100",
        profile("x-news", "machine-b", AccountPlatform::X, "x-news"),
    )
    .await?;

    let acquired = global
        .acquire_browser(AcquireBrowserRequest {
            client_id: "docker-e2e".to_string(),
            purpose: "verify-fake-pwright-route".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-main".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 60,
        })
        .await?
        .into_inner();
    let route = acquired.route.context("controller did not return route")?;
    let lease = acquired.lease.context("controller did not return lease")?;
    if route.machine_id != "machine-a" {
        bail!("expected route to machine-a, got {}", route.machine_id);
    }

    let mut routed_agent = connect_agent(&route.agent_grpc_addr).await?;
    let lease_context = LeaseContext {
        lease_id: lease.lease_id.clone(),
        profile_id: lease.profile_id.clone(),
        fencing_token: lease.fencing_token.clone(),
    };
    routed_agent
        .install_lease(InstallLeaseRequest {
            lease: Some(lease_context.clone()),
        })
        .await?;

    let snapshot = routed_agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(lease_context.clone()),
        })
        .await?
        .into_inner();
    if snapshot.nodes.iter().all(|node| node.r#ref != "e1") {
        bail!("recording gateway snapshot did not include e1");
    }

    let action = routed_agent
        .execute_action(ExecuteActionRequest {
            lease: Some(lease_context.clone()),
            action: "click".to_string(),
            r#ref: "e1".to_string(),
            text: String::new(),
            key: String::new(),
            options: HashMap::new(),
        })
        .await?
        .into_inner();
    if !action.success {
        bail!("recording gateway action failed: {}", action.message);
    }

    let upload_messages = vec![
        UploadArtifactRequest {
            part: Some(Part::Metadata(UploadArtifactMetadata {
                lease: Some(lease_context.clone()),
                original_filename: "video.mp4".to_string(),
                content_type: "video/mp4".to_string(),
                purpose: "youtube-upload".to_string(),
                ttl_seconds: 60,
            })),
        },
        UploadArtifactRequest {
            part: Some(Part::Chunk(b"fake-video-bytes".to_vec())),
        },
    ];
    let uploaded = routed_agent
        .upload_artifact(tokio_stream::iter(upload_messages))
        .await?
        .into_inner()
        .artifact
        .context("agent did not return uploaded artifact")?;
    if uploaded.size_bytes != 16 {
        bail!(
            "expected uploaded artifact size 16, got {}",
            uploaded.size_bytes
        );
    }

    let local_artifacts = routed_agent
        .list_local_artifacts(ListLocalArtifactsRequest {
            profile_id: lease.profile_id.clone(),
            lease_id: String::new(),
            include_expired: false,
        })
        .await?
        .into_inner();
    if local_artifacts.artifacts.len() != 1 {
        bail!(
            "expected one local artifact, got {}",
            local_artifacts.artifacts.len()
        );
    }

    global
        .report_artifacts(ReportArtifactsRequest {
            reporter_machine_id: route.machine_id.clone(),
            artifacts: local_artifacts.artifacts,
        })
        .await?;
    let fleet_artifacts = global
        .list_artifacts(ListArtifactsRequest {
            machine_id: route.machine_id.clone(),
            profile_id: lease.profile_id.clone(),
            lease_id: String::new(),
            include_expired: false,
        })
        .await?
        .into_inner();
    if fleet_artifacts.artifacts.len() != 1 {
        bail!(
            "expected one fleet artifact, got {}",
            fleet_artifacts.artifacts.len()
        );
    }

    let mut wrong_agent = connect_agent("http://agent-b:7100").await?;
    let wrong_result = wrong_agent
        .execute_action(ExecuteActionRequest {
            lease: Some(lease_context),
            action: "click".to_string(),
            r#ref: "e1".to_string(),
            text: String::new(),
            key: String::new(),
            options: HashMap::new(),
        })
        .await;
    match wrong_result {
        Err(status) if status.code() == tonic::Code::PermissionDenied => {}
        Err(status) => bail!("wrong agent returned unexpected status: {status}"),
        Ok(_) => bail!("wrong agent accepted a lease that was never installed"),
    }

    println!("docker e2e passed: controller routed to recording pwright gateway");
    Ok(())
}

async fn sqlite_persistence_main() -> anyhow::Result<()> {
    let root = Path::new("/tmp/bcp-sqlite-e2e");
    if root.exists() {
        std::fs::remove_dir_all(root)?;
    }
    std::fs::create_dir_all(root)?;

    let controller_db = root.join("controller.sqlite");
    let agent_db = root.join("agent.sqlite");
    let controller_addr = "127.0.0.1:7010";
    let agent_addr = "127.0.0.1:7110";
    let controller_endpoint = format!("http://{controller_addr}");
    let agent_endpoint = format!("http://{agent_addr}");

    let mut controller =
        spawn_controller(controller_addr, &controller_db).context("spawn first controller")?;
    let mut agent = spawn_agent(agent_addr, &agent_db, &controller_endpoint, &agent_endpoint)
        .context("spawn agent")?;

    let mut global = connect_global(&controller_endpoint).await?;
    wait_for_auto_registered_profile(&mut global, "machine-sqlite", "yt-sqlite").await?;

    let acquired = global
        .acquire_browser(AcquireBrowserRequest {
            client_id: "sqlite-e2e".to_string(),
            purpose: "verify-sqlite-restore".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-sqlite".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 300,
        })
        .await?
        .into_inner();
    let route = acquired.route.context("controller did not return route")?;
    let lease = acquired.lease.context("controller did not return lease")?;
    if route.agent_grpc_addr != agent_endpoint {
        bail!(
            "expected auto-registered agent addr {agent_endpoint}, got {}",
            route.agent_grpc_addr
        );
    }

    let lease_context = LeaseContext {
        lease_id: lease.lease_id.clone(),
        profile_id: lease.profile_id.clone(),
        fencing_token: lease.fencing_token.clone(),
    };
    let mut routed_agent = connect_agent(&route.agent_grpc_addr).await?;
    routed_agent
        .install_lease(InstallLeaseRequest {
            lease: Some(lease_context.clone()),
        })
        .await?;
    let snapshot = routed_agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(lease_context.clone()),
        })
        .await?
        .into_inner();
    if snapshot.nodes.iter().all(|node| node.r#ref != "e1") {
        bail!("sqlite e2e snapshot did not include e1");
    }

    stop_child(&mut controller, "first controller").await?;
    let mut restored_controller =
        spawn_controller(controller_addr, &controller_db).context("spawn restored controller")?;
    let mut restored_global = connect_global(&controller_endpoint).await?;

    let restored_route = restored_global
        .get_route(GetRouteRequest {
            lease_id: lease.lease_id.clone(),
            fencing_token: lease.fencing_token.clone(),
        })
        .await?
        .into_inner()
        .route
        .context("restored controller did not return route")?;
    if restored_route.machine_id != "machine-sqlite" {
        bail!(
            "expected restored route to machine-sqlite, got {}",
            restored_route.machine_id
        );
    }
    if restored_route.agent_grpc_addr != agent_endpoint {
        bail!(
            "expected restored agent addr {agent_endpoint}, got {}",
            restored_route.agent_grpc_addr
        );
    }

    let lookup = restored_global
        .lookup_browser_connection(LookupBrowserConnectionRequest {
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-sqlite".to_string(),
            label_selector: HashMap::new(),
            include_unavailable: true,
        })
        .await?
        .into_inner();
    if lookup.active_lease_id != lease.lease_id {
        bail!(
            "expected restored lookup active lease {}, got {}",
            lease.lease_id,
            lookup.active_lease_id
        );
    }

    stop_child(&mut restored_controller, "restored controller").await?;
    stop_child(&mut agent, "agent").await?;

    println!("docker sqlite e2e passed: auto-registration and sqlite restore work");
    Ok(())
}

async fn real_browser_main() -> anyhow::Result<()> {
    let controller =
        std::env::var("BCP_CONTROLLER").unwrap_or_else(|_| "http://controller:7000".to_string());
    let mut global = connect_global(&controller).await?;

    let machine_a_profiles = vec![
        real_profile(
            "yt-a-1",
            "machine-a",
            AccountPlatform::Youtube,
            "yt-a-1",
            "http://172.31.10.11:9222",
        ),
        real_profile(
            "x-a-2",
            "machine-a",
            AccountPlatform::X,
            "x-a-2",
            "http://172.31.10.12:9222",
        ),
        real_profile(
            "douyin-a-3",
            "machine-a",
            AccountPlatform::Douyin,
            "douyin-a-3",
            "http://172.31.10.13:9222",
        ),
    ];
    let machine_b_profiles = vec![
        real_profile(
            "yt-b-1",
            "machine-b",
            AccountPlatform::Youtube,
            "yt-b-1",
            "http://172.31.20.11:9222",
        ),
        real_profile(
            "x-b-2",
            "machine-b",
            AccountPlatform::X,
            "x-b-2",
            "http://172.31.20.12:9222",
        ),
        real_profile(
            "douyin-b-3",
            "machine-b",
            AccountPlatform::Douyin,
            "douyin-b-3",
            "http://172.31.20.13:9222",
        ),
    ];
    register_machine_with_profiles(
        &mut global,
        "machine-a",
        "http://agent-a:7100",
        machine_a_profiles,
    )
    .await?;
    register_machine_with_profiles(
        &mut global,
        "machine-b",
        "http://agent-b:7100",
        machine_b_profiles,
    )
    .await?;

    let profiles = global
        .list_profiles(ListProfilesRequest {
            platform: AccountPlatform::Unspecified as i32,
            account_id: String::new(),
            label_selector: HashMap::new(),
            include_unavailable: true,
        })
        .await?
        .into_inner();
    if profiles.profiles.len() != 6 {
        bail!("expected six registered real browser profiles");
    }

    exercise_real_profile(
        &mut global,
        AccountPlatform::Youtube,
        "yt-a-1",
        "machine-a",
        "http://agent-b:7100",
    )
    .await?;
    exercise_real_profile(
        &mut global,
        AccountPlatform::X,
        "x-b-2",
        "machine-b",
        "http://agent-a:7100",
    )
    .await?;

    println!("docker real-browser e2e passed: controller routed to real CDP browsers");
    Ok(())
}

async fn exercise_real_profile(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
    platform: AccountPlatform,
    account_id: &str,
    expected_machine_id: &str,
    wrong_agent_addr: &str,
) -> anyhow::Result<()> {
    let acquired = global
        .acquire_browser(AcquireBrowserRequest {
            client_id: "docker-real-e2e".to_string(),
            purpose: "verify-real-browser-route".to_string(),
            platform: platform as i32,
            account_id: account_id.to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 60,
        })
        .await?
        .into_inner();
    let route = acquired.route.context("controller did not return route")?;
    let lease = acquired.lease.context("controller did not return lease")?;
    if route.machine_id != expected_machine_id {
        bail!(
            "expected route to {expected_machine_id}, got {}",
            route.machine_id
        );
    }

    let lease_context = LeaseContext {
        lease_id: lease.lease_id.clone(),
        profile_id: lease.profile_id.clone(),
        fencing_token: lease.fencing_token.clone(),
    };
    let mut agent = connect_agent(&route.agent_grpc_addr).await?;
    agent
        .install_lease(InstallLeaseRequest {
            lease: Some(lease_context.clone()),
        })
        .await?;
    agent
        .ensure_browser(EnsureBrowserRequest {
            lease: Some(lease_context.clone()),
        })
        .await?;

    let snapshot = agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(lease_context.clone()),
        })
        .await?
        .into_inner();
    let button = snapshot
        .nodes
        .iter()
        .find(|node| node.name.contains(account_id) && node.name.contains("Mark"))
        .cloned()
        .context("real browser snapshot did not contain target button")?;
    agent
        .execute_action(ExecuteActionRequest {
            lease: Some(lease_context.clone()),
            action: "click".to_string(),
            r#ref: button.r#ref,
            text: String::new(),
            key: String::new(),
            options: HashMap::from([("selector".to_string(), "#mark".to_string())]),
        })
        .await?;
    let clicked = agent
        .evaluate(EvaluateRequest {
            lease: Some(lease_context.clone()),
            expression: "document.body.dataset.clicked || ''".to_string(),
        })
        .await?
        .into_inner();
    if clicked.json_result != account_id {
        bail!(
            "real browser click did not update page state for {account_id}: {}",
            clicked.json_result
        );
    }

    let mut wrong_agent = connect_agent(wrong_agent_addr).await?;
    let wrong_result = wrong_agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(lease_context),
        })
        .await;
    match wrong_result {
        Err(status)
            if matches!(
                status.code(),
                tonic::Code::PermissionDenied | tonic::Code::NotFound
            ) => {}
        Err(status) => bail!("wrong real-browser agent returned unexpected status: {status}"),
        Ok(_) => bail!("wrong real-browser agent accepted a lease that was never installed"),
    }
    Ok(())
}

fn spawn_controller(addr: &str, db_path: &Path) -> anyhow::Result<Child> {
    let binary = std::env::var("BCP_CONTROLLER_BIN")
        .unwrap_or_else(|_| "/usr/local/bin/bcp-controller".to_string());
    let child = Command::new(binary)
        .arg("--addr")
        .arg(addr)
        .arg("--db-path")
        .arg(db_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(child)
}

fn spawn_agent(
    addr: &str,
    db_path: &Path,
    controller_endpoint: &str,
    agent_endpoint: &str,
) -> anyhow::Result<Child> {
    let binary =
        std::env::var("BCP_AGENT_BIN").unwrap_or_else(|_| "/usr/local/bin/bcp-agent".to_string());
    let child = Command::new(binary)
        .arg("--addr")
        .arg(addr)
        .arg("--db-path")
        .arg(db_path)
        .env("BCP_CONTROLLER", controller_endpoint)
        .env("BCP_CONTROLLER_REGISTER_SECONDS", "1")
        .env("BCP_AGENT_PUBLIC_ADDR", agent_endpoint)
        .env("BCP_MACHINE_ID", "machine-sqlite")
        .env("BCP_E2E_PROFILE_ID", "youtube-sqlite")
        .env("BCP_E2E_ACCOUNT_ID", "yt-sqlite")
        .env("BCP_E2E_PLATFORM", "youtube")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(child)
}

async fn stop_child(child: &mut Child, name: &str) -> anyhow::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    child.kill().await.context(format!("kill {name}"))?;
    let _ = child.wait().await;
    Ok(())
}

async fn wait_for_auto_registered_profile(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
    expected_machine_id: &str,
    expected_account_id: &str,
) -> anyhow::Result<()> {
    let mut last_error = None;
    for _ in 0..30 {
        match global
            .lookup_browser_connection(LookupBrowserConnectionRequest {
                platform: AccountPlatform::Youtube as i32,
                account_id: expected_account_id.to_string(),
                label_selector: HashMap::new(),
                include_unavailable: true,
            })
            .await
        {
            Ok(response) => {
                let response = response.into_inner();
                if response
                    .route_hint
                    .as_ref()
                    .is_some_and(|route| route.machine_id == expected_machine_id)
                {
                    return Ok(());
                }
            }
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    match last_error {
        Some(error) => Err(anyhow::Error::from(error))
            .context("auto-registered profile did not become visible"),
        None => bail!("auto-registered profile did not become visible"),
    }
}

async fn connect_global(
    endpoint: &str,
) -> anyhow::Result<GlobalControllerClient<tonic::transport::Channel>> {
    let mut last_error = None;
    for _ in 0..30 {
        match GlobalControllerClient::connect(endpoint.to_string()).await {
            Ok(client) => return Ok(client),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    match last_error {
        Some(error) => {
            Err(anyhow::Error::from(error)).context("global controller did not become reachable")
        }
        None => bail!("global controller did not become reachable"),
    }
}

async fn connect_agent(
    endpoint: &str,
) -> anyhow::Result<MachineControllerClient<tonic::transport::Channel>> {
    let mut last_error = None;
    for _ in 0..30 {
        match MachineControllerClient::connect(endpoint.to_string()).await {
            Ok(client) => return Ok(client),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    match last_error {
        Some(error) => {
            Err(anyhow::Error::from(error)).context("machine controller did not become reachable")
        }
        None => bail!("machine controller did not become reachable"),
    }
}

async fn register_machine(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
    machine_id: &str,
    agent_grpc_addr: &str,
    profile: BrowserProfile,
) -> anyhow::Result<()> {
    register_machine_with_profiles(global, machine_id, agent_grpc_addr, vec![profile]).await
}

async fn register_machine_with_profiles(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
    machine_id: &str,
    agent_grpc_addr: &str,
    profiles: Vec<BrowserProfile>,
) -> anyhow::Result<()> {
    global
        .register_machine(RegisterMachineRequest {
            machine: Some(Machine {
                machine_id: machine_id.to_string(),
                hostname: machine_id.to_string(),
                tailscale_host: format!("{machine_id}.k3s.test"),
                agent_grpc_addr: agent_grpc_addr.to_string(),
                status: MachineStatus::Online as i32,
                labels: HashMap::from([("cluster".to_string(), "docker-k3s".to_string())]),
                last_heartbeat_unix_ms: 0,
            }),
            profiles,
        })
        .await?;
    Ok(())
}

fn profile(
    profile_id: &str,
    machine_id: &str,
    platform: AccountPlatform,
    account_id: &str,
) -> BrowserProfile {
    BrowserProfile {
        profile_id: profile_id.to_string(),
        machine_id: machine_id.to_string(),
        profile_path: format!("/fake-profiles/{profile_id}"),
        display_name: profile_id.to_string(),
        status: ProfileStatus::Available as i32,
        cdp_url: "fake://pwright".to_string(),
        cdp_port: 0,
        accounts: vec![Account {
            account_id: account_id.to_string(),
            platform: platform as i32,
            handle: format!("@{account_id}"),
            health: "logged_in".to_string(),
            capabilities: vec!["click".to_string(), "snapshot".to_string()],
        }],
        labels: HashMap::new(),
        last_seen_unix_ms: 0,
    }
}

fn real_profile(
    profile_id: &str,
    machine_id: &str,
    platform: AccountPlatform,
    account_id: &str,
    cdp_url: &str,
) -> BrowserProfile {
    BrowserProfile {
        profile_id: profile_id.to_string(),
        machine_id: machine_id.to_string(),
        profile_path: format!("/real-profiles/{profile_id}"),
        display_name: profile_id.to_string(),
        status: ProfileStatus::Available as i32,
        cdp_url: cdp_url.to_string(),
        cdp_port: 9222,
        accounts: vec![Account {
            account_id: account_id.to_string(),
            platform: platform as i32,
            handle: format!("@{account_id}"),
            health: "logged_in".to_string(),
            capabilities: vec![
                "click".to_string(),
                "snapshot".to_string(),
                "eval".to_string(),
            ],
        }],
        labels: HashMap::new(),
        last_seen_unix_ms: 0,
    }
}
