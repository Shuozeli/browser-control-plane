pub(crate) use std::collections::HashMap;
pub(crate) use std::io::Write;
pub(crate) use std::path::PathBuf;
pub(crate) use std::pin::Pin;
pub(crate) use std::sync::{Arc, RwLock};

pub(crate) use bcp_core::artifact::{ArtifactError, ArtifactStore, ArtifactStoreConfig};
pub(crate) use bcp_core::fleet::{BrowserFleetManager, FleetTelemetry};
pub(crate) use bcp_core::id::UuidIdGenerator;
pub(crate) use bcp_core::pwright::{PwrightError, PwrightGateway, SharedPwrightGateway};
pub(crate) use bcp_core::time::{Clock, SystemClock};
pub(crate) use bcp_proto::browsercontrol::v1::download_artifact_response::Part as DownloadPart;
pub(crate) use bcp_proto::browsercontrol::v1::machine_controller_server::MachineController;
pub(crate) use bcp_proto::browsercontrol::v1::upload_artifact_request::Part;
pub(crate) use bcp_proto::browsercontrol::v1::*;
pub(crate) use tonic::{Request, Response, Status};

pub mod config;
pub mod lifecycle;
pub mod proxy;
mod service;
#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct AgentService {
    leases: Arc<RwLock<HashMap<String, LocalLease>>>,
    pwright: SharedPwrightGateway,
    fleet: Arc<BrowserFleetManager>,
    artifacts: Arc<ArtifactStore>,
    clock: Arc<dyn Clock>,
}

#[derive(Debug, Clone)]
struct LocalLease {
    profile_id: String,
    fencing_token: String,
    expires_at_unix_ms: i64,
}

impl AgentService {
    pub fn new(pwright: SharedPwrightGateway) -> Self {
        Self::new_for_machine("local-machine", pwright)
    }

    pub fn new_for_machine(machine_id: &str, pwright: SharedPwrightGateway) -> Self {
        let artifacts = default_artifact_store(machine_id);
        Self::with_components(
            pwright.clone(),
            Arc::new(BrowserFleetManager::new(
                machine_id,
                pwright,
                Arc::new(SystemClock),
            )),
            artifacts,
        )
    }

    pub fn with_fleet(pwright: SharedPwrightGateway, fleet: Arc<BrowserFleetManager>) -> Self {
        let artifacts = default_artifact_store("local-machine");
        Self::with_components(pwright, fleet, artifacts)
    }

    pub fn with_components(
        pwright: SharedPwrightGateway,
        fleet: Arc<BrowserFleetManager>,
        artifacts: Arc<ArtifactStore>,
    ) -> Self {
        let clock = fleet.clock();
        Self {
            leases: Arc::new(RwLock::new(HashMap::new())),
            pwright,
            fleet,
            artifacts,
            clock,
        }
    }

    pub fn upsert_desired_profile(&self, profile: BrowserProfile) {
        self.fleet.upsert_desired_profile(profile);
    }

    pub fn machine_id(&self) -> String {
        self.fleet.machine_id().to_string()
    }

    pub fn list_profiles(&self) -> Vec<BrowserProfile> {
        self.fleet.list_profiles()
    }

    pub async fn reconcile_fleet_once(&self) {
        self.fleet.reconcile_once().await;
    }

    pub fn drain_fleet_telemetry(&self) -> FleetTelemetry {
        self.fleet.drain_telemetry()
    }

    pub fn requeue_fleet_telemetry(&self, telemetry: FleetTelemetry) {
        self.fleet.requeue_telemetry(telemetry);
    }

    /// Reconciles the local installed-lease map to the controller's authoritative
    /// set of active leases for this machine: installs any that are missing (so a
    /// restarted agent recovers its leases) and drops any the controller no longer
    /// holds (so revocation is reliable even if the uninstall RPC was missed). The
    /// controller can never hold a lease this agent has not seen, so pruning only
    /// ever removes released/expired leases.
    pub fn sync_leases(&self, desired: &[BrowserLease]) {
        let mut leases = self.leases.write().expect("agent lease lock poisoned");
        let desired_ids: std::collections::HashSet<&str> = desired
            .iter()
            .map(|lease| lease.lease_id.as_str())
            .collect();
        leases.retain(|lease_id, _| desired_ids.contains(lease_id.as_str()));
        for lease in desired {
            leases.insert(
                lease.lease_id.clone(),
                LocalLease {
                    profile_id: lease.profile_id.clone(),
                    fencing_token: lease.fencing_token.clone(),
                    expires_at_unix_ms: lease.expires_at_unix_ms,
                },
            );
        }
    }

