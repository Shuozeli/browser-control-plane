use std::net::SocketAddr;
use std::sync::Arc;

use bcp_agent::AgentService;
#[cfg(feature = "real-pwright")]
use bcp_core::pwright::{RealPwrightGateway, RealPwrightProfile};
use bcp_core::pwright::{RecordingProfileState, RecordingPwrightGateway};
use bcp_proto::browsercontrol::v1::machine_controller_server::MachineControllerServer;
use bcp_proto::browsercontrol::v1::{
    A11yNode, Account, AccountPlatform, BrowserProfile, ProfileStatus,
};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "bcp-agent", about = "Per-machine browser controller")]
struct Args {
    /// Address to bind. Defaults to TAILSCALE_IP:7100, then 0.0.0.0:7100.
    #[arg(long, env = "BCP_AGENT_ADDR")]
    addr: Option<SocketAddr>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let addr = args.addr.unwrap_or_else(default_addr);
    tracing::info!(%addr, "starting machine controller");
    let service = build_service_from_env();
    spawn_fleet_heartbeat(service.clone());
    spawn_artifact_cleanup(service.clone());

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

fn build_service_from_env() -> AgentService {
    if let Ok(real_profiles) = std::env::var("BCP_REAL_PROFILES") {
        return build_real_service_from_env(&real_profiles);
    }

    let profile_id = match std::env::var("BCP_E2E_PROFILE_ID") {
        Ok(value) => value,
        Err(_) => return AgentService::default(),
    };
    let account_id =
        std::env::var("BCP_E2E_ACCOUNT_ID").unwrap_or_else(|_| "recording-account".to_string());
    let platform = std::env::var("BCP_E2E_PLATFORM")
        .ok()
        .and_then(|value| parse_platform(&value))
        .unwrap_or(AccountPlatform::Youtube);
    let machine_id =
        std::env::var("BCP_MACHINE_ID").unwrap_or_else(|_| "recording-machine".to_string());

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
    }]);
    let service = AgentService::new_for_machine(&machine_id, Arc::new(gateway));
    service.upsert_desired_profile(profile);
    service
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
