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
        Ok("vm-fleet") => vm_fleet_main().await,
        Ok("scenarios") => scenarios_main().await,
        Ok("real-web-wsj") => real_web_wsj_main().await,
        Ok("real-web-hn") => real_web_hn_main().await,
        Ok("fake-failures") => fake_failures_main().await,
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
    let agent_config = root.join("agent.toml");
    let controller_addr = "127.0.0.1:7010";
    let agent_addr = "127.0.0.1:7110";
    let controller_endpoint = format!("http://{controller_addr}");
    let agent_endpoint = format!("http://{agent_addr}");
    std::fs::write(
        &agent_config,
        r#"
machine_id = "machine-sqlite"
gateway = "recording"

[labels]
cluster = "sqlite-e2e"

[[profiles]]
profile_id = "youtube-sqlite"
account_id = "yt-sqlite"
platform = "youtube"
profile_path = "/profiles/youtube-sqlite"
display_name = "YouTube SQLite"
capabilities = ["snapshot", "click"]

[profiles.lifecycle]
launch_command = ["sh", "-c", "sleep 30 # allocated {cdp_port} for {profile_id}"]
readiness_url = "recording://skip"
"#,
    )?;

    let mut controller =
        spawn_controller(controller_addr, &controller_db).context("spawn first controller")?;
    let mut agent = spawn_agent(
        agent_addr,
        &agent_db,
        &agent_config,
        &controller_endpoint,
        &agent_endpoint,
    )
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