    #[allow(clippy::result_large_err)]
    pub fn cleanup_expired_artifacts(&self) -> Result<Vec<Artifact>, Status> {
        self.artifacts
            .cleanup_expired()
            .map_err(Self::artifact_error_to_status)
    }

    pub fn install_lease(
        &self,
        lease_id: &str,
        profile_id: &str,
        fencing_token: &str,
        expires_at_unix_ms: i64,
    ) {
        let mut leases = self.leases.write().expect("agent lease lock poisoned");
        // A profile can be leased to at most one client at a time. Installing a
        // new lease for a profile atomically revokes any previously installed
        // lease for the same profile, so a client whose lease was released or
        // expired (and the profile re-leased) can no longer pass `validate_lease`.
        leases.retain(|held_lease_id, lease| {
            lease.profile_id != profile_id || held_lease_id == lease_id
        });
        leases.insert(
            lease_id.to_string(),
            LocalLease {
                profile_id: profile_id.to_string(),
                fencing_token: fencing_token.to_string(),
                expires_at_unix_ms,
            },
        );
    }

    /// Removes an installed lease. The controller calls this on lease release or
    /// expiry so a stale holder can no longer pass lease validation. The fencing
    /// token must match, so a revoked client cannot evict its successor.
    pub fn uninstall_lease(&self, lease_id: &str, fencing_token: &str) -> bool {
        let mut leases = self.leases.write().expect("agent lease lock poisoned");
        match leases.get(lease_id) {
            Some(lease) if lease.fencing_token == fencing_token => {
                leases.remove(lease_id);
                true
            }
            _ => false,
        }
    }

    #[allow(clippy::result_large_err)]
    fn validate_lease(&self, lease: Option<LeaseContext>) -> Result<String, Status> {
        let lease = lease.ok_or_else(|| Status::invalid_argument("lease is required"))?;
        if lease.lease_id.is_empty() {
            return Err(Status::invalid_argument("lease_id is required"));
        }
        if lease.profile_id.is_empty() {
            return Err(Status::invalid_argument("profile_id is required"));
        }
        if lease.fencing_token.is_empty() {
            return Err(Status::invalid_argument("fencing_token is required"));
        }
        let leases = self.leases.read().expect("agent lease lock poisoned");
        let local = leases
            .get(&lease.lease_id)
            .ok_or_else(|| Status::permission_denied("lease is not installed on this machine"))?;
        if local.profile_id != lease.profile_id {
            return Err(Status::permission_denied("lease profile mismatch"));
        }
        if local.fencing_token != lease.fencing_token {
            return Err(Status::permission_denied("invalid fencing token"));
        }
        // Agent-side expiry: a lease past its deadline is rejected even if the
        // controller's best-effort revocation never arrived. Fencing no longer
        // depends on the uninstall RPC landing.
        if local.expires_at_unix_ms > 0 && self.clock.now_unix_ms() >= local.expires_at_unix_ms {
            return Err(Status::permission_denied("lease expired"));
        }
        Ok(lease.profile_id)
    }

    /// Lease check for the raw CDP proxy, which cannot surface a gRPC `Status`.
    /// Applies the same rules as `validate_lease`: the lease must be installed,
    /// match the profile and fencing token, and not be expired.
    pub fn check_lease(&self, lease_id: &str, profile_id: &str, fencing_token: &str) -> bool {
        if lease_id.is_empty() || profile_id.is_empty() || fencing_token.is_empty() {
            return false;
        }
        let leases = self.leases.read().expect("agent lease lock poisoned");
        let Some(local) = leases.get(lease_id) else {
            return false;
        };
        if local.profile_id != profile_id || local.fencing_token != fencing_token {
            return false;
        }
        if local.expires_at_unix_ms > 0 && self.clock.now_unix_ms() >= local.expires_at_unix_ms {
            return false;
        }
        true
    }

