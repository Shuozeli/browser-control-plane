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
use pwright_bridge::playwright::{Page, ScreenshotFormat, ScreenshotOptions, WaitState};
#[cfg(feature = "real-pwright")]
use std::{future::Future, pin::Pin};
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
    /// Capture a screenshot, returning base64-encoded image bytes.
    async fn capture_screenshot(
        &self,
        profile_id: &str,
        format: &str,
        full_page: bool,
    ) -> Result<String, PwrightError>;
    /// Print the page to PDF, returning base64-encoded PDF bytes.
    async fn print_pdf(&self, profile_id: &str) -> Result<String, PwrightError>;
    /// Return all cookies visible to the page as a JSON array string. Includes
    /// httpOnly cookies, which page JavaScript cannot read.
    async fn get_cookies(&self, profile_id: &str) -> Result<String, PwrightError>;
    /// Set cookies from a JSON array (subset fields allowed; the rest default).
    /// Returns the number of cookies applied.
    async fn set_cookies(&self, profile_id: &str, cookies_json: &str) -> Result<u32, PwrightError>;
    /// Return the current page's URL, title, and full HTML content.
    async fn get_page(&self, profile_id: &str) -> Result<PageInfo, PwrightError>;
    /// Attach one or more machine-local file paths to a file `<input>` selected
    /// by CSS selector (bridges an out-of-band uploaded artifact into the page).
    async fn set_input_files(
        &self,
        profile_id: &str,
        selector: &str,
        files: &[String],
    ) -> Result<(), PwrightError>;
}

