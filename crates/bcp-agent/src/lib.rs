use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use bcp_core::artifact::{ArtifactError, ArtifactStore, ArtifactStoreConfig};
use bcp_core::fleet::{BrowserFleetManager, FleetTelemetry};
use bcp_core::id::UuidIdGenerator;
use bcp_core::pwright::{PwrightError, PwrightGateway, SharedPwrightGateway};
use bcp_core::time::SystemClock;
use bcp_proto::browsercontrol::v1::machine_controller_server::MachineController;
use bcp_proto::browsercontrol::v1::upload_artifact_request::Part;
use bcp_proto::browsercontrol::v1::*;
use tonic::{Request, Response, Status};

pub mod config;
pub mod lifecycle;

#[derive(Clone)]
pub struct AgentService {
    leases: Arc<RwLock<HashMap<String, LocalLease>>>,
    pwright: SharedPwrightGateway,
    fleet: Arc<BrowserFleetManager>,
    artifacts: Arc<ArtifactStore>,
}

#[derive(Debug, Clone)]
struct LocalLease {
    profile_id: String,
    fencing_token: String,
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
        Self {
            leases: Arc::new(RwLock::new(HashMap::new())),
            pwright,
            fleet,
            artifacts,
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

    #[allow(clippy::result_large_err)]
    pub fn cleanup_expired_artifacts(&self) -> Result<Vec<Artifact>, Status> {
        self.artifacts
            .cleanup_expired()
            .map_err(Self::artifact_error_to_status)
    }

    pub fn install_lease(&self, lease_id: &str, profile_id: &str, fencing_token: &str) {
        self.leases
            .write()
            .expect("agent lease lock poisoned")
            .insert(
                lease_id.to_string(),
                LocalLease {
                    profile_id: profile_id.to_string(),
                    fencing_token: fencing_token.to_string(),
                },
            );
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
        Ok(lease.profile_id)
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

#[tonic::async_trait]
impl MachineController for AgentService {
    async fn install_lease(
        &self,
        request: Request<InstallLeaseRequest>,
    ) -> Result<Response<InstallLeaseResponse>, Status> {
        let lease = request
            .into_inner()
            .lease
            .ok_or_else(|| Status::invalid_argument("lease is required"))?;
        if lease.lease_id.is_empty() {
            return Err(Status::invalid_argument("lease_id is required"));
        }
        if lease.profile_id.is_empty() {
            return Err(Status::invalid_argument("profile_id is required"));
        }
        if lease.fencing_token.is_empty() {
            return Err(Status::invalid_argument("fencing_token is required"));
        }
        self.install_lease(&lease.lease_id, &lease.profile_id, &lease.fencing_token);
        Ok(Response::new(InstallLeaseResponse { installed: true }))
    }

    async fn list_local_profiles(
        &self,
        _request: Request<ListLocalProfilesRequest>,
    ) -> Result<Response<ListLocalProfilesResponse>, Status> {
        Ok(Response::new(ListLocalProfilesResponse {
            profiles: self.fleet.list_profiles(),
        }))
    }

    async fn ensure_browser(
        &self,
        request: Request<EnsureBrowserRequest>,
    ) -> Result<Response<EnsureBrowserResponse>, Status> {
        let profile_id = self.validate_lease(request.into_inner().lease)?;
        let profile = self
            .pwright
            .ensure_browser(&profile_id)
            .await
            .map_err(Self::pwright_error_to_status)?;
        Ok(Response::new(EnsureBrowserResponse {
            profile: Some(profile),
        }))
    }

    async fn stop_browser(
        &self,
        request: Request<StopBrowserRequest>,
    ) -> Result<Response<StopBrowserResponse>, Status> {
        let profile_id = self.validate_lease(request.into_inner().lease)?;
        let stopped = self
            .pwright
            .stop_browser(&profile_id)
            .await
            .map_err(Self::pwright_error_to_status)?;
        Ok(Response::new(StopBrowserResponse { stopped }))
    }

    async fn check_browser(
        &self,
        request: Request<CheckBrowserRequest>,
    ) -> Result<Response<CheckBrowserResponse>, Status> {
        let profile_id = self.validate_lease(request.into_inner().lease)?;
        let health = self
            .pwright
            .check_browser(&profile_id)
            .await
            .map_err(Self::pwright_error_to_status)?;
        Ok(Response::new(CheckBrowserResponse {
            healthy: health.healthy,
            message: health.message,
        }))
    }

    async fn get_snapshot(
        &self,
        request: Request<GetSnapshotRequest>,
    ) -> Result<Response<GetSnapshotResponse>, Status> {
        let profile_id = self.validate_lease(request.into_inner().lease)?;
        let nodes = self
            .pwright
            .get_snapshot(&profile_id)
            .await
            .map_err(Self::pwright_error_to_status)?;
        Ok(Response::new(GetSnapshotResponse { nodes }))
    }

    async fn execute_action(
        &self,
        request: Request<ExecuteActionRequest>,
    ) -> Result<Response<ExecuteActionResponse>, Status> {
        let request = request.into_inner();
        let profile_id = self.validate_lease(request.lease.clone())?;
        let response = self
            .pwright
            .execute_action(&profile_id, request)
            .await
            .map_err(Self::pwright_error_to_status)?;
        Ok(Response::new(response))
    }

    async fn evaluate(
        &self,
        request: Request<EvaluateRequest>,
    ) -> Result<Response<EvaluateResponse>, Status> {
        let request = request.into_inner();
        let profile_id = self.validate_lease(request.lease)?;
        let json_result = self
            .pwright
            .evaluate(&profile_id, &request.expression)
            .await
            .map_err(Self::pwright_error_to_status)?;
        Ok(Response::new(EvaluateResponse { json_result }))
    }

    type RunScriptStream = Pin<
        Box<
            dyn tonic::codegen::tokio_stream::Stream<Item = Result<RunScriptResponse, Status>>
                + Send
                + 'static,
        >,
    >;

    async fn run_script(
        &self,
        request: Request<RunScriptRequest>,
    ) -> Result<Response<Self::RunScriptStream>, Status> {
        let request = request.into_inner();
        let profile_id = self.validate_lease(request.lease)?;
        let lines = self
            .pwright
            .run_script(&profile_id, &request.yaml, request.params)
            .await
            .map_err(Self::pwright_error_to_status)?;
        let stream = tonic::codegen::tokio_stream::iter(lines.into_iter().map(Ok));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn upload_artifact(
        &self,
        request: Request<tonic::Streaming<UploadArtifactRequest>>,
    ) -> Result<Response<UploadArtifactResponse>, Status> {
        let mut stream = request.into_inner();
        let metadata = match stream.message().await? {
            Some(UploadArtifactRequest {
                part: Some(Part::Metadata(metadata)),
            }) => metadata,
            Some(_) => {
                return Err(Status::invalid_argument(
                    "first upload message must contain metadata",
                ));
            }
            None => return Err(Status::invalid_argument("upload stream is empty")),
        };
        let lease = metadata
            .lease
            .clone()
            .ok_or_else(|| Status::invalid_argument("lease is required"))?;
        let profile_id = self.validate_lease(Some(lease.clone()))?;
        if profile_id != lease.profile_id {
            return Err(Status::permission_denied("lease profile mismatch"));
        }

        let ticket = self
            .artifacts
            .reserve(
                &lease,
                &metadata.original_filename,
                &metadata.content_type,
                &metadata.purpose,
                metadata.ttl_seconds,
            )
            .map_err(Self::artifact_error_to_status)?;
        let mut file = std::fs::File::create(&ticket.temp_path)
            .map_err(|error| Status::internal(error.to_string()))?;
        let mut size_bytes = 0_i64;

        while let Some(message) = stream.message().await? {
            match message.part {
                Some(Part::Chunk(chunk)) => {
                    size_bytes += chunk.len() as i64;
                    if let Err(error) = file.write_all(&chunk) {
                        let _ = self.artifacts.fail_upload(&ticket);
                        return Err(Status::internal(error.to_string()));
                    }
                }
                Some(Part::Metadata(_)) => {
                    let _ = self.artifacts.fail_upload(&ticket);
                    return Err(Status::invalid_argument(
                        "metadata is only allowed as the first upload message",
                    ));
                }
                None => {}
            }
        }
        drop(file);

        let artifact = self
            .artifacts
            .commit(&ticket, size_bytes)
            .map_err(Self::artifact_error_to_status)?;
        Ok(Response::new(UploadArtifactResponse {
            artifact: Some(artifact),
        }))
    }

    async fn list_local_artifacts(
        &self,
        request: Request<ListLocalArtifactsRequest>,
    ) -> Result<Response<ListLocalArtifactsResponse>, Status> {
        let request = request.into_inner();
        let profile_id = (!request.profile_id.is_empty()).then_some(request.profile_id.as_str());
        let lease_id = (!request.lease_id.is_empty()).then_some(request.lease_id.as_str());
        let artifacts = self
            .artifacts
            .list(profile_id, lease_id, request.include_expired)
            .map_err(Self::artifact_error_to_status)?;
        Ok(Response::new(ListLocalArtifactsResponse { artifacts }))
    }

    async fn delete_artifact(
        &self,
        request: Request<DeleteArtifactRequest>,
    ) -> Result<Response<DeleteArtifactResponse>, Status> {
        let artifact_id = request.into_inner().artifact_id;
        if artifact_id.is_empty() {
            return Err(Status::invalid_argument("artifact_id is required"));
        }
        let deleted = self
            .artifacts
            .delete(&artifact_id)
            .map_err(Self::artifact_error_to_status)?;
        Ok(Response::new(DeleteArtifactResponse { deleted }))
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
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::Arc;

    use bcp_core::artifact::{ArtifactStore, ArtifactStoreConfig};
    use bcp_core::fleet::BrowserFleetManager;
    use bcp_core::id::FakeIdGenerator;
    use bcp_core::pwright::{RecordingProfileState, RecordingPwrightGateway};
    use bcp_core::time::FakeClock;
    use bcp_proto::browsercontrol::v1::machine_controller_server::MachineController;

    use super::*;

    fn profile() -> BrowserProfile {
        BrowserProfile {
            profile_id: "youtube-main".to_string(),
            machine_id: "machine-a".to_string(),
            profile_path: "/profiles/youtube-main".to_string(),
            display_name: "YouTube Main".to_string(),
            status: ProfileStatus::Available as i32,
            cdp_url: "http://127.0.0.1:9312".to_string(),
            cdp_port: 9312,
            accounts: vec![],
            labels: HashMap::new(),
            last_seen_unix_ms: 0,
        }
    }

    fn lease() -> LeaseContext {
        LeaseContext {
            lease_id: "lease-1".to_string(),
            profile_id: "youtube-main".to_string(),
            fencing_token: "fence-1".to_string(),
        }
    }

    fn artifact_store(clock: Arc<FakeClock>) -> (tempfile::TempDir, Arc<ArtifactStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(
            ArtifactStoreConfig {
                machine_id: "machine-a".to_string(),
                root_dir: dir.path().to_path_buf(),
                max_ttl_seconds: 60,
            },
            clock,
            Arc::new(FakeIdGenerator::new(["artifact-1"])),
        )
        .unwrap();
        (dir, Arc::new(store))
    }

    #[tokio::test]
    async fn get_snapshot_requires_installed_lease() {
        // Arrange
        let gateway = Arc::new(RecordingPwrightGateway::new([RecordingProfileState {
            profile: profile(),
            healthy: true,
            health_message: "ok".to_string(),
            snapshot: vec![],
            eval_json: "{}".to_string(),
        }]));
        let service = AgentService::new(gateway);

        // Act
        let result = service
            .get_snapshot(Request::new(GetSnapshotRequest {
                lease: Some(lease()),
            }))
            .await;

        // Assert
        assert_eq!(result.unwrap_err().code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn execute_action_routes_through_pwright_gateway() {
        // Arrange
        let gateway = Arc::new(RecordingPwrightGateway::new([RecordingProfileState {
            profile: profile(),
            healthy: true,
            health_message: "ok".to_string(),
            snapshot: vec![A11yNode {
                r#ref: "e1".to_string(),
                role: "button".to_string(),
                name: "Publish".to_string(),
                depth: 1,
                value: String::new(),
            }],
            eval_json: r#"{"title":"ok"}"#.to_string(),
        }]));
        let service = AgentService::new(gateway.clone());
        service.install_lease("lease-1", "youtube-main", "fence-1");

        // Act
        let response = service
            .execute_action(Request::new(ExecuteActionRequest {
                lease: Some(lease()),
                action: "click".to_string(),
                r#ref: "e1".to_string(),
                text: String::new(),
                key: String::new(),
                options: HashMap::new(),
            }))
            .await
            .unwrap()
            .into_inner();

        // Assert
        assert!(response.success);
        let actions = gateway.actions();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].0, "youtube-main");
        assert_eq!(actions[0].1.action, "click");
        assert_eq!(actions[0].1.r#ref, "e1");
    }

    #[tokio::test]
    async fn list_local_profiles_returns_profiles_from_local_fleet() {
        // Arrange
        let gateway = Arc::new(RecordingPwrightGateway::new([RecordingProfileState {
            profile: profile(),
            healthy: true,
            health_message: "ok".to_string(),
            snapshot: vec![],
            eval_json: "{}".to_string(),
        }]));
        let fleet = Arc::new(BrowserFleetManager::new(
            "machine-a",
            gateway.clone(),
            Arc::new(FakeClock::new(1_000)),
        ));
        let service = AgentService::with_fleet(gateway, fleet);
        service.upsert_desired_profile(profile());

        // Act
        let response = service
            .list_local_profiles(Request::new(ListLocalProfilesRequest {}))
            .await
            .unwrap()
            .into_inner();

        // Assert
        assert_eq!(response.profiles.len(), 1);
        assert_eq!(response.profiles[0].profile_id, "youtube-main");
    }

    #[tokio::test]
    async fn reconcile_fleet_once_records_browser_health_telemetry() {
        // Arrange
        let gateway = Arc::new(RecordingPwrightGateway::new([RecordingProfileState {
            profile: profile(),
            healthy: true,
            health_message: "ok".to_string(),
            snapshot: vec![],
            eval_json: "{}".to_string(),
        }]));
        let fleet = Arc::new(BrowserFleetManager::new(
            "machine-a",
            gateway.clone(),
            Arc::new(FakeClock::new(1_000)),
        ));
        let service = AgentService::with_fleet(gateway, fleet);
        service.upsert_desired_profile(profile());

        // Act
        service.reconcile_fleet_once().await;
        let telemetry = service.drain_fleet_telemetry();

        // Assert
        assert!(
            telemetry
                .samples
                .iter()
                .any(|sample| sample.name == "bcp.browser.running" && sample.value == 1.0)
        );
    }

    #[tokio::test]
    async fn list_local_artifacts_returns_available_uploaded_files() {
        // Arrange
        let clock = Arc::new(FakeClock::new(1_000));
        let (_dir, artifacts) = artifact_store(clock);
        let gateway = Arc::new(RecordingPwrightGateway::new([RecordingProfileState {
            profile: profile(),
            healthy: true,
            health_message: "ok".to_string(),
            snapshot: vec![],
            eval_json: "{}".to_string(),
        }]));
        let fleet = Arc::new(BrowserFleetManager::new(
            "machine-a",
            gateway.clone(),
            Arc::new(FakeClock::new(1_000)),
        ));
        let service = AgentService::with_components(gateway, fleet, artifacts.clone());
        let ticket = artifacts
            .reserve(&lease(), "video.mp4", "video/mp4", "youtube-upload", 10)
            .unwrap();
        let mut file = std::fs::File::create(&ticket.temp_path).unwrap();
        file.write_all(b"abc").unwrap();
        artifacts.commit(&ticket, 3).unwrap();

        // Act
        let response = service
            .list_local_artifacts(Request::new(ListLocalArtifactsRequest {
                profile_id: "youtube-main".to_string(),
                lease_id: String::new(),
                include_expired: false,
            }))
            .await
            .unwrap()
            .into_inner();

        // Assert
        assert_eq!(response.artifacts.len(), 1);
        assert_eq!(response.artifacts[0].artifact_id, "artifact-1");
        assert_eq!(response.artifacts[0].size_bytes, 3);
    }

    #[tokio::test]
    async fn cleanup_expired_artifacts_deletes_local_files() {
        // Arrange
        let clock = Arc::new(FakeClock::new(1_000));
        let (_dir, artifacts) = artifact_store(clock.clone());
        let gateway = Arc::new(RecordingPwrightGateway::new([RecordingProfileState {
            profile: profile(),
            healthy: true,
            health_message: "ok".to_string(),
            snapshot: vec![],
            eval_json: "{}".to_string(),
        }]));
        let fleet = Arc::new(BrowserFleetManager::new(
            "machine-a",
            gateway.clone(),
            Arc::new(FakeClock::new(1_000)),
        ));
        let service = AgentService::with_components(gateway, fleet, artifacts.clone());
        let ticket = artifacts
            .reserve(&lease(), "video.mp4", "video/mp4", "youtube-upload", 1)
            .unwrap();
        std::fs::write(&ticket.temp_path, b"abc").unwrap();
        artifacts.commit(&ticket, 3).unwrap();
        clock.advance_ms(1_001);

        // Act
        let deleted = service.cleanup_expired_artifacts().unwrap();

        // Assert
        assert_eq!(deleted.len(), 1);
        assert!(!ticket.final_path.exists());
    }
}
