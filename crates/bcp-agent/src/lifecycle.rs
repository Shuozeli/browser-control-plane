use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bcp_core::pwright::{BrowserHealth, PwrightError, PwrightGateway, SharedPwrightGateway};
use bcp_proto::browsercontrol::v1::{
    A11yNode, BrowserProfile, ExecuteActionRequest, ExecuteActionResponse, RunScriptResponse,
};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

use crate::config::LifecycleConfig;

pub struct LifecyclePwrightGateway {
    inner: SharedPwrightGateway,
    lifecycles: HashMap<String, LifecycleConfig>,
    children: Mutex<HashMap<String, Child>>,
}

impl LifecyclePwrightGateway {
    pub fn maybe_wrap(
        inner: SharedPwrightGateway,
        lifecycles: HashMap<String, LifecycleConfig>,
    ) -> SharedPwrightGateway {
        if lifecycles.is_empty() {
            return inner;
        }
        Arc::new(Self {
            inner,
            lifecycles,
            children: Mutex::new(HashMap::new()),
        })
    }

    async fn ensure_process(&self, profile_id: &str) -> Result<(), PwrightError> {
        let Some(config) = self.lifecycles.get(profile_id) else {
            return Ok(());
        };
        if config.launch_command.is_empty() {
            return Ok(());
        }

        let mut children = self.children.lock().await;
        if let Some(child) = children.get_mut(profile_id) {
            match child.try_wait() {
                Ok(None) => return Ok(()),
                Ok(Some(_)) => {
                    children.remove(profile_id);
                }
                Err(error) => {
                    return Err(Self::operation_failed(profile_id, error));
                }
            }
        }

        let mut command = Command::new(&config.launch_command[0]);
        command.args(config.launch_command.iter().skip(1));
        if !config.working_dir.is_empty() {
            command.current_dir(&config.working_dir);
        }
        command.envs(&config.env);
        let child = command
            .spawn()
            .map_err(|error| Self::operation_failed(profile_id, error))?;
        children.insert(profile_id.to_string(), child);
        drop(children);
        self.wait_for_readiness(profile_id, config).await?;
        Ok(())
    }

    async fn wait_for_readiness(
        &self,
        profile_id: &str,
        config: &LifecycleConfig,
    ) -> Result<(), PwrightError> {
        let Some((host, port)) = readiness_host_port(&config.readiness_url) else {
            return Ok(());
        };
        let timeout_ms = config.readiness_timeout_ms.max(1);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let addr = format!("{host}:{port}");
        loop {
            match TcpStream::connect(&addr).await {
                Ok(_) => return Ok(()),
                Err(error) if Instant::now() >= deadline => {
                    return Err(Self::operation_failed(
                        profile_id,
                        format!("CDP endpoint {addr} did not become ready: {error}"),
                    ));
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    async fn stop_process(&self, profile_id: &str) -> Result<(), PwrightError> {
        let mut children = self.children.lock().await;
        let Some(mut child) = children.remove(profile_id) else {
            return Ok(());
        };
        child
            .kill()
            .await
            .map_err(|error| Self::operation_failed(profile_id, error))?;
        let _ = child.wait().await;
        Ok(())
    }

    fn operation_failed(profile_id: &str, error: impl std::fmt::Display) -> PwrightError {
        PwrightError::OperationFailed(profile_id.to_string(), error.to_string())
    }
}

fn readiness_host_port(url: &str) -> Option<(String, u16)> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = host_port.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    let host = host.trim_matches(['[', ']']);
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

#[async_trait]
impl PwrightGateway for LifecyclePwrightGateway {
    async fn ensure_browser(&self, profile_id: &str) -> Result<BrowserProfile, PwrightError> {
        self.ensure_process(profile_id).await?;
        self.inner.ensure_browser(profile_id).await
    }

    async fn stop_browser(&self, profile_id: &str) -> Result<bool, PwrightError> {
        let stopped = self.inner.stop_browser(profile_id).await?;
        self.stop_process(profile_id).await?;
        Ok(stopped)
    }

    async fn check_browser(&self, profile_id: &str) -> Result<BrowserHealth, PwrightError> {
        self.ensure_process(profile_id).await?;
        self.inner.check_browser(profile_id).await
    }

    async fn get_snapshot(&self, profile_id: &str) -> Result<Vec<A11yNode>, PwrightError> {
        self.inner.get_snapshot(profile_id).await
    }

    async fn execute_action(
        &self,
        profile_id: &str,
        request: ExecuteActionRequest,
    ) -> Result<ExecuteActionResponse, PwrightError> {
        self.inner.execute_action(profile_id, request).await
    }

    async fn evaluate(&self, profile_id: &str, expression: &str) -> Result<String, PwrightError> {
        self.inner.evaluate(profile_id, expression).await
    }

    async fn run_script(
        &self,
        profile_id: &str,
        yaml: &str,
        params: HashMap<String, String>,
    ) -> Result<Vec<RunScriptResponse>, PwrightError> {
        self.inner.run_script(profile_id, yaml, params).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bcp_core::pwright::{RecordingProfileState, RecordingPwrightGateway};
    use bcp_proto::browsercontrol::v1::{BrowserProfile, ProfileStatus};

    use super::*;

    fn profile() -> BrowserProfile {
        BrowserProfile {
            profile_id: "profile-a".to_string(),
            machine_id: "machine-a".to_string(),
            profile_path: "/profiles/profile-a".to_string(),
            display_name: "profile-a".to_string(),
            status: ProfileStatus::Available as i32,
            cdp_url: "recording://pwright-gateway".to_string(),
            cdp_port: 0,
            accounts: vec![],
            labels: HashMap::new(),
            last_seen_unix_ms: 0,
        }
    }

    #[tokio::test]
    async fn check_browser_starts_configured_process() {
        // Arrange
        let inner = Arc::new(RecordingPwrightGateway::new([RecordingProfileState {
            profile: profile(),
            healthy: true,
            health_message: "ok".to_string(),
            snapshot: vec![],
            eval_json: "{}".to_string(),
        }]));
        let gateway = LifecyclePwrightGateway::maybe_wrap(
            inner,
            HashMap::from([(
                "profile-a".to_string(),
                LifecycleConfig {
                    launch_command: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "sleep 30".to_string(),
                    ],
                    working_dir: String::new(),
                    env: HashMap::new(),
                    readiness_url: String::new(),
                    readiness_timeout_ms: 30_000,
                },
            )]),
        );

        // Act
        let health = gateway.check_browser("profile-a").await.unwrap();

        // Assert
        assert!(health.healthy);
        assert!(gateway.stop_browser("profile-a").await.unwrap());
    }

    #[test]
    fn parses_http_readiness_host_port() {
        // Arrange / Act
        let parsed = readiness_host_port("http://127.0.0.1:9222/json/version");

        // Assert
        assert_eq!(parsed, Some(("127.0.0.1".to_string(), 9222)));
    }
}
