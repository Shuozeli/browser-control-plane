use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bcp_agent::AgentService;
use bcp_agent::config::{
    AgentConfig, DiscoveredProfile, GatewayKind, discover_config_path, discover_profiles,
    load_agent_config,
};
use bcp_agent::lifecycle::LifecyclePwrightGateway;
#[cfg(feature = "real-pwright")]
use bcp_core::pwright::{RealPwrightGateway, RealPwrightProfile};
use bcp_core::pwright::{RecordingProfileState, RecordingPwrightGateway};
use bcp_proto::browsercontrol::v1::global_controller_client::GlobalControllerClient;
use bcp_proto::browsercontrol::v1::machine_controller_server::MachineControllerServer;
use bcp_proto::browsercontrol::v1::{
    A11yNode, Account, AccountPlatform, BrowserProfile, Machine, MachineStatus, ProfileStatus,
    RegisterMachineRequest, ReportTelemetryRequest,
};
use clap::Parser;
use prost::Message;
use rusqlite::{Connection, params};

#[derive(Debug, Parser)]
#[command(name = "bcp-agent", about = "Per-machine browser controller")]
struct Args {
    /// Address to bind. Defaults to TAILSCALE_IP:7100, then 0.0.0.0:7100.
    #[arg(long, env = "BCP_AGENT_ADDR")]
    addr: Option<SocketAddr>,

    /// SQLite database path for local agent profile state.
    #[arg(long, env = "BCP_AGENT_DB")]
    db_path: Option<PathBuf>,

    /// TOML profile discovery config. Defaults to BCP_AGENT_CONFIG, then .bcp/agent.toml.
    #[arg(long, env = "BCP_AGENT_CONFIG")]
    config_path: Option<PathBuf>,
}

struct RuntimeConfig {
    service: AgentService,
    machine_labels: std::collections::HashMap<String, String>,
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
    tracing::info!(%addr, db_path = %db_path.display(), "starting machine controller");
    let runtime = build_runtime(args.config_path)?;
    let service = runtime.service;
    if service.list_profiles().is_empty() {
        hydrate_profiles_from_sqlite(&service, &db_path)?;
    }
    persist_profiles_to_sqlite(&service, &db_path)?;
    spawn_fleet_heartbeat(service.clone());
    spawn_artifact_cleanup(service.clone());
    spawn_controller_registration(service.clone(), addr, runtime.machine_labels);
    spawn_telemetry_reporter(service.clone());