/// Page-level introspection returned by [`PwrightGateway::get_page`].
#[derive(Debug, Clone, Default)]
pub struct PageInfo {
    pub url: String,
    pub title: String,
    pub content: String,
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
        let mut profiles = self
            .profiles
            .write()
            .expect("recording pwright gateway profile lock poisoned");
        let state = profiles
            .get_mut(profile_id)
            .ok_or_else(|| PwrightError::ProfileNotFound(profile_id.to_string()))?;
        state.healthy = true;
        state.health_message = "recording pwright gateway ok".to_string();
        Ok(state.profile.clone())
    }

    async fn stop_browser(&self, profile_id: &str) -> Result<bool, PwrightError> {
        let mut profiles = self
            .profiles
            .write()
            .expect("recording pwright gateway profile lock poisoned");
        let state = profiles
            .get_mut(profile_id)
            .ok_or_else(|| PwrightError::ProfileNotFound(profile_id.to_string()))?;
        state.healthy = false;
        state.health_message = "recording pwright gateway stopped".to_string();
        Ok(true)
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

    async fn capture_screenshot(
        &self,
        profile_id: &str,
        _format: &str,
        _full_page: bool,
    ) -> Result<String, PwrightError> {
        if !self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .contains_key(profile_id)
        {
            return Err(PwrightError::ProfileNotFound(profile_id.to_string()));
        }
        // Base64 of "recording" — a deterministic, decodable stub.
        Ok("cmVjb3JkaW5n".to_string())
    }

    async fn print_pdf(&self, profile_id: &str) -> Result<String, PwrightError> {
        if !self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .contains_key(profile_id)
        {
            return Err(PwrightError::ProfileNotFound(profile_id.to_string()));
        }
        Ok("cmVjb3JkaW5n".to_string())
    }

    async fn get_cookies(&self, profile_id: &str) -> Result<String, PwrightError> {
        if !self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .contains_key(profile_id)
        {
            return Err(PwrightError::ProfileNotFound(profile_id.to_string()));
        }
        Ok("[]".to_string())
    }

    async fn set_cookies(&self, profile_id: &str, cookies_json: &str) -> Result<u32, PwrightError> {
        if !self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .contains_key(profile_id)
        {
            return Err(PwrightError::ProfileNotFound(profile_id.to_string()));
        }
        let parsed: serde_json::Value = serde_json::from_str(cookies_json).map_err(|error| {
            PwrightError::OperationFailed(profile_id.to_string(), error.to_string())
        })?;
        Ok(parsed
            .as_array()
            .map(|array| array.len() as u32)
            .unwrap_or(0))
    }

    async fn get_page(&self, profile_id: &str) -> Result<PageInfo, PwrightError> {
        if !self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .contains_key(profile_id)
        {
            return Err(PwrightError::ProfileNotFound(profile_id.to_string()));
        }
        Ok(PageInfo {
            url: "recording://page".to_string(),
            title: "recording".to_string(),
            content: "<html></html>".to_string(),
        })
    }

    async fn set_input_files(
        &self,
        profile_id: &str,
        _selector: &str,
        _files: &[String],
    ) -> Result<(), PwrightError> {
        if !self
            .profiles
            .read()
            .expect("recording pwright gateway profile lock poisoned")
            .contains_key(profile_id)
        {
            return Err(PwrightError::ProfileNotFound(profile_id.to_string()));
        }
        Ok(())
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

    /// Drop a profile's cached browser so the next `ensure_browser` reconnects.
    async fn evict(&self, profile_id: &str) {
        self.active.lock().await.remove(profile_id);
    }

    /// True when an error looks like a dropped CDP transport (a stale cached
    /// websocket), so the operation is worth retrying against a fresh connection.
    fn is_connection_closed(error: &PwrightError) -> bool {
        match error {
            PwrightError::OperationFailed(_, message) => {
                let message = message.to_ascii_lowercase();
                message.contains("connection closed")
                    || message.contains("reset")
                    || message.contains("closing handshake")
                    || message.contains("broken pipe")
                    || message.contains("not connected")
            }
            _ => false,
        }
    }

    /// Run a page operation, and if it fails because the cached CDP connection
    /// is dead, evict it, reconnect once, and retry. This makes a transient
    /// disconnect self-heal instead of wedging the profile forever (the cached
    /// handle was previously reused blindly by `ensure_browser`).
    async fn with_reconnect<'a, T, F>(
        &'a self,
        profile_id: &'a str,
        op: F,
    ) -> Result<T, PwrightError>
    where
        F: Fn(Page) -> Pin<Box<dyn Future<Output = Result<T, PwrightError>> + Send + 'a>>,
    {
        let page = self.page_for_profile(profile_id).await?;
        match op(page).await {
            Err(error) if Self::is_connection_closed(&error) => {
                self.evict(profile_id).await;
                let page = self.page_for_profile(profile_id).await?;
                op(page).await
            }
            other => other,
        }
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
        self.with_reconnect(profile_id, |page| {
            Box::pin(async move {
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
                let nodes: Vec<serde_json::Value> = serde_json::from_str(raw)
                    .map_err(|error| Self::operation_failed(profile_id, error))?;
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
            })
        })
        .await
    }

    async fn execute_action(
        &self,
        profile_id: &str,
        request: ExecuteActionRequest,
    ) -> Result<ExecuteActionResponse, PwrightError> {
        let selector = request
            .options
            .get("selector")
            .cloned()
            .unwrap_or_else(|| format!(r#"[data-bcp-ref="{}"]"#, request.r#ref));
        let request = &request;
        let selector = selector.as_str();
        self.with_reconnect(profile_id, move |page| {
            Box::pin(async move {
                match request.action.as_str() {
                    "click" => page
                        .click(selector)
                        .await
                        .map_err(|error| Self::operation_failed(profile_id, error))?,
                    "fill" => page
                        .fill(selector, &request.text)
                        .await
                        .map_err(|error| Self::operation_failed(profile_id, error))?,
                    "type" => page
                        .type_text(selector, &request.text)
                        .await
                        .map_err(|error| Self::operation_failed(profile_id, error))?,
                    "press" => page
                        .press(selector, &request.key)
                        .await
                        .map_err(|error| Self::operation_failed(profile_id, error))?,
                    "hover" => page
                        .locator(selector)
                        .hover()
                        .await
                        .map_err(|error| Self::operation_failed(profile_id, error))?,
                    "dblclick" => page
                        .locator(selector)
                        .dblclick()
                        .await
                        .map_err(|error| Self::operation_failed(profile_id, error))?,
                    "select" => page
                        .locator(selector)
                        .select_option(&request.text)
                        .await
                        .map_err(|error| Self::operation_failed(profile_id, error))?,
                    "wait_for_selector" => {
                        let timeout_ms = request
                            .options
                            .get("timeout_ms")
                            .and_then(|value| value.parse::<u64>().ok())
                            .unwrap_or(5_000)
                            .min(MAX_STEP_WAIT_MS);
                        page.locator(selector)
                            .wait_for(timeout_ms, WaitState::Visible)
                            .await
                            .map_err(|error| Self::operation_failed(profile_id, error))?;
                    }
                    "scroll" => {
                        let dx = request
                            .options
                            .get("dx")
                            .and_then(|value| value.parse::<i64>().ok())
                            .unwrap_or(0);
                        let dy = request
                            .options
                            .get("dy")
                            .and_then(|value| value.parse::<i64>().ok())
                            .unwrap_or(0);
                        page.evaluate(&format!("window.scrollBy({dx},{dy})"))
                            .await
                            .map_err(|error| Self::operation_failed(profile_id, error))?;
                    }
                    "reload" => page
                        .reload()
                        .await
                        .map_err(|error| Self::operation_failed(profile_id, error))?,
                    "back" => page
                        .go_back()
                        .await
                        .map_err(|error| Self::operation_failed(profile_id, error))?,
                    "forward" => page
                        .go_forward()
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
            })
        })
        .await
    }

    async fn evaluate(&self, profile_id: &str, expression: &str) -> Result<String, PwrightError> {
        self.with_reconnect(profile_id, |page| {
            Box::pin(async move {
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
            })
        })
        .await
    }

    async fn run_script(
        &self,
        profile_id: &str,
        yaml: &str,
        params: HashMap<String, String>,
    ) -> Result<Vec<RunScriptResponse>, PwrightError> {
        // Parse first, then substitute params into the parsed string fields, so a
        // param value cannot inject or reshape the YAML program.
        let mut script: ScriptSpec = serde_yaml::from_str(yaml)
            .map_err(|error| Self::operation_failed(profile_id, error))?;
        for step in &mut script.steps {
            step.apply_params(&params);
        }

        // Run the program; if a step fails because the cached CDP connection is
        // dead, evict it and restart the program once against a fresh connection.
        let mut reconnected = false;
        let lines = loop {
            let page = self.page_for_profile(profile_id).await?;
            let mut lines = Vec::new();
            let mut restart = false;
            for (index, step) in script.steps.iter().enumerate() {
                match run_script_step(&page, step).await {
                    Ok(result) => lines.push(RunScriptResponse {
                        json_line: serde_json::json!({
                            "step": index,
                            "action": step.action,
                            "ok": true,
                            "result": result,
                        })
                        .to_string(),
                    }),
                    Err(error) => {
                        if !reconnected && Self::is_connection_closed(&error) {
                            // Stale cached connection: reconnect and rerun once.
                            restart = true;
                            break;
                        }
                        lines.push(RunScriptResponse {
                            json_line: serde_json::json!({
                                "step": index,
                                "action": step.action,
                                "ok": false,
                                "error": error.to_string(),
                            })
                            .to_string(),
                        });
                        // Stop at the first failing step so callers see where it broke.
                        return Ok(lines);
                    }
                }
            }
            if restart {
                reconnected = true;
                self.evict(profile_id).await;
                continue;
            }
            break lines;
        };
        let mut lines = lines;
        lines.push(RunScriptResponse {
            json_line: serde_json::json!({
                "event": "script_complete",
                "steps": script.steps.len(),
            })
            .to_string(),
        });
        Ok(lines)
    }

    async fn capture_screenshot(
        &self,
        profile_id: &str,
        format: &str,
        full_page: bool,
    ) -> Result<String, PwrightError> {
        self.with_reconnect(profile_id, move |page| {
            Box::pin(async move {
                page.screenshot(Some(ScreenshotOptions {
                    format: screenshot_format(format),
                    full_page,
                }))
                .await
                .map_err(|error| Self::operation_failed(profile_id, error))
            })
        })
        .await
    }

    async fn print_pdf(&self, profile_id: &str) -> Result<String, PwrightError> {
        self.with_reconnect(profile_id, |page| {
            Box::pin(async move {
                page.pdf()
                    .await
                    .map_err(|error| Self::operation_failed(profile_id, error))
            })
        })
        .await
    }

    async fn get_cookies(&self, profile_id: &str) -> Result<String, PwrightError> {
        // The pinned bridge exposes no raw CDP session, so this reads
        // document.cookie (name/value pairs only). httpOnly cookies are not
        // visible here — retrieve those over the raw CDP proxy (Network.getCookies).
        self.with_reconnect(profile_id, |page| {
            Box::pin(async move {
                let result = page
                    .evaluate(
                        r#"JSON.stringify(document.cookie.split('; ').filter(Boolean).map((pair) => {
  const eq = pair.indexOf('=');
  return { name: pair.slice(0, eq), value: pair.slice(eq + 1) };
}))"#,
                    )
                    .await
                    .map_err(|error| Self::operation_failed(profile_id, error))?;
                Ok(result
                    .get("value")
                    .and_then(|value| value.as_str())
                    .unwrap_or("[]")
                    .to_string())
            })
        })
        .await
    }

    async fn set_cookies(&self, profile_id: &str, cookies_json: &str) -> Result<u32, PwrightError> {
        let array: Vec<serde_json::Value> = serde_json::from_str(cookies_json)
            .map_err(|error| Self::operation_failed(profile_id, error))?;
        let array = &array;
        self.with_reconnect(profile_id, move |page| {
            Box::pin(async move {
                let mut count = 0_u32;
                for cookie in array {
                    let name = cookie.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let value = cookie.get("value").and_then(|v| v.as_str()).unwrap_or("");
                    if name.is_empty() {
                        continue;
                    }
                    let path = cookie.get("path").and_then(|v| v.as_str()).unwrap_or("/");
                    let assignment = format!("{name}={value}; path={path}");
                    // JSON-encode the assignment into a JS string literal so cookie
                    // contents cannot break out of the expression.
                    let literal = serde_json::to_string(&assignment)
                        .map_err(|error| Self::operation_failed(profile_id, error))?;
                    page.evaluate(&format!("document.cookie = {literal}"))
                        .await
                        .map_err(|error| Self::operation_failed(profile_id, error))?;
                    count += 1;
                }
                Ok(count)
            })
        })
        .await
    }

    async fn get_page(&self, profile_id: &str) -> Result<PageInfo, PwrightError> {
        self.with_reconnect(profile_id, |page| {
            Box::pin(async move {
                let url = page
                    .url()
                    .await
                    .map_err(|error| Self::operation_failed(profile_id, error))?;
                let title = page
                    .title()
                    .await
                    .map_err(|error| Self::operation_failed(profile_id, error))?;
                let content = page
                    .content()
                    .await
                    .map_err(|error| Self::operation_failed(profile_id, error))?;
                Ok(PageInfo {
                    url,
                    title,
                    content,
                })
            })
        })
        .await
    }

    async fn set_input_files(
        &self,
        profile_id: &str,
        selector: &str,
        files: &[String],
    ) -> Result<(), PwrightError> {
        self.with_reconnect(profile_id, move |page| {
            Box::pin(async move {
                page.set_input_files(selector, files)
                    .await
                    .map_err(|error| Self::operation_failed(profile_id, error))
            })
        })
        .await
    }
}