    /// Resolve a profile's local Chrome DevTools HTTP base URL (e.g.
    /// `http://127.0.0.1:9222`) so the proxy can reach and rewrite it.
    pub fn profile_cdp_url(&self, profile_id: &str) -> Option<String> {
        self.list_profiles()
            .into_iter()
            .find(|profile| profile.profile_id == profile_id)
            .map(|profile| profile.cdp_url)
            .filter(|url| !url.is_empty())
    }

    fn pwright_error_to_status(error: PwrightError) -> Status {
        match error {
            PwrightError::ProfileNotFound(_) => Status::not_found(error.to_string()),
            PwrightError::Unhealthy(_, _) => Status::failed_precondition(error.to_string()),
            PwrightError::OperationFailed(_, _) => Status::failed_precondition(error.to_string()),
        }
    }

    fn artifact_error_to_status(error: ArtifactError) -> Status {
        match error {
            ArtifactError::MissingTtl
            | ArtifactError::TtlTooLong(_)
            | ArtifactError::MissingFilename
            | ArtifactError::MissingLease
            | ArtifactError::MissingLeaseId
            | ArtifactError::MissingProfileId => Status::invalid_argument(error.to_string()),
            ArtifactError::NotFound(_) => Status::not_found(error.to_string()),
            ArtifactError::Io(_) | ArtifactError::Sqlite(_) => Status::internal(error.to_string()),
        }
    }
}

fn default_artifact_store(machine_id: &str) -> Arc<ArtifactStore> {
    let root_dir = std::env::var("BCP_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".bcp").join("artifacts"));
    let max_ttl_seconds = std::env::var("BCP_ARTIFACT_MAX_TTL_SECONDS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(24 * 60 * 60);
    Arc::new(
        ArtifactStore::open(
            ArtifactStoreConfig {
                machine_id: machine_id.to_string(),
                root_dir,
                max_ttl_seconds,
            },
            Arc::new(SystemClock),
            Arc::new(UuidIdGenerator),
        )
        .expect("artifact store should open"),
    )
}

impl Default for AgentService {
    fn default() -> Self {
        Self::new(Arc::new(EmptyPwrightGateway))
    }
}

struct EmptyPwrightGateway;

#[tonic::async_trait]
impl PwrightGateway for EmptyPwrightGateway {
    async fn ensure_browser(&self, profile_id: &str) -> Result<BrowserProfile, PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }

    async fn stop_browser(&self, profile_id: &str) -> Result<bool, PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }

    async fn check_browser(
        &self,
        profile_id: &str,
    ) -> Result<bcp_core::pwright::BrowserHealth, PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }

    async fn get_snapshot(&self, profile_id: &str) -> Result<Vec<A11yNode>, PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }

    async fn execute_action(
        &self,
        profile_id: &str,
        _request: ExecuteActionRequest,
    ) -> Result<ExecuteActionResponse, PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }

    async fn evaluate(&self, profile_id: &str, _expression: &str) -> Result<String, PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }

    async fn run_script(
        &self,
        profile_id: &str,
        _yaml: &str,
        _params: HashMap<String, String>,
    ) -> Result<Vec<RunScriptResponse>, PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }

    async fn capture_screenshot(
        &self,
        profile_id: &str,
        _format: &str,
        _full_page: bool,
    ) -> Result<String, PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }

    async fn print_pdf(&self, profile_id: &str) -> Result<String, PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }

    async fn get_cookies(&self, profile_id: &str) -> Result<String, PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }

    async fn set_cookies(
        &self,
        profile_id: &str,
        _cookies_json: &str,
    ) -> Result<u32, PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }

    async fn get_page(
        &self,
        profile_id: &str,
    ) -> Result<bcp_core::pwright::PageInfo, PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }

    async fn set_input_files(
        &self,
        profile_id: &str,
        _selector: &str,
        _files: &[String],
    ) -> Result<(), PwrightError> {
        Err(PwrightError::ProfileNotFound(profile_id.to_string()))
    }
}