    tonic::transport::Server::builder()
        .add_service(MachineControllerServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}

fn default_addr() -> SocketAddr {
    let host = std::env::var("TAILSCALE_IP").unwrap_or_else(|_| "0.0.0.0".to_string());
    format!("{host}:7100")
        .parse()
        .expect("default agent address should parse")
}

fn default_db_path() -> PathBuf {
    PathBuf::from(".bcp").join("agent.sqlite")
}

fn build_runtime(config_path: Option<PathBuf>) -> anyhow::Result<RuntimeConfig> {
    if let Some(path) = discover_config_path(config_path) {
        let config = load_agent_config(&path)?;
        tracing::info!(config_path = %path.display(), "loaded agent profile config");
        return build_runtime_from_config(config);
    }
    Ok(RuntimeConfig {
        service: build_service_from_env(),
        machine_labels: parse_labels_env("BCP_MACHINE_LABELS"),
    })
}

fn build_runtime_from_config(config: AgentConfig) -> anyhow::Result<RuntimeConfig> {
    let discovered = discover_profiles(&config)?;
    let service = match config.gateway {
        GatewayKind::Recording => {
            build_recording_service_from_discovered(&config.machine_id, discovered)
        }
        GatewayKind::RealPwright => {
            build_real_service_from_discovered(&config.machine_id, discovered)?
        }
    };
    Ok(RuntimeConfig {
        service,
        machine_labels: config.labels,
    })
}

fn build_recording_service_from_discovered(
    machine_id: &str,
    discovered: Vec<DiscoveredProfile>,
) -> AgentService {
    let lifecycles = lifecycle_map(&discovered);
    let states: Vec<RecordingProfileState> = discovered
        .into_iter()
        .map(|entry| RecordingProfileState {
            profile: entry.profile,
            healthy: true,
            health_message: "recording pwright gateway ok".to_string(),
            snapshot: vec![A11yNode {
                r#ref: "e1".to_string(),
                role: "button".to_string(),
                name: "Publish".to_string(),
                depth: 1,
                value: String::new(),
            }],
            eval_json: r#"{"source":"recording_gateway"}"#.to_string(),
        })
        .collect();
    let gateway = LifecyclePwrightGateway::maybe_wrap(
        Arc::new(RecordingPwrightGateway::new(states.clone())),
        lifecycles,
    );
    let service = AgentService::new_for_machine(machine_id, gateway);
    for state in states {
        service.upsert_desired_profile(state.profile);
    }
    service
}

#[cfg(feature = "real-pwright")]
fn build_real_service_from_discovered(
    machine_id: &str,
    discovered: Vec<DiscoveredProfile>,
) -> anyhow::Result<AgentService> {
    let lifecycles = lifecycle_map(&discovered);
    let profiles: Vec<RealPwrightProfile> = discovered
        .into_iter()
        .map(|entry| RealPwrightProfile {
            profile: entry.profile,
            initial_url: entry.initial_url,
        })
        .collect();
    let gateway = LifecyclePwrightGateway::maybe_wrap(
        Arc::new(RealPwrightGateway::new(profiles.clone())),
        lifecycles,
    );
    let service = AgentService::new_for_machine(machine_id, gateway);
    for profile in profiles {
        service.upsert_desired_profile(profile.profile);
    }
    Ok(service)
}

fn lifecycle_map(
    discovered: &[DiscoveredProfile],
) -> std::collections::HashMap<String, bcp_agent::config::LifecycleConfig> {
    discovered
        .iter()
        .filter_map(|entry| {
            entry
                .lifecycle
                .clone()
                .map(|lifecycle| (entry.profile.profile_id.clone(), lifecycle))
        })
        .collect()
}

#[cfg(not(feature = "real-pwright"))]
fn build_real_service_from_discovered(
    _machine_id: &str,
    _discovered: Vec<DiscoveredProfile>,
) -> anyhow::Result<AgentService> {
    anyhow::bail!(
        "gateway = 'real-pwright' requires building bcp-agent with --features real-pwright"
    )
}

fn build_service_from_env() -> AgentService {
    if let Ok(real_profiles) = std::env::var("BCP_REAL_PROFILES") {
        return build_real_service_from_env(&real_profiles);
    }

    let machine_id =
        std::env::var("BCP_MACHINE_ID").unwrap_or_else(|_| "local-machine".to_string());
    let profile_id = match std::env::var("BCP_E2E_PROFILE_ID") {
        Ok(value) => value,
        Err(_) => {
            return AgentService::new_for_machine(
                &machine_id,
                Arc::new(RecordingPwrightGateway::default()),
            );
        }
    };
    let account_id =
        std::env::var("BCP_E2E_ACCOUNT_ID").unwrap_or_else(|_| "recording-account".to_string());
    let platform = std::env::var("BCP_E2E_PLATFORM")
        .ok()
        .and_then(|value| parse_platform(&value))
        .unwrap_or(AccountPlatform::Youtube);
    let healthy = std::env::var("BCP_E2E_HEALTHY")
        .ok()
        .and_then(|value| parse_bool(&value))
        .unwrap_or(true);
    let health_message = std::env::var("BCP_E2E_HEALTH_MESSAGE").unwrap_or_else(|_| {
        if healthy {
            "recording pwright gateway ok".to_string()
        } else {
            "recording pwright gateway crashed".to_string()
        }
    });

    let profile = BrowserProfile {
        profile_id: profile_id.clone(),
        machine_id: machine_id.clone(),
        profile_path: format!("/fake-profiles/{profile_id}"),
        display_name: profile_id.clone(),
        status: ProfileStatus::Available as i32,
        cdp_url: "recording://pwright-gateway".to_string(),
        cdp_port: 0,
        accounts: vec![Account {
            account_id,
            platform: platform as i32,
            handle: "@fake".to_string(),
            health: "logged_in".to_string(),
            capabilities: vec!["snapshot".to_string(), "click".to_string()],
        }],
        labels: Default::default(),
        last_seen_unix_ms: 0,
    };
    let gateway = RecordingPwrightGateway::new([RecordingProfileState {
        profile: profile.clone(),
        healthy,
        health_message,
        snapshot: vec![A11yNode {
            r#ref: "e1".to_string(),
            role: "button".to_string(),
            name: "Publish".to_string(),
            depth: 1,
            value: String::new(),
        }],
        eval_json: r#"{"source":"recording_gateway"}"#.to_string(),
    }]);
    let service = AgentService::new_for_machine(&machine_id, Arc::new(gateway));
    service.upsert_desired_profile(profile);
    service
}

fn hydrate_profiles_from_sqlite(service: &AgentService, db_path: &Path) -> anyhow::Result<()> {
    let conn = Connection::open(db_path)?;
    init_agent_store(&conn)?;
    for profile in load_agent_profiles(&conn)? {
        service.upsert_desired_profile(profile);
    }
    Ok(())
}

fn persist_profiles_to_sqlite(service: &AgentService, db_path: &Path) -> anyhow::Result<()> {
    let mut conn = Connection::open(db_path)?;
    init_agent_store(&conn)?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM agent_profiles", [])?;
    {
        let mut stmt =
            tx.prepare("INSERT INTO agent_profiles (profile_id, payload) VALUES (?1, ?2)")?;
        for profile in service.list_profiles() {
            stmt.execute(params![profile.profile_id, profile.encode_to_vec()])?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn init_agent_store(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_profiles (
            profile_id TEXT PRIMARY KEY,
            payload BLOB NOT NULL
        );",
    )?;
    Ok(())
}

fn load_agent_profiles(conn: &Connection) -> anyhow::Result<Vec<BrowserProfile>> {
    let mut stmt = conn.prepare("SELECT payload FROM agent_profiles ORDER BY profile_id ASC")?;
    let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    let mut profiles = Vec::new();
    for row in rows {
        let payload = row?;
        profiles.push(BrowserProfile::decode(payload.as_slice())?);
    }
    Ok(profiles)
}

#[cfg(feature = "real-pwright")]
fn build_real_service_from_env(real_profiles: &str) -> AgentService {
    let machine_id = std::env::var("BCP_MACHINE_ID").unwrap_or_else(|_| "real-machine".to_string());
    let profiles: Vec<RealPwrightProfile> = real_profiles
        .split(';')
        .filter(|entry| !entry.trim().is_empty())
        .map(|entry| parse_real_profile(&machine_id, entry))
        .collect();
    let gateway = Arc::new(RealPwrightGateway::new(profiles.clone()));
    let service = AgentService::new_for_machine(&machine_id, gateway);
    for profile in profiles {
        service.upsert_desired_profile(profile.profile);
    }
    service
}

#[cfg(not(feature = "real-pwright"))]
fn build_real_service_from_env(_real_profiles: &str) -> AgentService {
    panic!("BCP_REAL_PROFILES requires building bcp-agent with --features real-pwright");
}

#[cfg(feature = "real-pwright")]
fn parse_real_profile(machine_id: &str, entry: &str) -> RealPwrightProfile {
    let parts: Vec<&str> = entry.trim().split('|').map(str::trim).collect();
    assert!(
        parts.len() >= 5,
        "BCP_REAL_PROFILES entries must be profile_id|account_id|platform|cdp_url|initial_url"
    );
    let profile_id = parts[0].to_string();
    let account_id = parts[1].to_string();
    let platform = parse_platform(parts[2]).unwrap_or(AccountPlatform::Unspecified);
    let cdp_url = parts[3].to_string();
    let initial_url = parts[4].to_string();
    let cdp_port = cdp_url
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<i32>().ok())
        .unwrap_or(9222);
    RealPwrightProfile {
        initial_url: initial_url.clone(),
        profile: BrowserProfile {
            profile_id: profile_id.clone(),
            machine_id: machine_id.to_string(),
            profile_path: format!("/real-profiles/{profile_id}"),
            display_name: profile_id.clone(),
            status: ProfileStatus::Available as i32,
            cdp_url,
            cdp_port,
            accounts: vec![Account {
                account_id,
                platform: platform as i32,
                handle: format!("@{profile_id}"),
                health: "logged_in".to_string(),
                capabilities: vec![
                    "snapshot".to_string(),
                    "click".to_string(),
                    "eval".to_string(),
                ],
            }],
            labels: Default::default(),
            last_seen_unix_ms: 0,
        },
    }
}

fn parse_platform(value: &str) -> Option<AccountPlatform> {
    match value.to_ascii_lowercase().as_str() {
        "youtube" => Some(AccountPlatform::Youtube),
        "x" | "twitter" => Some(AccountPlatform::X),
        "douyin" => Some(AccountPlatform::Douyin),
        "tiktok" => Some(AccountPlatform::Tiktok),
        "reddit" => Some(AccountPlatform::Reddit),
        "zhihu" => Some(AccountPlatform::Zhihu),
        "weibo" => Some(AccountPlatform::Weibo),
        "wsj" => Some(AccountPlatform::Wsj),
        "hn" | "hacker-news" | "hackernews" => Some(AccountPlatform::HackerNews),
        _ => None,
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" => Some(true),
        "0" | "false" | "no" | "n" => Some(false),
        _ => None,
    }
}

fn spawn_fleet_heartbeat(service: AgentService) {
    let interval_seconds = std::env::var("BCP_BROWSER_HEARTBEAT_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(10)
        .max(1);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_seconds));
        loop {
            interval.tick().await;
            service.reconcile_fleet_once().await;
        }
    });
}

