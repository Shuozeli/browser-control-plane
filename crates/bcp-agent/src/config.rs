use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use bcp_proto::browsercontrol::v1::{Account, AccountPlatform, BrowserProfile, ProfileStatus};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub machine_id: String,
    pub gateway: GatewayKind,
    pub labels: HashMap<String, String>,
    pub profiles: Vec<ProfileConfig>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GatewayKind {
    Recording,
    RealPwright,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileConfig {
    pub profile_id: String,
    pub account_id: String,
    pub platform: String,
    pub profile_path: String,
    pub display_name: String,
    pub cdp_url: String,
    pub cdp_port: Option<i32>,
    pub initial_url: String,
    pub handle: String,
    pub health: String,
    pub capabilities: Vec<String>,
    pub labels: HashMap<String, String>,
    pub accounts: Vec<AccountConfig>,
    pub lifecycle: Option<LifecycleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountConfig {
    pub account_id: String,
    pub platform: String,
    pub handle: String,
    pub health: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct LifecycleConfig {
    pub launch_command: Vec<String>,
    pub working_dir: String,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct DiscoveredProfile {
    pub profile: BrowserProfile,
    pub initial_url: String,
    pub lifecycle: Option<LifecycleConfig>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            machine_id: default_machine_id(),
            gateway: GatewayKind::Recording,
            labels: HashMap::new(),
            profiles: Vec::new(),
        }
    }
}

impl Default for ProfileConfig {
    fn default() -> Self {
        Self {
            profile_id: String::new(),
            account_id: String::new(),
            platform: String::new(),
            profile_path: String::new(),
            display_name: String::new(),
            cdp_url: String::new(),
            cdp_port: None,
            initial_url: String::new(),
            handle: String::new(),
            health: "unknown".to_string(),
            capabilities: vec![
                "snapshot".to_string(),
                "click".to_string(),
                "eval".to_string(),
            ],
            labels: HashMap::new(),
            accounts: Vec::new(),
            lifecycle: None,
        }
    }
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            account_id: String::new(),
            platform: String::new(),
            handle: String::new(),
            health: "unknown".to_string(),
            capabilities: vec![
                "snapshot".to_string(),
                "click".to_string(),
                "eval".to_string(),
            ],
        }
    }
}

pub fn discover_config_path(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path);
    }
    if let Ok(path) = std::env::var("BCP_AGENT_CONFIG")
        && !path.trim().is_empty()
    {
        return Some(PathBuf::from(path));
    }
    let local = PathBuf::from(".bcp").join("agent.toml");
    local.exists().then_some(local)
}

pub fn load_agent_config(path: &Path) -> anyhow::Result<AgentConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read agent config {}", path.display()))?;
    parse_agent_config(&raw)
}

pub fn parse_agent_config(raw: &str) -> anyhow::Result<AgentConfig> {
    let config: AgentConfig = toml::from_str(raw).context("parse agent config TOML")?;
    validate_agent_config(&config)?;
    Ok(config)
}

pub fn discover_profiles(config: &AgentConfig) -> anyhow::Result<Vec<DiscoveredProfile>> {
    config
        .profiles
        .iter()
        .map(|profile| profile.discover(&config.machine_id))
        .collect()
}

fn validate_agent_config(config: &AgentConfig) -> anyhow::Result<()> {
    if config.machine_id.trim().is_empty() {
        bail!("machine_id is required");
    }
    if config.profiles.is_empty() {
        bail!("at least one [[profiles]] entry is required");
    }
    for profile in &config.profiles {
        profile.validate()?;
    }
    Ok(())
}

impl ProfileConfig {
    fn validate(&self) -> anyhow::Result<()> {
        if self.profile_id.trim().is_empty() {
            bail!("profiles.profile_id is required");
        }
        if self.account_id.trim().is_empty() && self.accounts.is_empty() {
            bail!(
                "profile '{}' requires account_id/platform or accounts",
                self.profile_id
            );
        }
        if !self.account_id.trim().is_empty() && self.platform.trim().is_empty() {
            bail!(
                "profile '{}' has account_id but no platform",
                self.profile_id
            );
        }
        if self.profile_path.trim().is_empty() {
            bail!("profile '{}' requires profile_path", self.profile_id);
        }
        for account in &self.accounts {
            if account.account_id.trim().is_empty() {
                bail!(
                    "profile '{}' has account without account_id",
                    self.profile_id
                );
            }
            if account.platform.trim().is_empty() {
                bail!("profile '{}' has account without platform", self.profile_id);
            }
        }
        Ok(())
    }

