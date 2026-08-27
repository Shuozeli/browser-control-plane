use super::*;

pub(crate) async fn exercise_wsj_profile(
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
        expires_at_unix_ms: lease.expires_at_unix_ms,
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

pub(crate) async fn exercise_hn_profile(
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
        expires_at_unix_ms: lease.expires_at_unix_ms,
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

pub(crate) async fn exercise_real_profile(
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
        expires_at_unix_ms: lease.expires_at_unix_ms,
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

pub(crate) fn machine_octet(machine: &str) -> i32 {
    match machine {
        "a" => 10,
        "b" => 20,
        "c" => 30,
        _ => 40,
    }
}

pub(crate) fn wsj_headline_expression() -> String {
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

pub(crate) fn hn_headline_expression() -> String {
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

pub(crate) fn spawn_controller(addr: &str, db_path: &Path) -> anyhow::Result<Child> {
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

pub(crate) fn spawn_agent(
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

pub(crate) struct RecordingAgentSpec<'a> {
    pub(crate) machine_id: &'a str,
    pub(crate) profile_id: &'a str,
    pub(crate) account_id: &'a str,
    pub(crate) agent_endpoint: &'a str,
}

pub(crate) fn spawn_recording_agent(
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

pub(crate) async fn acquire_youtube(
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

pub(crate) async fn assert_action_succeeds(
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

pub(crate) async fn stop_child(child: &mut Child, name: &str) -> anyhow::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    child.kill().await.context(format!("kill {name}"))?;
    let _ = child.wait().await;
    Ok(())
}

pub(crate) async fn wait_for_auto_registered_profile(
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

pub(crate) struct FleetEntry {
    pub(crate) machine_id: String,
    pub(crate) agent_addr: String,
    pub(crate) platform: AccountPlatform,
    pub(crate) account_id: String,
    pub(crate) cdp_url: String,
}

pub(crate) fn parse_platform(value: &str) -> AccountPlatform {
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
pub(crate) fn parse_fleet(spec: &str) -> anyhow::Result<Vec<FleetEntry>> {
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

pub(crate) async fn exercise_vm_profile(
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
        expires_at_unix_ms: lease.expires_at_unix_ms,
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

pub(crate) async fn register_fleet(
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

#[allow(clippy::result_large_err)]
pub(crate) async fn raw_acquire(
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
pub(crate) async fn drive_once(
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
        expires_at_unix_ms: lease.expires_at_unix_ms,
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
pub(crate) async fn dial_and_snapshot(addr: &str, lease: &BrowserLease) -> anyhow::Result<()> {
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
        expires_at_unix_ms: lease.expires_at_unix_ms,
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

pub(crate) async fn connect_global(
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

pub(crate) async fn connect_agent(
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

pub(crate) async fn register_machine(
    global: &mut GlobalControllerClient<tonic::transport::Channel>,
    machine_id: &str,
    agent_grpc_addr: &str,
    profile: BrowserProfile,
) -> anyhow::Result<()> {
    register_machine_with_profiles(global, machine_id, agent_grpc_addr, vec![profile]).await
}

pub(crate) async fn register_machine_with_profiles(
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

pub(crate) fn profile(
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

pub(crate) fn real_profile(
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