fn spawn_telemetry_reporter(service: AgentService) {
    let Ok(controller) = std::env::var("BCP_CONTROLLER") else {
        return;
    };
    let interval_seconds = std::env::var("BCP_TELEMETRY_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(5)
        .max(1);
    let machine_id = service.machine_id();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_seconds));
        loop {
            interval.tick().await;
            let telemetry = service.drain_fleet_telemetry();
            if telemetry.samples.is_empty() && telemetry.events.is_empty() {
                continue;
            }
            let request = ReportTelemetryRequest {
                reporter_machine_id: machine_id.clone(),
                samples: telemetry.samples.clone(),
                events: telemetry.events.clone(),
            };
            let connect = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                GlobalControllerClient::connect(controller.clone()),
            )
            .await;
            let sent = match connect {
                Ok(Ok(mut client)) => match client.report_telemetry(request).await {
                    Ok(_) => true,
                    Err(error) => {
                        tracing::warn!(%error, controller, "telemetry report failed");
                        false
                    }
                },
                _ => {
                    tracing::warn!(controller, "telemetry controller unreachable");
                    false
                }
            };
            // Never drop audit events on a transient controller outage: put the
            // batch back so the next tick retries it.
            if !sent {
                service.requeue_fleet_telemetry(telemetry);
            }
        }
    });
}