    fn discover(&self, machine_id: &str) -> anyhow::Result<DiscoveredProfile> {
        let cdp_url = if self.cdp_url.trim().is_empty() {
            String::new()
        } else {
            self.cdp_url.clone()
        };
        let cdp_port = self
            .cdp_port
            .unwrap_or_else(|| parse_cdp_port(&cdp_url).unwrap_or_default());
        let mut labels = self.labels.clone();
        if self.lifecycle.is_some() {
            labels.insert("bcp.lifecycle".to_string(), "managed".to_string());
        }
        let profile = BrowserProfile {
            profile_id: self.profile_id.clone(),
            machine_id: machine_id.to_string(),
            profile_path: self.profile_path.clone(),
            display_name: if self.display_name.trim().is_empty() {
                self.profile_id.clone()
            } else {
                self.display_name.clone()
            },
            status: ProfileStatus::Available as i32,
            cdp_url,
            cdp_port,
            accounts: self.discover_accounts()?,
            labels,
            last_seen_unix_ms: 0,
        };
        Ok(DiscoveredProfile {
            profile,
            initial_url: self.initial_url.clone(),
            lifecycle: self.lifecycle.clone(),
        })
    }

    fn discover_accounts(&self) -> anyhow::Result<Vec<Account>> {
        let mut accounts = Vec::new();
        if !self.account_id.trim().is_empty() {
            accounts.push(account_from_parts(
                &self.account_id,
                &self.platform,
                &self.handle,
                &self.health,
                &self.capabilities,
            )?);
        }
        for account in &self.accounts {
            accounts.push(account_from_parts(
                &account.account_id,
                &account.platform,
                &account.handle,
                &account.health,
                &account.capabilities,
            )?);
        }
        Ok(accounts)
    }
}

fn account_from_parts(
    account_id: &str,
    platform: &str,
    handle: &str,
    health: &str,
    capabilities: &[String],
) -> anyhow::Result<Account> {
    let platform = parse_platform(platform)
        .ok_or_else(|| anyhow::anyhow!("unsupported platform '{platform}'"))?;
    Ok(Account {
        account_id: account_id.to_string(),
        platform: platform as i32,
        handle: if handle.trim().is_empty() {
            format!("@{account_id}")
        } else {
            handle.to_string()
        },
        health: health.to_string(),
        capabilities: capabilities.to_vec(),
    })
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

fn parse_cdp_port(cdp_url: &str) -> Option<i32> {
    cdp_url
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<i32>().ok())
}

fn default_machine_id() -> String {
    std::env::var("BCP_MACHINE_ID").unwrap_or_else(|_| "local-machine".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_profile_config() {
        // Arrange
        let raw = r#"
machine_id = "mac-mini-1"
gateway = "recording"

[labels]
site = "home"

[[profiles]]
profile_id = "yt-main"
account_id = "yt-main"
platform = "youtube"
profile_path = "/profiles/yt-main"
display_name = "YouTube Main"
cdp_url = "http://127.0.0.1:9222"
initial_url = "https://studio.youtube.com"
capabilities = ["snapshot", "click"]

[profiles.lifecycle]
launch_command = ["google-chrome", "--remote-debugging-port=9222"]
"#;

        // Act
        let config = parse_agent_config(raw).unwrap();
        let discovered = discover_profiles(&config).unwrap();

        // Assert
        assert_eq!(config.machine_id, "mac-mini-1");
        assert_eq!(config.gateway, GatewayKind::Recording);
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].profile.profile_id, "yt-main");
        assert_eq!(discovered[0].profile.cdp_port, 9222);
        assert_eq!(discovered[0].profile.accounts[0].account_id, "yt-main");
        assert_eq!(
            discovered[0].profile.labels.get("bcp.lifecycle").unwrap(),
            "managed"
        );
    }

    #[test]
    fn rejects_profile_without_account_mapping() {
        // Arrange
        let raw = r#"
machine_id = "mac-mini-1"

[[profiles]]
profile_id = "empty"
profile_path = "/profiles/empty"
"#;

        // Act
        let error = parse_agent_config(raw).unwrap_err();

        // Assert
        assert!(error.to_string().contains("requires account_id/platform"));
    }
}