async fn fake_failures_main() -> anyhow::Result<()> {
    let root = Path::new("/tmp/bcp-fake-failures-e2e");
    if root.exists() {
        std::fs::remove_dir_all(root)?;
    }
    std::fs::create_dir_all(root)?;

    let controller_addr = "127.0.0.1:7020";
    let agent_addr = "127.0.0.1:7120";
    let controller_endpoint = format!("http://{controller_addr}");
    let agent_endpoint = format!("http://{agent_addr}");
    let first_controller_db = root.join("controller-first.sqlite");
    let restored_controller_db = root.join("controller-restored.sqlite");
    let agent_db = root.join("agent.sqlite");
    let agent_spec = RecordingAgentSpec {
        machine_id: "machine-failure",
        profile_id: "youtube-failure",
        account_id: "yt-failure",
        agent_endpoint: &agent_endpoint,
    };

    let mut controller =
        spawn_controller(controller_addr, &first_controller_db).context("spawn controller")?;
    let mut agent = spawn_recording_agent(
        agent_addr,
        &agent_db,
        &controller_endpoint,
        &agent_spec,
        true,
    )
    .context("spawn recording agent")?;

    let mut global = connect_global(&controller_endpoint).await?;
    wait_for_auto_registered_profile(&mut global, "machine-failure", "yt-failure").await?;

    let (route, lease) = acquire_youtube(&mut global, "fake-failures", "yt-failure").await?;
    if route.agent_grpc_addr != agent_endpoint {
        bail!(
            "expected route to restarted-test agent {agent_endpoint}, got {}",
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

    routed_agent
        .stop_browser(StopBrowserRequest {
            lease: Some(lease_context.clone()),
        })
        .await?;
    let stopped = routed_agent
        .check_browser(CheckBrowserRequest {
            lease: Some(lease_context.clone()),
        })
        .await?
        .into_inner();
    if stopped.healthy {
        bail!("expected fake browser to be unhealthy after stop_browser");
    }
    routed_agent
        .ensure_browser(EnsureBrowserRequest {
            lease: Some(lease_context.clone()),
        })
        .await?;
    let recovered = routed_agent
        .check_browser(CheckBrowserRequest {
            lease: Some(lease_context.clone()),
        })
        .await?
        .into_inner();
    if !recovered.healthy {
        bail!(
            "expected fake browser to recover after ensure_browser, got {}",
            recovered.message
        );
    }

    stop_child(&mut agent, "first agent").await?;
    let mut restarted_agent = spawn_recording_agent(
        agent_addr,
        &agent_db,
        &controller_endpoint,
        &agent_spec,
        true,
    )
    .context("spawn restarted recording agent")?;
    wait_for_auto_registered_profile(&mut global, "machine-failure", "yt-failure").await?;
    let mut restarted_agent_client = connect_agent(&agent_endpoint).await?;
    let old_lease_result = restarted_agent_client
        .execute_action(ExecuteActionRequest {
            lease: Some(lease_context.clone()),
            action: "click".to_string(),
            r#ref: "e1".to_string(),
            text: String::new(),
            key: String::new(),
            options: HashMap::new(),
        })
        .await;
    match old_lease_result {
        Err(status) if status.code() == tonic::Code::PermissionDenied => {}
        Err(status) => bail!("restarted agent returned unexpected status for old lease: {status}"),
        Ok(_) => bail!("restarted agent accepted a lease that was installed before restart"),
    }

    global
        .release_lease(ReleaseLeaseRequest {
            lease_id: lease.lease_id.clone(),
            fencing_token: lease.fencing_token.clone(),
        })
        .await?;
    let (_route, new_lease) = acquire_youtube(&mut global, "fake-failures", "yt-failure").await?;
    let new_lease_context = LeaseContext {
        lease_id: new_lease.lease_id.clone(),
        profile_id: new_lease.profile_id.clone(),
        fencing_token: new_lease.fencing_token.clone(),
    };
    restarted_agent_client
        .install_lease(InstallLeaseRequest {
            lease: Some(new_lease_context.clone()),
        })
        .await?;
    assert_action_succeeds(&mut restarted_agent_client, new_lease_context).await?;

    stop_child(&mut controller, "first controller").await?;
    let mut restored_controller = spawn_controller(controller_addr, &restored_controller_db)
        .context("spawn restored empty controller")?;
    let mut restored_global = connect_global(&controller_endpoint).await?;
    wait_for_auto_registered_profile(&mut restored_global, "machine-failure", "yt-failure").await?;
    let (restored_route, restored_lease) =
        acquire_youtube(&mut restored_global, "fake-failures", "yt-failure").await?;
    if restored_route.agent_grpc_addr != agent_endpoint {
        bail!(
            "expected route after controller restart to {agent_endpoint}, got {}",
            restored_route.agent_grpc_addr
        );
    }
    let restored_lease_context = LeaseContext {
        lease_id: restored_lease.lease_id,
        profile_id: restored_lease.profile_id,
        fencing_token: restored_lease.fencing_token,
    };
    restarted_agent_client
        .install_lease(InstallLeaseRequest {
            lease: Some(restored_lease_context.clone()),
        })
        .await?;
    assert_action_succeeds(&mut restarted_agent_client, restored_lease_context).await?;

    stop_child(&mut restored_controller, "restored controller").await?;
    stop_child(&mut restarted_agent, "restarted agent").await?;

    println!(
        "docker fake-failures e2e passed: browser recovery, agent restart, and controller re-register work"
    );
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

async fn real_web_wsj_main() -> anyhow::Result<()> {
    let controller =
        std::env::var("BCP_CONTROLLER").unwrap_or_else(|_| "http://controller:7000".to_string());
    let mut global = connect_global(&controller).await?;

    for machine in ["a", "b", "c"] {
        let machine_id = format!("machine-{machine}");
        let agent_addr = format!("http://agent-{machine}:7100");
        let profiles = (1..=3)
            .map(|index| {
                real_profile(
                    &format!("wsj-{machine}-{index}"),
                    &machine_id,
                    AccountPlatform::Wsj,
                    &format!("wsj-{machine}-{index}"),
                    &format!("http://172.31.{}.1{index}:9222", machine_octet(machine)),
                )
            })
            .collect();
        register_machine_with_profiles(&mut global, &machine_id, &agent_addr, profiles).await?;
    }

    let mut all_headlines = Vec::new();
    for machine in ["a", "b", "c"] {
        for index in 1..=3 {
            let account_id = format!("wsj-{machine}-{index}");
            let headlines = exercise_wsj_profile(&mut global, &account_id).await?;
            println!(
                "wsj headlines via {account_id}: {}",
                headlines
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
            all_headlines.extend(headlines);
        }
    }

    all_headlines.sort();
    all_headlines.dedup();
    if all_headlines.len() < 5 {
        bail!(
            "expected at least five unique WSJ headline-like texts, got {}",
            all_headlines.len()
        );
    }

    println!(
        "docker wsj e2e passed: collected {} unique headline-like texts across 9 browsers",
        all_headlines.len()
    );
    Ok(())
}

async fn real_web_hn_main() -> anyhow::Result<()> {
    let controller =
        std::env::var("BCP_CONTROLLER").unwrap_or_else(|_| "http://controller:7000".to_string());
    let mut global = connect_global(&controller).await?;

    for machine in ["a", "b", "c"] {
        let machine_id = format!("machine-{machine}");
        let agent_addr = format!("http://agent-{machine}:7100");
        let profiles = (1..=3)
            .map(|index| {
                real_profile(
                    &format!("hn-{machine}-{index}"),
                    &machine_id,
                    AccountPlatform::HackerNews,
                    &format!("hn-{machine}-{index}"),
                    &format!("http://172.32.{}.1{index}:9222", machine_octet(machine)),
                )
            })
            .collect();
        register_machine_with_profiles(&mut global, &machine_id, &agent_addr, profiles).await?;
    }

    let mut all_headlines = Vec::new();
    for machine in ["a", "b", "c"] {
        for index in 1..=3 {
            let account_id = format!("hn-{machine}-{index}");
            let headlines = exercise_hn_profile(&mut global, &account_id).await?;
            println!(
                "hn headlines via {account_id}: {}",
                headlines
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
            all_headlines.extend(headlines);
        }
    }

    all_headlines.sort();
    all_headlines.dedup();
    if all_headlines.len() < 10 {
        bail!(
            "expected at least ten unique Hacker News headline texts, got {}",
            all_headlines.len()
        );
    }

    println!(
        "docker hn e2e passed: collected {} unique headline texts across 9 browsers",
        all_headlines.len()
    );
    Ok(())
}

async fn exercise_wsj_profile(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
    account_id: &str,
) -> anyhow::Result<Vec<String>> {
    let acquired = global
        .acquire_browser(AcquireBrowserRequest {
            client_id: "docker-wsj-e2e".to_string(),
            purpose: "collect-wsj-headlines".to_string(),
            platform: AccountPlatform::Wsj as i32,
            account_id: account_id.to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 120,
        })
        .await?
        .into_inner();
    let route = acquired.route.context("controller did not return route")?;
    let lease = acquired.lease.context("controller did not return lease")?;
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

    let raw = agent
        .evaluate(EvaluateRequest {
            lease: Some(lease_context),
            expression: wsj_headline_expression(),
        })
        .await?
        .into_inner()
        .json_result;
    let extraction: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse WSJ extraction JSON from {account_id}: {raw}"))?;
    let href = extraction
        .get("href")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let title = extraction
        .get("title")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let block_signal = extraction
        .get("blockSignal")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let blocked = extraction
        .get("blocked")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if blocked {
        bail!(
            "WSJ blocked headless Docker browser for {account_id}; href={href}; title={title}; block_signal={block_signal}"
        );
    }

    let headlines = extraction
        .get("headlines")
        .and_then(|value| value.as_array())
        .context("WSJ extraction did not include a headlines array")?
        .iter()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    if headlines.len() < 3 {
        bail!(
            "expected at least three headline-like texts from {account_id}, got {}; href={href}; title={title}; headlines={:?}",
            headlines.len(),
            headlines
        );
    }
    Ok(headlines)
}

async fn exercise_hn_profile(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
    account_id: &str,
) -> anyhow::Result<Vec<String>> {
    let acquired = global
        .acquire_browser(AcquireBrowserRequest {
            client_id: "docker-hn-e2e".to_string(),
            purpose: "collect-hn-headlines".to_string(),
            platform: AccountPlatform::HackerNews as i32,
            account_id: account_id.to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 120,
        })
        .await?
        .into_inner();
    let route = acquired.route.context("controller did not return route")?;
    let lease = acquired.lease.context("controller did not return lease")?;
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

    let raw = agent
        .evaluate(EvaluateRequest {
            lease: Some(lease_context),
            expression: hn_headline_expression(),
        })
        .await?
        .into_inner()
        .json_result;
    let extraction: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse Hacker News extraction JSON from {account_id}: {raw}"))?;
    let href = extraction
        .get("href")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let title = extraction
        .get("title")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let headlines = extraction
        .get("headlines")
        .and_then(|value| value.as_array())
        .context("Hacker News extraction did not include a headlines array")?
        .iter()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    if headlines.len() < 10 {
        bail!(
            "expected at least ten Hacker News headlines from {account_id}, got {}; href={href}; title={title}; headlines={:?}",
            headlines.len(),
            headlines
        );
    }
    Ok(headlines)
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

fn machine_octet(machine: &str) -> i32 {
    match machine {
        "a" => 10,
        "b" => 20,
        "c" => 30,
        _ => 40,
    }
}

fn wsj_headline_expression() -> String {
    r#"
(() => {
  const selectors = [
    "h1", "h2", "h3",
    "[data-testid*='headline']",
    "[class*='headline']",
    "article a",
    "main a"
  ];
  const href = window.location.href;
  const title = document.title || "";
  const bodyText = (document.body && document.body.innerText || "").slice(0, 5000);
  const resourceUrls = performance.getEntriesByType("resource")
    .map((entry) => entry.name || "")
    .filter(Boolean);
  const scriptUrls = Array.from(document.scripts)
    .map((script) => script.src || "")
    .filter(Boolean);
  const blockSignal = [href, title, bodyText, ...resourceUrls, ...scriptUrls]
    .find((value) => /captcha|datadome|captcha-delivery/i.test(value)) || "";
  const blocked =
    blockSignal.length > 0 ||
    /verify you are human|are you a robot/i.test(bodyText) ||
    window.location.hostname.includes("captcha-delivery.com");
  const seen = new Set();
  const headlines = [];
  for (const el of document.querySelectorAll(selectors.join(","))) {
    const text = (el.innerText || el.textContent || "").replace(/\s+/g, " ").trim();
    if (text.length < 20 || text.length > 220) continue;
    if (/^(subscribe|sign in|log in|advertisement|skip to|menu)$/i.test(text)) continue;
    const key = text.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    headlines.push(text);
    if (headlines.length >= 30) break;
  }
  return JSON.stringify({ href, title, blocked, blockSignal, headlines });
})()
"#
    .to_string()
}

fn hn_headline_expression() -> String {
    r#"
(() => {
  const href = window.location.href;
  const title = document.title || "";
  const seen = new Set();
  const headlines = [];
  for (const el of document.querySelectorAll(".titleline > a, tr.athing .title a")) {
    const text = (el.innerText || el.textContent || "").replace(/\s+/g, " ").trim();
    if (text.length < 5 || text.length > 180) continue;
    const key = text.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    headlines.push(text);
    if (headlines.length >= 30) break;
  }
  return JSON.stringify({ href, title, headlines });
})()
"#
    .to_string()
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
    config_path: &Path,
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
        .arg("--config-path")
        .arg(config_path)
        .env("BCP_CONTROLLER", controller_endpoint)
        .env("BCP_CONTROLLER_REGISTER_SECONDS", "1")
        .env("BCP_AGENT_PUBLIC_ADDR", agent_endpoint)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(child)
}

struct RecordingAgentSpec<'a> {
    machine_id: &'a str,
    profile_id: &'a str,
    account_id: &'a str,
    agent_endpoint: &'a str,
}

fn spawn_recording_agent(
    addr: &str,
    db_path: &Path,
    controller_endpoint: &str,
    spec: &RecordingAgentSpec<'_>,
    healthy: bool,
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
        .env("BCP_BROWSER_HEARTBEAT_SECONDS", "60")
        .env("BCP_AGENT_PUBLIC_ADDR", spec.agent_endpoint)
        .env("BCP_MACHINE_ID", spec.machine_id)
        .env("BCP_E2E_PROFILE_ID", spec.profile_id)
        .env("BCP_E2E_ACCOUNT_ID", spec.account_id)
        .env("BCP_E2E_PLATFORM", "youtube")
        .env("BCP_E2E_HEALTHY", if healthy { "true" } else { "false" })
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(child)
}

async fn acquire_youtube(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
    client_id: &str,
    account_id: &str,
) -> anyhow::Result<(BrowserRoute, BrowserLease)> {
    let acquired = global
        .acquire_browser(AcquireBrowserRequest {
            client_id: client_id.to_string(),
            purpose: "fake-failure-recovery".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: account_id.to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 300,
        })
        .await?
        .into_inner();
    let route = acquired.route.context("controller did not return route")?;
    let lease = acquired.lease.context("controller did not return lease")?;
    Ok((route, lease))
}

async fn assert_action_succeeds(
    agent: &mut MachineControllerClient<tonic::transport::Channel>,
    lease: LeaseContext,
) -> anyhow::Result<()> {
    let action = agent
        .execute_action(ExecuteActionRequest {
            lease: Some(lease),
            action: "click".to_string(),
            r#ref: "e1".to_string(),
            text: String::new(),
            key: String::new(),
            options: HashMap::new(),
        })
        .await?
        .into_inner();
    if !action.success {
        bail!(
            "expected fake browser action to succeed: {}",
            action.message
        );
    }
    Ok(())
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

struct FleetEntry {
    machine_id: String,
    agent_addr: String,
    platform: AccountPlatform,
    account_id: String,
    cdp_url: String,
}

fn parse_platform(value: &str) -> AccountPlatform {
    match value.to_ascii_lowercase().as_str() {
        "youtube" => AccountPlatform::Youtube,
        "x" | "twitter" => AccountPlatform::X,
        "douyin" => AccountPlatform::Douyin,
        "tiktok" => AccountPlatform::Tiktok,
        "reddit" => AccountPlatform::Reddit,
        "zhihu" => AccountPlatform::Zhihu,
        "weibo" => AccountPlatform::Weibo,
        "wsj" => AccountPlatform::Wsj,
        "hackernews" | "hn" => AccountPlatform::HackerNews,
        _ => AccountPlatform::Unspecified,
    }
}

/// Fleet spec from `BCP_FLEET`: entries separated by `;`, each field by `|`:
/// `machine_id|agent_grpc_addr|platform|account_id|cdp_url`.
fn parse_fleet(spec: &str) -> anyhow::Result<Vec<FleetEntry>> {
    let mut entries = Vec::new();
    for raw in spec.split(';') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let parts: Vec<&str> = raw.split('|').map(str::trim).collect();
        if parts.len() != 5 {
            bail!("fleet entry must have 5 |-separated fields, got: {raw}");
        }
        entries.push(FleetEntry {
            machine_id: parts[0].to_string(),
            agent_addr: parts[1].to_string(),
            platform: parse_platform(parts[2]),
            account_id: parts[3].to_string(),
            cdp_url: parts[4].to_string(),
        });
    }
    if entries.is_empty() {
        bail!("BCP_FLEET did not contain any fleet entries");
    }
    Ok(entries)
}

/// Drives a real VirtualBox VM fleet: registers every machine, then for each
/// one proves the control plane can route a lease, install it on the owning
/// machine controller, and execute real Chrome DevTools work (a11y snapshot and
/// a JavaScript evaluation) through it, while rejecting a foreign machine's use
/// of the same lease.
async fn vm_fleet_main() -> anyhow::Result<()> {
    let controller =
        std::env::var("BCP_CONTROLLER").unwrap_or_else(|_| "http://127.0.0.1:7000".to_string());
    let spec = std::env::var("BCP_FLEET").context("BCP_FLEET environment variable is required")?;
    let fleet = parse_fleet(&spec)?;
    let mut global = connect_global(&controller).await?;

    for entry in &fleet {
        let profile = real_profile(
            &format!("{}-profile", entry.machine_id),
            &entry.machine_id,
            entry.platform,
            &entry.account_id,
            &entry.cdp_url,
        );
        register_machine_with_profiles(
            &mut global,
            &entry.machine_id,
            &entry.agent_addr,
            vec![profile],
        )
        .await?;
    }

    let machines = global
        .list_machines(ListMachinesRequest {
            label_selector: HashMap::new(),
        })
        .await?
        .into_inner();
    if machines.machines.len() != fleet.len() {
        bail!(
            "expected {} registered machines, got {}",
            fleet.len(),
            machines.machines.len()
        );
    }

    for (index, entry) in fleet.iter().enumerate() {
        let wrong = &fleet[(index + 1) % fleet.len()];
        let ua = exercise_vm_profile(&mut global, entry, &wrong.agent_addr).await?;
        println!(
            "vm-fleet: {} routed + real CDP ok (ua={})",
            entry.machine_id,
            ua.trim()
        );
    }

    println!(
        "vm-fleet e2e passed: {} VMs routed to real CDP browsers across the fleet",
        fleet.len()
    );
    Ok(())
}

async fn exercise_vm_profile(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
    entry: &FleetEntry,
    wrong_agent_addr: &str,
) -> anyhow::Result<String> {
    let acquired = global
        .acquire_browser(AcquireBrowserRequest {
            client_id: "vm-fleet-e2e".to_string(),
            purpose: "verify-vm-fleet".to_string(),
            platform: entry.platform as i32,
            account_id: entry.account_id.clone(),
            label_selector: HashMap::new(),
            ttl_seconds: 120,
        })
        .await?
        .into_inner();
    let route = acquired
        .route
        .context("controller did not return a route")?;
    let lease = acquired
        .lease
        .context("controller did not return a lease")?;
    if route.machine_id != entry.machine_id {
        bail!(
            "expected route to {}, got {}",
            entry.machine_id,
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
    // A successful a11y snapshot proves the machine controller reached a live
    // CDP browser through the pwright gateway.
    agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(lease_context.clone()),
        })
        .await?;
    let ua = agent
        .evaluate(EvaluateRequest {
            lease: Some(lease_context.clone()),
            expression: "navigator.userAgent".to_string(),
        })
        .await?
        .into_inner();
    if !ua.json_result.contains("Chrome") {
        bail!(
            "evaluate did not return a real Chrome user agent for {}: {}",
            entry.machine_id,
            ua.json_result
        );
    }

    let mut wrong_agent = connect_agent(wrong_agent_addr).await?;
    match wrong_agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(lease_context),
        })
        .await
    {
        Err(status)
            if matches!(
                status.code(),
                tonic::Code::PermissionDenied | tonic::Code::NotFound
            ) => {}
        Err(status) => bail!("wrong fleet agent returned unexpected status: {status}"),
        Ok(_) => bail!("wrong fleet agent accepted a lease that was never installed there"),
    }

    Ok(ua.json_result)
}

async fn register_fleet(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
    fleet: &[FleetEntry],
) -> anyhow::Result<()> {
    for entry in fleet {
        let profile = real_profile(
            &format!("{}-profile", entry.machine_id),
            &entry.machine_id,
            entry.platform,
            &entry.account_id,
            &entry.cdp_url,
        );
        register_machine_with_profiles(global, &entry.machine_id, &entry.agent_addr, vec![profile])
            .await?;
    }
    Ok(())
}

async fn raw_acquire(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
    entry: &FleetEntry,
) -> Result<AcquireBrowserResponse, tonic::Status> {
    global
        .acquire_browser(AcquireBrowserRequest {
            client_id: "scenario-e2e".to_string(),
            purpose: "scenario".to_string(),
            platform: entry.platform as i32,
            account_id: entry.account_id.clone(),
            label_selector: HashMap::new(),
            ttl_seconds: 120,
        })
        .await
        .map(tonic::Response::into_inner)
}

/// Acquire + install + ensure + snapshot + eval on the routed machine, without
/// the cross-machine rejection step. Returns the browser user agent.
async fn drive_once(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
    entry: &FleetEntry,
) -> anyhow::Result<String> {
    let acquired = raw_acquire(global, entry)
        .await
        .map_err(|status| anyhow::anyhow!("acquire failed for {}: {status}", entry.machine_id))?;
    let route = acquired.route.context("controller returned no route")?;
    let lease = acquired.lease.context("controller returned no lease")?;
    let lease_context = LeaseContext {
        lease_id: lease.lease_id,
        profile_id: lease.profile_id,
        fencing_token: lease.fencing_token,
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
    agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(lease_context.clone()),
        })
        .await?;
    let ua = agent
        .evaluate(EvaluateRequest {
            lease: Some(lease_context),
            expression: "navigator.userAgent".to_string(),
        })
        .await?
        .into_inner();
    Ok(ua.json_result)
}

/// Single-attempt agent dial with a short timeout, for probing a machine that
/// may be down (avoids `connect_agent`'s 30s retry loop).
async fn dial_and_snapshot(addr: &str, lease: &BrowserLease) -> anyhow::Result<()> {
    let mut agent = tokio::time::timeout(
        Duration::from_secs(4),
        MachineControllerClient::connect(addr.to_string()),
    )
    .await
    .map_err(|_| anyhow::anyhow!("connect timed out"))??;
    let lease_context = LeaseContext {
        lease_id: lease.lease_id.clone(),
        profile_id: lease.profile_id.clone(),
        fencing_token: lease.fencing_token.clone(),
    };
    agent
        .install_lease(InstallLeaseRequest {
            lease: Some(lease_context.clone()),
        })
        .await?;
    agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(lease_context),
        })
        .await?;
    Ok(())
}