/// Maps a caller-supplied format string onto a `ScreenshotFormat`, defaulting to
/// PNG. Lossy formats use a fixed quality since callers pass only a name.
#[cfg(feature = "real-pwright")]
fn screenshot_format(format: &str) -> ScreenshotFormat {
    match format.to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => ScreenshotFormat::Jpeg(80),
        "webp" => ScreenshotFormat::Webp(80),
        _ => ScreenshotFormat::Png,
    }
}

/// A `RunScript` program: an ordered list of steps executed against the page.
#[cfg(feature = "real-pwright")]
#[derive(Debug, serde::Deserialize)]
struct ScriptSpec {
    #[serde(default)]
    steps: Vec<ScriptStep>,
}

#[cfg(feature = "real-pwright")]
#[derive(Debug, serde::Deserialize)]
struct ScriptStep {
    action: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    selector: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    expression: String,
    #[serde(default)]
    ms: u64,
    #[serde(default)]
    dx: i64,
    #[serde(default)]
    dy: i64,
    #[serde(default)]
    full_page: bool,
}

/// Upper bound on a single `wait_ms` step so a script cannot pin the streaming
/// handler (and its browser page) for an unbounded time.
#[cfg(feature = "real-pwright")]
const MAX_STEP_WAIT_MS: u64 = 60_000;

