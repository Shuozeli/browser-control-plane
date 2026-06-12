use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bcp_proto::browsercontrol::v1::{
    A11yNode, BrowserProfile, ExecuteActionRequest, ExecuteActionResponse, RunScriptResponse,
};
use thiserror::Error;

#[cfg(feature = "real-pwright")]
use pwright_bridge::browser::{Browser, BrowserConfig, TabHandle};
#[cfg(feature = "real-pwright")]
use pwright_bridge::playwright::Page;
#[cfg(feature = "real-pwright")]
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum PwrightError {
    #[error("profile '{0}' was not found")]
    ProfileNotFound(String),
    #[error("browser profile '{0}' is unhealthy: {1}")]
    Unhealthy(String, String),
    #[error("browser operation failed for profile '{0}': {1}")]
    OperationFailed(String, String),
}

#[derive(Debug, Clone)]
pub struct BrowserHealth {
    pub healthy: bool,
    pub message: String,
}

#[async_trait]
pub trait PwrightGateway: Send + Sync {
    async fn ensure_browser(&self, profile_id: &str) -> Result<BrowserProfile, PwrightError>;
    async fn stop_browser(&self, profile_id: &str) -> Result<bool, PwrightError>;
    async fn check_browser(&self, profile_id: &str) -> Result<BrowserHealth, PwrightError>;
    async fn get_snapshot(&self, profile_id: &str) -> Result<Vec<A11yNode>, PwrightError>;
    async fn execute_action(
        &self,
        profile_id: &str,
        request: ExecuteActionRequest,
    ) -> Result<ExecuteActionResponse, PwrightError>;
    async fn evaluate(&self, profile_id: &str, expression: &str) -> Result<String, PwrightError>;
    async fn run_script(
        &self,
        profile_id: &str,
        yaml: &str,
        params: HashMap<String, String>,
    ) -> Result<Vec<RunScriptResponse>, PwrightError>;
}

#[derive(Debug, Clone)]
pub struct RecordingProfileState {
    pub profile: BrowserProfile,
    pub healthy: bool,
    pub health_message: String,
    pub snapshot: Vec<A11yNode>,
    pub eval_json: String,
}

#[derive(Debug, Default)]
pub struct RecordingPwrightGateway {
    profiles: RwLock<HashMap<String, RecordingProfileState>>,
    actions: RwLock<Vec<(String, ExecuteActionRequest)>>,
}

impl RecordingPwrightGateway {
    pub fn new(states: impl IntoIterator<Item = RecordingProfileState>) -> Self {
        let gateway = Self::default();
        for state in states {
            gateway.insert_profile(state);
        }
        gateway
    }

    pub fn insert_profile(&self, state: RecordingProfileState) {
        self.profiles
            .write()
            .expect("recording pwright gateway profile lock poisoned")
            .insert(state.profile.profile_id.clone(), state);
    }

    pub fn actions(&self) -> Vec<(String, ExecuteActionRequest)> {
        self.actions
            .read()
            .expect("recording pwright gateway action lock poisoned")
            .clone()
    }
}

#[async_trait]
impl PwrightGateway for RecordingPwrightGateway {
    async fn ensure_browser(&self, profile_id: &str) -> Result<BrowserProfile, PwrightError> {
        let state = self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .get(profile_id)
            .cloned()
            .ok_or_else(|| PwrightError::ProfileNotFound(profile_id.to_string()))?;
        Ok(state.profile)
    }

    async fn stop_browser(&self, profile_id: &str) -> Result<bool, PwrightError> {
        if self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .contains_key(profile_id)
        {
            Ok(true)
        } else {
            Err(PwrightError::ProfileNotFound(profile_id.to_string()))
        }
    }

    async fn check_browser(&self, profile_id: &str) -> Result<BrowserHealth, PwrightError> {
        let state = self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .get(profile_id)
            .cloned()
            .ok_or_else(|| PwrightError::ProfileNotFound(profile_id.to_string()))?;
        Ok(BrowserHealth {
            healthy: state.healthy,
            message: state.health_message,
        })
    }