async fn scenarios_main() -> anyhow::Result<()> {
    let scenario = std::env::var("BCP_SCENARIO").context("BCP_SCENARIO is required")?;
    let controller =
        std::env::var("BCP_CONTROLLER").unwrap_or_else(|_| "http://127.0.0.1:7000".to_string());
    let mut global = connect_global(&controller).await?;

    match scenario.as_str() {
        "exclusivity" => scenario_exclusivity(&mut global).await,
        "fencing" => scenario_fencing(&mut global).await,
        "fencing-release" => scenario_fencing_release(&mut global).await,
        "auto-offline" => scenario_auto_offline(&mut global).await,
        "quarantine" => scenario_quarantine(&mut global).await,
        "audit" => scenario_audit(&mut global).await,
        "failover" => scenario_failover(&mut global).await,
        "persistence-acquire" => scenario_persistence_acquire(&mut global).await,
        "persistence-verify" => scenario_persistence_verify(&mut global).await,
        other => bail!("unknown BCP_SCENARIO: {other}"),
    }
}

fn fleet_from_env() -> anyhow::Result<Vec<FleetEntry>> {
    let spec = std::env::var("BCP_FLEET").context("BCP_FLEET is required")?;
    parse_fleet(&spec)
}

async fn scenario_exclusivity(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let fleet = fleet_from_env()?;
    register_fleet(global, &fleet).await?;
    let first = &fleet[0];

    let acquired = raw_acquire(global, first)
        .await
        .map_err(|status| anyhow::anyhow!("first acquire failed: {status}"))?;
    let lease = acquired.lease.context("no lease on first acquire")?;
    println!("exclusivity: first acquire of {} ok", first.account_id);

    match raw_acquire(global, first).await {
        Err(status) if status.code() == tonic::Code::NotFound => {
            println!("exclusivity: PASS second acquire of same account denied (NOT_FOUND)");
        }
        Err(status) => bail!("exclusivity: unexpected error on second acquire: {status}"),
        Ok(_) => bail!("exclusivity: VIOLATED — same account was leased twice concurrently"),
    }

    if fleet.len() > 1 {
        raw_acquire(global, &fleet[1])
            .await
            .map_err(|status| anyhow::anyhow!("independent acquire failed: {status}"))?;
        println!(
            "exclusivity: PASS independent account {} acquired concurrently",
            fleet[1].account_id
        );
    }

    global
        .release_lease(ReleaseLeaseRequest {
            lease_id: lease.lease_id.clone(),
            fencing_token: lease.fencing_token.clone(),
        })
        .await?;
    raw_acquire(global, first)
        .await
        .map_err(|status| anyhow::anyhow!("reacquire after release failed: {status}"))?;
    println!("exclusivity: PASS profile reclaimed and reacquired after release");
    println!("SCENARIO exclusivity: PASSED");
    Ok(())
}

