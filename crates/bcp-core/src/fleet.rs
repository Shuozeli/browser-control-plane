use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use bcp_proto::browsercontrol::v1::{
    BrowserProfile, ControlPlaneEvent, EventSeverity, MetricKind, MetricSample, ProfileStatus,
};

use crate::pwright::{PwrightError, SharedPwrightGateway};
use crate::time::Clock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserRuntimeStatus {
    Desired,
    Running,
    Unhealthy,
    Missing,
}

#[derive(Debug, Clone)]
pub struct ManagedBrowser {
    pub profile: BrowserProfile,
    pub desired: bool,
    pub status: BrowserRuntimeStatus,
    pub last_heartbeat_unix_ms: i64,
    pub restart_count: u64,
    pub last_error: String,
}

#[derive(Debug, Clone, Default)]
pub struct FleetTelemetry {
    pub samples: Vec<MetricSample>,
    pub events: Vec<ControlPlaneEvent>,
}

pub struct BrowserFleetManager {
    machine_id: String,
    pwright: SharedPwrightGateway,
    clock: Arc<dyn Clock>,
    browsers: RwLock<HashMap<String, ManagedBrowser>>,
    pending: RwLock<FleetTelemetry>,
}

impl BrowserFleetManager {
    pub fn new(
        machine_id: impl Into<String>,
        pwright: SharedPwrightGateway,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            machine_id: machine_id.into(),
            pwright,
            clock,
            browsers: RwLock::new(HashMap::new()),
            pending: RwLock::new(FleetTelemetry::default()),
        }
    }

    pub fn upsert_desired_profile(&self, mut profile: BrowserProfile) {
        if profile.status == ProfileStatus::Unspecified as i32 {
            profile.status = ProfileStatus::Available as i32;
        }
        self.browsers
            .write()
            .expect("browser fleet lock poisoned")
            .entry(profile.profile_id.clone())
            .and_modify(|browser| {
                browser.profile = profile.clone();
                browser.desired = true;
                if browser.status == BrowserRuntimeStatus::Missing {
                    browser.status = BrowserRuntimeStatus::Desired;
                }
            })
            .or_insert_with(|| ManagedBrowser {
                profile,
                desired: true,
                status: BrowserRuntimeStatus::Desired,
                last_heartbeat_unix_ms: 0,
                restart_count: 0,
                last_error: String::new(),
            });
    }

    pub fn list_profiles(&self) -> Vec<BrowserProfile> {
        self.browsers
            .read()
            .expect("browser fleet lock poisoned")
            .values()
            .map(|browser| browser.profile.clone())
            .collect()
    }

    pub fn browser(&self, profile_id: &str) -> Option<ManagedBrowser> {
        self.browsers
            .read()
            .expect("browser fleet lock poisoned")
            .get(profile_id)
            .cloned()
    }

    pub async fn reconcile_once(&self) {
        let desired: Vec<String> = self
            .browsers
            .read()
            .expect("browser fleet lock poisoned")
            .values()
            .filter(|browser| browser.desired)
            .map(|browser| browser.profile.profile_id.clone())
            .collect();

        for profile_id in desired {
            self.reconcile_browser(&profile_id).await;
        }
    }

    pub fn drain_telemetry(&self) -> FleetTelemetry {
        std::mem::take(
            &mut *self
                .pending
                .write()
                .expect("browser fleet telemetry lock poisoned"),
        )
    }

    async fn reconcile_browser(&self, profile_id: &str) {
        let now = self.clock.now_unix_ms();
        let health = self.pwright.check_browser(profile_id).await;
        match health {
            Ok(health) if health.healthy => {
                self.update_running(profile_id, now, "");
                self.record_running_sample(profile_id, now, 1.0);
            }
            Ok(health) => {
                self.update_unhealthy(profile_id, now, &health.message);
                self.ensure_browser(profile_id, now, "unhealthy").await;
            }
            Err(PwrightError::ProfileNotFound(_)) => {
                self.update_missing(profile_id, now, "profile not found");
                self.ensure_browser(profile_id, now, "missing").await;
            }
            Err(error) => {
                self.update_unhealthy(profile_id, now, &error.to_string());
                self.ensure_browser(profile_id, now, "error").await;
            }
        }
    }

    async fn ensure_browser(&self, profile_id: &str, now: i64, reason: &str) {
        match self.pwright.ensure_browser(profile_id).await {
            Ok(profile) => {
                let mut browsers = self.browsers.write().expect("browser fleet lock poisoned");
                if let Some(browser) = browsers.get_mut(profile_id) {
                    browser.profile = profile;
                    browser.status = BrowserRuntimeStatus::Running;
                    browser.last_heartbeat_unix_ms = now;
                    browser.restart_count += 1;
                    browser.last_error.clear();
                }
                drop(browsers);
                self.record_restart(profile_id, now, reason);
                self.record_running_sample(profile_id, now, 1.0);
            }
            Err(error) => {
                let mut browsers = self.browsers.write().expect("browser fleet lock poisoned");
                if let Some(browser) = browsers.get_mut(profile_id) {
                    browser.status = BrowserRuntimeStatus::Unhealthy;
                    browser.last_heartbeat_unix_ms = now;
                    browser.last_error = error.to_string();
                }
                drop(browsers);
                self.record_event(
                    "bcp.browser.ensure_failed",
                    EventSeverity::Error,
                    profile_id,
                    now,
                    &error.to_string(),
                    HashMap::from([("reason".to_string(), reason.to_string())]),
                );
                self.record_running_sample(profile_id, now, 0.0);
            }
        }
    }

    fn update_running(&self, profile_id: &str, now: i64, error: &str) {
        let mut browsers = self.browsers.write().expect("browser fleet lock poisoned");
        if let Some(browser) = browsers.get_mut(profile_id) {
            browser.status = BrowserRuntimeStatus::Running;
            browser.last_heartbeat_unix_ms = now;
            browser.last_error = error.to_string();
        }
    }

    fn update_unhealthy(&self, profile_id: &str, now: i64, error: &str) {
        let mut browsers = self.browsers.write().expect("browser fleet lock poisoned");
        if let Some(browser) = browsers.get_mut(profile_id) {
            browser.status = BrowserRuntimeStatus::Unhealthy;
            browser.last_heartbeat_unix_ms = now;
            browser.last_error = error.to_string();
        }
        drop(browsers);
        self.record_event(
            "bcp.browser.unhealthy",
            EventSeverity::Warn,
            profile_id,
            now,
            error,
            HashMap::new(),
        );
    }

    fn update_missing(&self, profile_id: &str, now: i64, error: &str) {
        let mut browsers = self.browsers.write().expect("browser fleet lock poisoned");
        if let Some(browser) = browsers.get_mut(profile_id) {
            browser.status = BrowserRuntimeStatus::Missing;
            browser.last_heartbeat_unix_ms = now;
            browser.last_error = error.to_string();
        }
        drop(browsers);
        self.record_event(
            "bcp.browser.missing",
            EventSeverity::Warn,
            profile_id,
            now,
            error,
            HashMap::new(),
        );
    }

    fn record_restart(&self, profile_id: &str, now: i64, reason: &str) {
        self.push_sample(MetricSample {
            name: "bcp.browser.restart.count".to_string(),
            kind: MetricKind::Counter as i32,
            observed_at_unix_ms: now,
            machine_id: self.machine_id.clone(),
            profile_id: profile_id.to_string(),
            platform: 0,
            domain: String::new(),
            action: "ensure_browser".to_string(),
            status_class: String::new(),
            error_class: reason.to_string(),
            value: 1.0,
        });
        self.record_event(
            "bcp.browser.ensure_started",
            EventSeverity::Info,
            profile_id,
            now,
            "browser ensure succeeded",
            HashMap::from([("reason".to_string(), reason.to_string())]),
        );
    }

    fn record_running_sample(&self, profile_id: &str, now: i64, value: f64) {
        self.push_sample(MetricSample {
            name: "bcp.browser.running".to_string(),
            kind: MetricKind::Gauge as i32,
            observed_at_unix_ms: now,
            machine_id: self.machine_id.clone(),
            profile_id: profile_id.to_string(),
            platform: 0,
            domain: String::new(),
            action: String::new(),
            status_class: String::new(),
            error_class: String::new(),
            value,
        });
    }

    fn record_event(
        &self,
        event_type: &str,
        severity: EventSeverity,
        profile_id: &str,
        now: i64,
        message: &str,
        attributes: HashMap<String, String>,
    ) {
        self.pending
            .write()
            .expect("browser fleet telemetry lock poisoned")
            .events
            .push(ControlPlaneEvent {
                event_id: format!("{}:{}:{}", self.machine_id, profile_id, now),
                event_type: event_type.to_string(),
                severity: severity as i32,
                observed_at_unix_ms: now,
                machine_id: self.machine_id.clone(),
                profile_id: profile_id.to_string(),
                platform: 0,
                message: message.to_string(),
                attributes,
            });
    }

    fn push_sample(&self, sample: MetricSample) {
        self.pending
            .write()
            .expect("browser fleet telemetry lock poisoned")
            .samples
            .push(sample);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bcp_proto::browsercontrol::v1::BrowserProfile;

    use crate::pwright::{RecordingProfileState, RecordingPwrightGateway};
    use crate::time::FakeClock;

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
    async fn reconcile_marks_healthy_browser_running() {
        // Arrange
        let clock = Arc::new(FakeClock::new(10_000));
        let gateway = Arc::new(RecordingPwrightGateway::new([RecordingProfileState {
            profile: profile("youtube-main"),
            healthy: true,
            health_message: "ok".to_string(),
            snapshot: vec![],
            eval_json: "{}".to_string(),
        }]));
        let manager = BrowserFleetManager::new("machine-a", gateway, clock);
        manager.upsert_desired_profile(profile("youtube-main"));

        // Act
        manager.reconcile_once().await;

        // Assert
        let browser = manager.browser("youtube-main").unwrap();
        assert_eq!(browser.status, BrowserRuntimeStatus::Running);
        assert_eq!(browser.last_heartbeat_unix_ms, 10_000);
        let telemetry = manager.drain_telemetry();
        assert_eq!(telemetry.samples[0].name, "bcp.browser.running");
        assert_eq!(telemetry.samples[0].value, 1.0);
    }

    #[tokio::test]
    async fn reconcile_brings_up_unhealthy_browser_and_records_restart() {
        // Arrange
        let clock = Arc::new(FakeClock::new(20_000));
        let gateway = Arc::new(RecordingPwrightGateway::new([RecordingProfileState {
            profile: profile("youtube-main"),
            healthy: false,
            health_message: "crashed".to_string(),
            snapshot: vec![],
            eval_json: "{}".to_string(),
        }]));
        let manager = BrowserFleetManager::new("machine-a", gateway, clock);
        manager.upsert_desired_profile(profile("youtube-main"));

        // Act
        manager.reconcile_once().await;

        // Assert
        let browser = manager.browser("youtube-main").unwrap();
        assert_eq!(browser.status, BrowserRuntimeStatus::Running);
        assert_eq!(browser.restart_count, 1);
        let telemetry = manager.drain_telemetry();
        assert!(
            telemetry
                .events
                .iter()
                .any(|event| event.event_type == "bcp.browser.unhealthy")
        );
        assert!(
            telemetry
                .samples
                .iter()
                .any(|sample| sample.name == "bcp.browser.restart.count")
        );
    }
}