#[cfg(feature = "real-pwright")]
impl ScriptStep {
    /// Substitutes `${key}` occurrences in the string fields with param values,
    /// after the YAML has been parsed (so params cannot alter the program shape).
    fn apply_params(&mut self, params: &HashMap<String, String>) {
        let substitute = |value: &mut String| {
            for (key, replacement) in params {
                *value = value.replace(&format!("${{{key}}}"), replacement);
            }
        };
        substitute(&mut self.url);
        substitute(&mut self.selector);
        substitute(&mut self.text);
        substitute(&mut self.key);
        substitute(&mut self.expression);
    }
}

#[cfg(feature = "real-pwright")]
async fn run_script_step(
    page: &Page,
    step: &ScriptStep,
) -> Result<serde_json::Value, PwrightError> {
    let fail = |error: Box<dyn std::fmt::Display>| {
        PwrightError::OperationFailed("script".to_string(), error.to_string())
    };
    match step.action.as_str() {
        "goto" => {
            page.goto(
                &step.url,
                Some(pwright_bridge::playwright::GotoOptions {
                    wait_until: pwright_bridge::navigate::WaitStrategy::Dom,
                    timeout_ms: Some(30_000),
                }),
            )
            .await
            .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!(step.url))
        }
        "click" => {
            page.click(&step.selector)
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!("clicked"))
        }
        "fill" => {
            page.fill(&step.selector, &step.text)
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!("filled"))
        }
        "type" => {
            page.type_text(&step.selector, &step.text)
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!("typed"))
        }
        "press" => {
            page.press(&step.selector, &step.key)
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!("pressed"))
        }
        "wait_ms" => {
            let ms = step.ms.min(MAX_STEP_WAIT_MS);
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            Ok(serde_json::json!(ms))
        }
        "hover" => {
            page.locator(&step.selector)
                .hover()
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!("hovered"))
        }
        "dblclick" => {
            page.locator(&step.selector)
                .dblclick()
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!("dblclicked"))
        }
        "select" => {
            page.locator(&step.selector)
                .select_option(&step.text)
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!(step.text))
        }
        "wait_for_selector" => {
            let timeout_ms = if step.ms == 0 {
                5_000
            } else {
                step.ms.min(MAX_STEP_WAIT_MS)
            };
            page.locator(&step.selector)
                .wait_for(timeout_ms, WaitState::Visible)
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!(step.selector))
        }
        "scroll" => {
            page.evaluate(&format!("window.scrollBy({},{})", step.dx, step.dy))
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!({ "dx": step.dx, "dy": step.dy }))
        }
        "reload" => {
            page.reload().await.map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!("reloaded"))
        }
        "back" => {
            page.go_back()
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!("back"))
        }
        "forward" => {
            page.go_forward()
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!("forward"))
        }
        "screenshot" => {
            let base64 = page
                .screenshot(Some(ScreenshotOptions {
                    format: screenshot_format(&step.text),
                    full_page: step.full_page,
                }))
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!({ "base64": base64 }))
        }
        "pdf" => {
            let base64 = page.pdf().await.map_err(|error| fail(Box::new(error)))?;
            Ok(serde_json::json!({ "base64": base64 }))
        }
        "eval" => {
            let value = page
                .evaluate(&step.expression)
                .await
                .map_err(|error| fail(Box::new(error)))?;
            Ok(value.get("value").cloned().unwrap_or(value))
        }
        other => Err(PwrightError::OperationFailed(
            "script".to_string(),
            format!("unknown step action: {other}"),
        )),
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

    #[tokio::test]
    async fn recording_gateway_ensure_recovers_health_after_stop() {
        // Arrange
        let gateway = RecordingPwrightGateway::new([RecordingProfileState {
            profile: profile("youtube-main"),
            healthy: true,
            health_message: "ok".to_string(),
            snapshot: vec![],
            eval_json: "{}".to_string(),
        }]);

        // Act
        gateway.stop_browser("youtube-main").await.unwrap();
        let stopped_health = gateway.check_browser("youtube-main").await.unwrap();
        gateway.ensure_browser("youtube-main").await.unwrap();
        let recovered_health = gateway.check_browser("youtube-main").await.unwrap();

        // Assert
        assert!(!stopped_health.healthy);
        assert_eq!(stopped_health.message, "recording pwright gateway stopped");
        assert!(recovered_health.healthy);
        assert_eq!(recovered_health.message, "recording pwright gateway ok");
    }
}