async fn scenario_fencing(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let fleet = fleet_from_env()?;
    register_fleet(global, &fleet).await?;
    let entry = &fleet[0];

    let acquired = raw_acquire(global, entry)
        .await
        .map_err(|status| anyhow::anyhow!("acquire failed: {status}"))?;
    let route = acquired.route.context("no route")?;
    let lease1 = acquired.lease.context("no lease")?;
    let mut agent = connect_agent(&route.agent_grpc_addr).await?;
    let lc1 = LeaseContext {
        lease_id: lease1.lease_id.clone(),
        profile_id: lease1.profile_id.clone(),
        fencing_token: lease1.fencing_token.clone(),
    };
    agent
        .install_lease(InstallLeaseRequest {
            lease: Some(lc1.clone()),
        })
        .await?;
    agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(lc1.clone()),
        })
        .await?;
    println!("fencing: valid installed lease works");

    // A: wrong fencing token on a known lease id
    let wrong_token = LeaseContext {
        fencing_token: "bogus-token".to_string(),
        ..lc1.clone()
    };
    match agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(wrong_token),
        })
        .await
    {
        Err(status) if status.code() == tonic::Code::PermissionDenied => {
            println!("fencing: PASS wrong fencing token rejected");
        }
        other => bail!("fencing: wrong token NOT rejected: {other:?}"),
    }

    // B: a lease that was never installed on this machine
    let ghost = LeaseContext {
        lease_id: "ghost-lease".to_string(),
        profile_id: lease1.profile_id.clone(),
        fencing_token: "ghost-token".to_string(),
    };
    match agent
        .get_snapshot(GetSnapshotRequest { lease: Some(ghost) })
        .await
    {
        Err(status) if status.code() == tonic::Code::PermissionDenied => {
            println!("fencing: PASS uninstalled lease rejected");
        }
        other => bail!("fencing: uninstalled lease NOT rejected: {other:?}"),
    }

    // C: superseded lease — release L1 at the controller, acquire L2 on the same
    // profile, install L2, then replay the OLD L1 context against the agent.
    global
        .release_lease(ReleaseLeaseRequest {
            lease_id: lease1.lease_id.clone(),
            fencing_token: lease1.fencing_token.clone(),
        })
        .await?;
    let acquired2 = raw_acquire(global, entry)
        .await
        .map_err(|status| anyhow::anyhow!("reacquire failed: {status}"))?;
    let lease2 = acquired2.lease.context("no lease2")?;
    let lc2 = LeaseContext {
        lease_id: lease2.lease_id.clone(),
        profile_id: lease2.profile_id.clone(),
        fencing_token: lease2.fencing_token.clone(),
    };
    agent
        .install_lease(InstallLeaseRequest {
            lease: Some(lc2.clone()),
        })
        .await?;
    match agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(lc1.clone()),
        })
        .await
    {
        Err(status) if status.code() == tonic::Code::PermissionDenied => {
            println!("fencing: PASS superseded lease correctly revoked at agent");
        }
        Ok(_) => {
            println!(
                "fencing: FINDING/GAP — a released+superseded lease is STILL accepted by the \
                 agent. The agent keys installed leases by lease_id and never uninstalls on \
                 release/expiry, so a client whose lease was revoked can keep driving the \
                 browser. Fix: propagate release/expiry to the machine controller (StopBrowser \
                 or an uninstall RPC), or have the agent revalidate the lease against the \
                 controller."
            );
        }
        Err(status) => println!("fencing: superseded lease errored unexpectedly: {status}"),
    }
    println!("SCENARIO fencing: DONE (A+B pass; C is a documented finding)");
    Ok(())
}

