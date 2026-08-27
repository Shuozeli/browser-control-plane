use super::*;

pub(crate) async fn recording_main() -> anyhow::Result<()> {
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
        expires_at_unix_ms: lease.expires_at_unix_ms,
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

pub(crate) async fn sqlite_persistence_main() -> anyhow::Result<()> {
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
        expires_at_unix_ms: lease.expires_at_unix_ms,
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

pub(crate) async fn fake_failures_main() -> anyhow::Result<()> {
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
        expires_at_unix_ms: lease.expires_at_unix_ms,
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
        expires_at_unix_ms: new_lease.expires_at_unix_ms,
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
        expires_at_unix_ms: restored_lease.expires_at_unix_ms,
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

pub(crate) async fn real_browser_main() -> anyhow::Result<()> {
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

pub(crate) async fn real_web_wsj_main() -> anyhow::Result<()> {
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

pub(crate) async fn real_web_hn_main() -> anyhow::Result<()> {
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

pub(crate) async fn vm_fleet_main() -> anyhow::Result<()> {
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
