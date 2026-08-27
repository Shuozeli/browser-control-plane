use super::*;

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

#[cfg(test)]
mod tests {
    use bcp_proto::browsercontrol::v1::{BrowserProfile, ExecuteActionRequest, ProfileStatus};

    use super::super::*;

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