/// Proves the controller revokes a lease at its agent on release even when no
/// successor re-leases the profile (the residual the local-eviction fix left).
async fn scenario_fencing_release(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let fleet = fleet_from_env()?;
    register_fleet(global, &fleet).await?;
    let entry = &fleet[0];

    let acquired = raw_acquire(global, entry)
        .await
        .map_err(|status| anyhow::anyhow!("acquire failed: {status}"))?;
    let route = acquired.route.context("no route")?;
    let lease = acquired.lease.context("no lease")?;
    let context = LeaseContext {
        lease_id: lease.lease_id.clone(),
        profile_id: lease.profile_id.clone(),
        fencing_token: lease.fencing_token.clone(),
    };
    let mut agent = connect_agent(&route.agent_grpc_addr).await?;
    agent
        .install_lease(InstallLeaseRequest {
            lease: Some(context.clone()),
        })
        .await?;
    agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(context.clone()),
        })
        .await?;
    println!("fencing-release: valid lease works");

    global
        .release_lease(ReleaseLeaseRequest {
            lease_id: lease.lease_id.clone(),
            fencing_token: lease.fencing_token.clone(),
        })
        .await?;
    // The controller revokes the lease at the agent asynchronously.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    match agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(context),
        })
        .await
    {
        Err(status) if status.code() == tonic::Code::PermissionDenied => {
            println!("fencing-release: PASS released lease revoked at agent (no successor needed)");
        }
        Ok(_) => bail!("fencing-release: released lease is still valid at the agent"),
        Err(status) => bail!("fencing-release: unexpected status: {status}"),
    }
    println!("SCENARIO fencing-release: PASSED");
    Ok(())
}