#[cfg(all(test, feature = "real-pwright"))]
mod real_pwright_tests {
    use super::*;

    fn op_failed(message: &str) -> PwrightError {
        PwrightError::OperationFailed("profile".to_string(), message.to_string())
    }

    #[test]
    fn is_connection_closed_matches_dropped_transport_errors() {
        // Arrange / Act / Assert: the errors that should trigger evict+reconnect.
        assert!(RealPwrightGateway::is_connection_closed(&op_failed(
            "Connection closed"
        )));
        assert!(RealPwrightGateway::is_connection_closed(&op_failed(
            "IO error: Connection reset by peer (os error 104)"
        )));
        assert!(RealPwrightGateway::is_connection_closed(&op_failed(
            "WebSocket protocol error: Connection reset without closing handshake"
        )));
        assert!(RealPwrightGateway::is_connection_closed(&op_failed(
            "Broken pipe"
        )));
    }

    #[test]
    fn is_connection_closed_ignores_ordinary_operation_errors() {
        // Arrange / Act / Assert: real failures must not be retried as reconnects.
        assert!(!RealPwrightGateway::is_connection_closed(&op_failed(
            "no element matched selector '#missing'"
        )));
        assert!(!RealPwrightGateway::is_connection_closed(&op_failed(
            "unsupported real-browser action: teleport"
        )));
        assert!(!RealPwrightGateway::is_connection_closed(
            &PwrightError::ProfileNotFound("profile".to_string())
        ));
    }
}