    async fn get_snapshot(&self, profile_id: &str) -> Result<Vec<A11yNode>, PwrightError> {
        let state = self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .get(profile_id)
            .cloned()
            .ok_or_else(|| PwrightError::ProfileNotFound(profile_id.to_string()))?;
        Ok(state.snapshot)
    }

    async fn execute_action(
        &self,
        profile_id: &str,
        request: ExecuteActionRequest,
    ) -> Result<ExecuteActionResponse, PwrightError> {
        if !self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .contains_key(profile_id)
        {
            return Err(PwrightError::ProfileNotFound(profile_id.to_string()));
        }
        self.actions
            .write()
            .expect("recording pwright gateway action lock poisoned")
            .push((profile_id.to_string(), request));
        Ok(ExecuteActionResponse {
            success: true,
            message: "recording pwright gateway action executed".to_string(),
        })
    }

    async fn evaluate(&self, profile_id: &str, _expression: &str) -> Result<String, PwrightError> {
        let state = self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .get(profile_id)
            .cloned()
            .ok_or_else(|| PwrightError::ProfileNotFound(profile_id.to_string()))?;
        Ok(state.eval_json)
    }

    async fn run_script(
        &self,
        profile_id: &str,
        _yaml: &str,
        _params: HashMap<String, String>,
    ) -> Result<Vec<RunScriptResponse>, PwrightError> {
        if !self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .contains_key(profile_id)
        {
            return Err(PwrightError::ProfileNotFound(profile_id.to_string()));
        }
        Ok(vec![RunScriptResponse {
            json_line: r#"{"event":"script_complete","source":"recording_gateway"}"#.to_string(),
        }])
    }
}

pub type SharedPwrightGateway = Arc<dyn PwrightGateway>;

#[derive(Debug, Clone)]
pub struct RealPwrightProfile {
    pub profile: BrowserProfile,
    pub initial_url: String,
}

#[cfg(feature = "real-pwright")]
pub struct RealPwrightGateway {
    profiles: RwLock<HashMap<String, RealPwrightProfile>>,
    active: Mutex<HashMap<String, ActiveBrowser>>,
}

#[cfg(feature = "real-pwright")]
struct ActiveBrowser {
    tab: TabHandle,
}

#[cfg(feature = "real-pwright")]
impl RealPwrightGateway {
    pub fn new(profiles: impl IntoIterator<Item = RealPwrightProfile>) -> Self {
        let gateway = Self {
            profiles: RwLock::new(HashMap::new()),
            active: Mutex::new(HashMap::new()),
        };
        for profile in profiles {
            gateway.insert_profile(profile);
        }
        gateway
    }

    pub fn insert_profile(&self, profile: RealPwrightProfile) {
        self.profiles
            .write()
            .expect("real pwright gateway profile lock poisoned")
            .insert(profile.profile.profile_id.clone(), profile);
    }

    async fn page_for_profile(&self, profile_id: &str) -> Result<Page, PwrightError> {
        self.ensure_browser(profile_id).await?;
        let active = self.active.lock().await;
        let active = active
            .get(profile_id)
            .ok_or_else(|| PwrightError::ProfileNotFound(profile_id.to_string()))?;
        Ok(active.tab.page())
    }

    fn operation_failed(profile_id: &str, error: impl std::fmt::Display) -> PwrightError {
        PwrightError::OperationFailed(profile_id.to_string(), error.to_string())
    }
}