/// Proves the controller's background sweep marks machines offline once their
/// registration heartbeat goes stale. Run the controller with a low
/// `BCP_MACHINE_OFFLINE_MS` and `BCP_SWEEP_SECONDS`.
async fn scenario_auto_offline(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let fleet = fleet_from_env()?;
    register_fleet(global, &fleet).await?;
    let wait_ms: u64 = std::env::var("BCP_OFFLINE_WAIT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(6000);
    tokio::time::sleep(Duration::from_millis(wait_ms)).await;

    let machines = global
        .list_machines(ListMachinesRequest {
            label_selector: HashMap::new(),
        })
        .await?
        .into_inner();
    let offline = machines
        .machines
        .iter()
        .filter(|machine| machine.status == MachineStatus::Offline as i32)
        .count();
    if offline < fleet.len() {
        bail!(
            "expected all {} stale machines offline, got {}",
            fleet.len(),
            offline
        );
    }
    println!("auto-offline: PASS the sweep marked {offline} stale machine(s) offline");
    println!("SCENARIO auto-offline: PASSED");
    Ok(())
}

/// Proves a quarantined profile is evicted and excluded from acquire until it is
/// released back to service.
async fn scenario_quarantine(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let fleet = fleet_from_env()?;
    register_fleet(global, &fleet).await?;
    let entry = &fleet[0];
    let profile_id = format!("{}-profile", entry.machine_id);

    // Lease it first so quarantine also has to evict an active lease.
    raw_acquire(global, entry)
        .await
        .map_err(|status| anyhow::anyhow!("initial acquire failed: {status}"))?;
    global
        .quarantine_profile(QuarantineProfileRequest {
            profile_id: profile_id.clone(),
            reason: "scenario".to_string(),
        })
        .await?;

    // Optionally wait so a self-registering agent re-registers (reporting the
    // profile Available) in between: quarantine must survive that heartbeat.
    let wait_ms: u64 = std::env::var("BCP_QUARANTINE_WAIT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if wait_ms > 0 {
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
    }

    match raw_acquire(global, entry).await {
        Err(status) if status.code() == tonic::Code::NotFound => {
            println!("quarantine: PASS acquire blocked while quarantined");
        }
        Ok(_) => bail!("quarantine: acquire succeeded on a quarantined profile"),
        Err(status) => bail!("quarantine: unexpected error: {status}"),
    }

    global
        .release_quarantine(ReleaseQuarantineRequest {
            profile_id: profile_id.clone(),
        })
        .await?;
    raw_acquire(global, entry)
        .await
        .map_err(|status| anyhow::anyhow!("reacquire after release failed: {status}"))?;
    println!("quarantine: PASS profile acquirable again after release");
    println!("SCENARIO quarantine: PASSED");
    Ok(())
}

/// Proves browser operations produce structured audit events that the agent
/// reports to the controller (requires self-registering agents so their
/// telemetry reporter is active).
async fn scenario_audit(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let fleet = fleet_from_env()?;
    register_fleet(global, &fleet).await?;
    let entry = &fleet[0];

    // Drive a real browser operation, which emits browser.* audit events.
    drive_once(global, entry).await?;

    let wait_ms: u64 = std::env::var("BCP_AUDIT_WAIT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8000);
    tokio::time::sleep(Duration::from_millis(wait_ms)).await;

    let events = global
        .list_control_plane_events(ListControlPlaneEventsRequest {
            start_unix_ms: 0,
            end_unix_ms: 0,
            machine_id: entry.machine_id.clone(),
            profile_id: String::new(),
            limit: 200,
        })
        .await?
        .into_inner();
    let browser_events = events
        .events
        .iter()
        .filter(|event| event.event_type.starts_with("browser."))
        .count();
    if browser_events == 0 {
        bail!(
            "no browser.* audit events reported to the controller for {}",
            entry.machine_id
        );
    }
    println!(
        "audit: PASS {browser_events} browser audit event(s) reported for {}",
        entry.machine_id
    );
    println!("SCENARIO audit: PASSED");
    Ok(())
}

async fn scenario_failover(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let fleet = fleet_from_env()?;
    let dead = std::env::var("BCP_DEAD_MACHINE").context("BCP_DEAD_MACHINE is required")?;
    register_fleet(global, &fleet).await?;

    let dead_entry = fleet
        .iter()
        .find(|entry| entry.machine_id == dead)
        .context("BCP_DEAD_MACHINE is not in the fleet")?;
    match raw_acquire(global, dead_entry).await {
        Ok(acquired) => {
            let route = acquired.route.context("no route for dead machine")?;
            let lease = acquired.lease.context("no lease for dead machine")?;
            match dial_and_snapshot(&route.agent_grpc_addr, &lease).await {
                Err(error) => println!(
                    "failover: PASS dead machine {dead} surfaces an error at use-time ({error})"
                ),
                Ok(()) => bail!("failover: dead machine {dead} unexpectedly served a request"),
            }
        }
        Err(status) => {
            println!("failover: controller declined to route to {dead} ({status})");
        }
    }

    let live = fleet
        .iter()
        .find(|entry| entry.machine_id != dead)
        .context("no live machine to verify")?;
    let ua = drive_once(global, live).await?;
    println!(
        "failover: PASS live machine {} still serves real CDP (ua={})",
        live.machine_id,
        ua.trim()
    );
    println!(
        "SCENARIO failover: DONE (note: the controller has no auto-offline sweep — ListMachines \
         still reports {dead} as online; failure is only surfaced when the agent is used)"
    );
    Ok(())
}

async fn scenario_persistence_acquire(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let fleet = fleet_from_env()?;
    register_fleet(global, &fleet).await?;
    let acquired = raw_acquire(global, &fleet[0])
        .await
        .map_err(|status| anyhow::anyhow!("acquire failed: {status}"))?;
    let lease = acquired.lease.context("no lease")?;
    let machines = global
        .list_machines(ListMachinesRequest {
            label_selector: HashMap::new(),
        })
        .await?
        .into_inner();
    println!("PERSIST_LEASE_ID={}", lease.lease_id);
    println!("PERSIST_FENCE={}", lease.fencing_token);
    println!("PERSIST_MACHINE={}", lease.machine_id);
    println!("PERSIST_MACHINE_COUNT={}", machines.machines.len());
    println!("SCENARIO persistence-acquire: DONE");
    Ok(())
}

async fn scenario_persistence_verify(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let lease_id = std::env::var("BCP_LEASE_ID").context("BCP_LEASE_ID is required")?;
    let fence = std::env::var("BCP_FENCE").context("BCP_FENCE is required")?;
    let expect_machine = std::env::var("BCP_MACHINE").unwrap_or_default();
    let expect_count: usize = std::env::var("BCP_MACHINE_COUNT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    let route = global
        .get_route(GetRouteRequest {
            lease_id: lease_id.clone(),
            fencing_token: fence.clone(),
        })
        .await
        .map_err(|status| {
            anyhow::anyhow!("get_route after restart failed — lease was lost: {status}")
        })?
        .into_inner()
        .route
        .context("no route after restart")?;
    if !expect_machine.is_empty() && route.machine_id != expect_machine {
        bail!(
            "route machine changed after restart: expected {expect_machine}, got {}",
            route.machine_id
        );
    }
    let machines = global
        .list_machines(ListMachinesRequest {
            label_selector: HashMap::new(),
        })
        .await?
        .into_inner();
    if machines.machines.len() < expect_count {
        bail!(
            "machines lost after restart: {} < {expect_count}",
            machines.machines.len()
        );
    }
    println!(
        "persistence: PASS lease {} and {} machine(s) survived controller restart",
        lease_id,
        machines.machines.len()
    );
    println!("SCENARIO persistence: PASSED");
    Ok(())
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
