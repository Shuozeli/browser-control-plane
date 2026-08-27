use super::*;

pub(crate) async fn scenarios_main() -> anyhow::Result<()> {
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
        "lease-expiry" => scenario_lease_expiry(&mut global).await,
        "restart-hold" => scenario_restart_hold(&mut global).await,
        "restart-check" => scenario_restart_check().await,
        "failover" => scenario_failover(&mut global).await,
        "persistence-acquire" => scenario_persistence_acquire(&mut global).await,
        "persistence-verify" => scenario_persistence_verify(&mut global).await,
        other => bail!("unknown BCP_SCENARIO: {other}"),
    }
}

pub(crate) fn fleet_from_env() -> anyhow::Result<Vec<FleetEntry>> {
    let spec = std::env::var("BCP_FLEET").context("BCP_FLEET is required")?;
    parse_fleet(&spec)
}

pub(crate) async fn scenario_exclusivity(
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

pub(crate) async fn scenario_fencing(
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
        expires_at_unix_ms: lease1.expires_at_unix_ms,
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
        expires_at_unix_ms: 0,
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
        expires_at_unix_ms: lease2.expires_at_unix_ms,
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
pub(crate) async fn scenario_fencing_release(
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
        expires_at_unix_ms: lease.expires_at_unix_ms,
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
pub(crate) async fn scenario_auto_offline(
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
pub(crate) async fn scenario_quarantine(
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
pub(crate) async fn scenario_audit(
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

/// Proves an expired lease is rejected by the agent itself (run the controller
/// with a long BCP_SWEEP_SECONDS so the only thing rejecting it is the agent's
/// own expiry check, not the controller's revocation).
pub(crate) async fn scenario_lease_expiry(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let fleet = fleet_from_env()?;
    register_fleet(global, &fleet).await?;
    let entry = &fleet[0];

    let acquired = global
        .acquire_browser(AcquireBrowserRequest {
            client_id: "lease-expiry-e2e".to_string(),
            purpose: "verify-expiry".to_string(),
            platform: entry.platform as i32,
            account_id: entry.account_id.clone(),
            label_selector: HashMap::new(),
            ttl_seconds: 3,
        })
        .await?
        .into_inner();
    let route = acquired.route.context("no route")?;
    let lease = acquired.lease.context("no lease")?;
    let context = LeaseContext {
        lease_id: lease.lease_id.clone(),
        profile_id: lease.profile_id.clone(),
        fencing_token: lease.fencing_token.clone(),
        expires_at_unix_ms: lease.expires_at_unix_ms,
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
    println!("lease-expiry: valid lease works");

    // Wait past the 3s TTL. With a long sweep interval the controller has not
    // revoked it, so only the agent's own expiry check can reject it.
    tokio::time::sleep(Duration::from_millis(4500)).await;
    match agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(context),
        })
        .await
    {
        Err(status) if status.code() == tonic::Code::PermissionDenied => {
            println!("lease-expiry: PASS expired lease rejected by the agent");
        }
        Ok(_) => bail!("lease-expiry: expired lease still worked at the agent"),
        Err(status) => bail!("lease-expiry: unexpected status: {status}"),
    }
    println!("SCENARIO lease-expiry: PASSED");
    Ok(())
}

/// Phase 1 of the restart-recovery check: acquire a lease, install it, prove it
/// works, and print its details for phase 2. The lease is intentionally NOT
/// released so it survives an agent restart.
pub(crate) async fn scenario_restart_hold(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
) -> anyhow::Result<()> {
    let fleet = fleet_from_env()?;
    register_fleet(global, &fleet).await?;
    let entry = &fleet[0];
    let acquired = global
        .acquire_browser(AcquireBrowserRequest {
            client_id: "restart-e2e".to_string(),
            purpose: "verify-restart-recovery".to_string(),
            platform: entry.platform as i32,
            account_id: entry.account_id.clone(),
            label_selector: HashMap::new(),
            ttl_seconds: 300,
        })
        .await?
        .into_inner();
    let route = acquired.route.context("no route")?;
    let lease = acquired.lease.context("no lease")?;
    let context = LeaseContext {
        lease_id: lease.lease_id.clone(),
        profile_id: lease.profile_id.clone(),
        fencing_token: lease.fencing_token.clone(),
        expires_at_unix_ms: lease.expires_at_unix_ms,
    };
    let mut agent = connect_agent(&route.agent_grpc_addr).await?;
    agent
        .install_lease(InstallLeaseRequest {
            lease: Some(context.clone()),
        })
        .await?;
    agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(context),
        })
        .await?;
    println!("HELD_LEASE_ID={}", lease.lease_id);
    println!("HELD_FENCE={}", lease.fencing_token);
    println!("HELD_PROFILE={}", lease.profile_id);
    println!("HELD_EXPIRES={}", lease.expires_at_unix_ms);
    println!("HELD_AGENT={}", route.agent_grpc_addr);
    println!("SCENARIO restart-hold: DONE");
    Ok(())
}

/// Phase 2: after the owning agent has been restarted, the held lease must still
/// work because the agent reconciled it from the controller on startup.
pub(crate) async fn scenario_restart_check() -> anyhow::Result<()> {
    let agent_addr = std::env::var("BCP_AGENT").context("BCP_AGENT is required")?;
    let context = LeaseContext {
        lease_id: std::env::var("BCP_LEASE_ID").context("BCP_LEASE_ID is required")?,
        profile_id: std::env::var("BCP_PROFILE").context("BCP_PROFILE is required")?,
        fencing_token: std::env::var("BCP_FENCE").context("BCP_FENCE is required")?,
        expires_at_unix_ms: std::env::var("BCP_EXPIRES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
    };
    let mut agent = connect_agent(&agent_addr).await?;
    agent
        .get_snapshot(GetSnapshotRequest {
            lease: Some(context),
        })
        .await
        .map_err(|status| {
            anyhow::anyhow!("lease did not survive the agent restart (not reconciled): {status}")
        })?;
    println!("restart-check: PASS held lease works after agent restart (reconciled)");
    println!("SCENARIO restart-check: PASSED");
    Ok(())
}

pub(crate) async fn scenario_failover(
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

pub(crate) async fn scenario_persistence_acquire(
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

pub(crate) async fn scenario_persistence_verify(
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