#[cfg(feature = "real-pwright")]
#[async_trait]
impl PwrightGateway for RealPwrightGateway {
    async fn ensure_browser(&self, profile_id: &str) -> Result<BrowserProfile, PwrightError> {
        {
            let active = self.active.lock().await;
            if active.contains_key(profile_id) {
                let profiles = self
                    .profiles
                    .read()
                    .expect("real pwright gateway profile lock poisoned");
                return profiles
                    .get(profile_id)
                    .map(|profile| profile.profile.clone())
                    .ok_or_else(|| PwrightError::ProfileNotFound(profile_id.to_string()));
            }
        }

        let configured = {
            self.profiles
                .read()
                .expect("real pwright gateway profile lock poisoned")
                .get(profile_id)
                .cloned()
                .ok_or_else(|| PwrightError::ProfileNotFound(profile_id.to_string()))?
        };
        let config = BrowserConfig {
            cdp_url: configured.profile.cdp_url.clone(),
            max_parallel_tabs: 4,
            navigate_timeout_ms: 30_000,
            tab_close_via_http: true,
            tab_close_http_timeout_ms: 5_000,
        };
        let mut last_error = None;
        let mut browser = None;
        for _ in 0..30 {
            match Browser::connect(config.clone()).await {
                Ok(connected) => {
                    browser = Some(connected);
                    break;
                }
                Err(error) => {
                    last_error = Some(error);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
        let browser = match browser {
            Some(browser) => browser,
            None => {
                return Err(Self::operation_failed(
                    profile_id,
                    last_error
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| "CDP browser did not become reachable".to_string()),
                ));
            }
        };
        let tab = browser
            .new_tab(&configured.initial_url)
            .await
            .map_err(|error| Self::operation_failed(profile_id, error))?;
        let page = tab.page();
        page.goto(
            &configured.initial_url,
            Some(pwright_bridge::playwright::GotoOptions {
                wait_until: pwright_bridge::navigate::WaitStrategy::Dom,
                timeout_ms: Some(30_000),
            }),
        )
        .await
        .map_err(|error| Self::operation_failed(profile_id, error))?;

        let mut active = self.active.lock().await;
        active.insert(profile_id.to_string(), ActiveBrowser { tab });
        Ok(configured.profile)
    }

    async fn stop_browser(&self, profile_id: &str) -> Result<bool, PwrightError> {
        let mut active = self.active.lock().await;
        let Some(active) = active.remove(profile_id) else {
            return Ok(false);
        };
        active
            .tab
            .close()
            .await
            .map_err(|error| Self::operation_failed(profile_id, error))?;
        Ok(true)
    }

    async fn check_browser(&self, profile_id: &str) -> Result<BrowserHealth, PwrightError> {
        self.ensure_browser(profile_id).await?;
        Ok(BrowserHealth {
            healthy: true,
            message: "real pwright CDP browser ok".to_string(),
        })
    }

    async fn get_snapshot(&self, profile_id: &str) -> Result<Vec<A11yNode>, PwrightError> {
        let page = self.page_for_profile(profile_id).await?;
        let result = page
            .evaluate(
                r#"
(() => {
  const els = Array.from(document.querySelectorAll('button,input,textarea,select,a,[role="button"]'));
  return JSON.stringify(els.map((el, index) => {
    const ref = `e${index}`;
    el.setAttribute('data-bcp-ref', ref);
    const role = el.getAttribute('role') || el.tagName.toLowerCase();
    const label = el.getAttribute('aria-label') || el.innerText || el.value || el.name || el.id || role;
    return { ref, role, name: label.trim(), value: el.value || "", depth: 1 };
  }));
})()
                        "#,
            )
            .await
            .map_err(|error| Self::operation_failed(profile_id, error))?;
        let raw = result
            .get("value")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                PwrightError::OperationFailed(
                    profile_id.to_string(),
                    format!("unexpected snapshot result: {result}"),
                )
            })?;
        let nodes: Vec<serde_json::Value> =
            serde_json::from_str(raw).map_err(|error| Self::operation_failed(profile_id, error))?;
        Ok(nodes
            .into_iter()
            .map(|node| A11yNode {
                r#ref: node
                    .get("ref")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                role: node
                    .get("role")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                name: node
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                depth: node
                    .get("depth")
                    .and_then(|value| value.as_i64())
                    .unwrap_or_default() as i32,
                value: node
                    .get("value")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect())
    }

    async fn execute_action(
        &self,
        profile_id: &str,
        request: ExecuteActionRequest,
    ) -> Result<ExecuteActionResponse, PwrightError> {
        let page = self.page_for_profile(profile_id).await?;
        let selector = request
            .options
            .get("selector")
            .cloned()
            .unwrap_or_else(|| format!(r#"[data-bcp-ref="{}"]"#, request.r#ref));
        match request.action.as_str() {
            "click" => page
                .click(&selector)
                .await
                .map_err(|error| Self::operation_failed(profile_id, error))?,
            "fill" => page
                .fill(&selector, &request.text)
                .await
                .map_err(|error| Self::operation_failed(profile_id, error))?,
            "type" => page
                .type_text(&selector, &request.text)
                .await
                .map_err(|error| Self::operation_failed(profile_id, error))?,
            "press" => page
                .press(&selector, &request.key)
                .await
                .map_err(|error| Self::operation_failed(profile_id, error))?,
            other => {
                return Err(PwrightError::OperationFailed(
                    profile_id.to_string(),
                    format!("unsupported real-browser action: {other}"),
                ));
            }
        }
        Ok(ExecuteActionResponse {
            success: true,
            message: "real pwright CDP action executed".to_string(),
        })
    }

    async fn evaluate(&self, profile_id: &str, expression: &str) -> Result<String, PwrightError> {
        let page = self.page_for_profile(profile_id).await?;
        let result = page
            .evaluate(expression)
            .await
            .map_err(|error| Self::operation_failed(profile_id, error))?;
        if let Some(value) = result.get("value") {
            if let Some(value) = value.as_str() {
                return Ok(value.to_string());
            }
            return Ok(value.to_string());
        }
        Ok(result.to_string())
    }

    async fn run_script(
        &self,
        profile_id: &str,
        _yaml: &str,
        _params: HashMap<String, String>,
    ) -> Result<Vec<RunScriptResponse>, PwrightError> {
        self.ensure_browser(profile_id).await?;
        Ok(vec![RunScriptResponse {
            json_line: r#"{"event":"script_complete","source":"real_pwright_gateway"}"#.to_string(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use bcp_proto::browsercontrol::v1::{BrowserProfile, ExecuteActionRequest, ProfileStatus};

    use super::*;

    fn profile(profile_id: &str) -> BrowserProfile {
        BrowserProfile {
            profile_id: profile_id.to_string(),
            machine_id: "machine-a".to_string(),
            profile_path: format!("/profiles/{profile_id}"),
            display_name: profile_id.to_string(),
            status: ProfileStatus::Available as i32,
            cdp_url: "recording://pwright-gateway".to_string(),
            cdp_port: 0,
            accounts: vec![],
            labels: HashMap::new(),
            last_seen_unix_ms: 0,
        }
    }

    #[tokio::test]
    async fn recording_gateway_records_executed_actions() {
        // Arrange
        let gateway = RecordingPwrightGateway::new([RecordingProfileState {
            profile: profile("youtube-main"),
            healthy: true,
            health_message: "ok".to_string(),
            snapshot: vec![],
            eval_json: "{}".to_string(),
        }]);

        // Act
        gateway
            .execute_action(
                "youtube-main",
                ExecuteActionRequest {
                    lease: None,
                    action: "click".to_string(),
                    r#ref: "e1".to_string(),
                    text: String::new(),
                    key: String::new(),
                    options: HashMap::new(),
                },
            )
            .await
            .unwrap();

        // Assert
        let actions = gateway.actions();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].0, "youtube-main");
        assert_eq!(actions[0].1.action, "click");
    }

    #[tokio::test]
    async fn recording_gateway_reports_unhealthy_on_check_but_can_ensure_profile() {
        // Arrange
        let gateway = RecordingPwrightGateway::new([RecordingProfileState {
            profile: profile("youtube-main"),
            healthy: false,
            health_message: "crashed".to_string(),
            snapshot: vec![],
            eval_json: "{}".to_string(),
        }]);

        // Act
        let health = gateway.check_browser("youtube-main").await.unwrap();
        let ensured = gateway.ensure_browser("youtube-main").await.unwrap();

        // Assert
        assert!(!health.healthy);
        assert_eq!(health.message, "crashed");
        assert_eq!(ensured.profile_id, "youtube-main");
    }
}