fn spawn_artifact_cleanup(service: AgentService) {
    let interval_seconds = std::env::var("BCP_ARTIFACT_CLEANUP_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60)
        .max(1);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_seconds));
        loop {
            interval.tick().await;
            match service.cleanup_expired_artifacts() {
                Ok(deleted) if !deleted.is_empty() => {
                    tracing::info!(deleted = deleted.len(), "cleaned expired artifacts");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "artifact cleanup failed");
                }
            }
        }
    });
}

fn spawn_controller_registration(
    service: AgentService,
    bind_addr: SocketAddr,
    machine_labels: std::collections::HashMap<String, String>,
) {
    let Ok(controller) = std::env::var("BCP_CONTROLLER") else {
        return;
    };
    let agent_grpc_addr = public_agent_addr(bind_addr);
    let interval_seconds = std::env::var("BCP_CONTROLLER_REGISTER_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(10)
        .max(1);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_seconds));
        loop {
            interval.tick().await;
            if let Err(error) =
                register_with_controller(&controller, &agent_grpc_addr, &service, &machine_labels)
                    .await
            {
                tracing::warn!(%error, controller, "controller registration failed");
            }
        }
    });
}

async fn register_with_controller(
    controller: &str,
    agent_grpc_addr: &str,
    service: &AgentService,
    machine_labels: &std::collections::HashMap<String, String>,
) -> anyhow::Result<()> {
    let machine_id = service.machine_id();
    let mut client = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        GlobalControllerClient::connect(controller.to_string()),
    )
    .await
    .map_err(|_| anyhow::anyhow!("controller connect timed out"))??;
    client
        .register_machine(RegisterMachineRequest {
            machine: Some(Machine {
                machine_id: machine_id.clone(),
                hostname: std::env::var("HOSTNAME").unwrap_or_else(|_| machine_id.clone()),
                tailscale_host: std::env::var("TAILSCALE_HOST").unwrap_or_default(),
                agent_grpc_addr: agent_grpc_addr.to_string(),
                status: MachineStatus::Online as i32,
                labels: machine_labels.clone(),
                last_heartbeat_unix_ms: 0,
            }),
            profiles: service.list_profiles(),
        })
        .await?;
    Ok(())
}

fn public_agent_addr(bind_addr: SocketAddr) -> String {
    if let Ok(value) = std::env::var("BCP_AGENT_PUBLIC_ADDR") {
        return value;
    }
    if let Ok(value) = std::env::var("BCP_AGENT_GRPC_ADDR") {
        return value;
    }
    if let Ok(host) = std::env::var("TAILSCALE_HOST") {
        return format!("http://{}:{}", host.trim_end_matches('.'), bind_addr.port());
    }
    if bind_addr.ip().is_unspecified() {
        return format!("http://127.0.0.1:{}", bind_addr.port());
    }
    format!("http://{}:{}", bind_addr.ip(), bind_addr.port())
}

fn parse_labels_env(name: &str) -> std::collections::HashMap<String, String> {
    std::env::var(name)
        .ok()
        .map(|value| {
            value
                .split(',')
                .filter_map(|entry| {
                    let (key, value) = entry.split_once('=')?;
                    Some((key.trim().to_string(), value.trim().to_string()))
                })
                .filter(|(key, _)| !key.is_empty())
                .collect()
        })
        .unwrap_or_default()
}
