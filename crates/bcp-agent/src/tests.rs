
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
        expires_at_unix_ms: 0,
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
async fn installing_new_lease_revokes_prior_lease_for_same_profile() {
    // Arrange
    let gateway = Arc::new(RecordingPwrightGateway::new([RecordingProfileState {
        profile: profile(),
        healthy: true,
        health_message: "ok".to_string(),
        snapshot: vec![],
        eval_json: "{}".to_string(),
    }]));
    let service = AgentService::new(gateway);
    let old_lease = LeaseContext {
        lease_id: "lease-old".to_string(),
        profile_id: "youtube-main".to_string(),
        fencing_token: "fence-old".to_string(),
        expires_at_unix_ms: 0,
    };
    let new_lease = LeaseContext {
        lease_id: "lease-new".to_string(),
        profile_id: "youtube-main".to_string(),
        fencing_token: "fence-new".to_string(),
        expires_at_unix_ms: 0,
    };
    service.install_lease(
        &old_lease.lease_id,
        &old_lease.profile_id,
        &old_lease.fencing_token,
        0,
    );

    // Act
    service.install_lease(
        &new_lease.lease_id,
        &new_lease.profile_id,
        &new_lease.fencing_token,
        0,
    );

    // Assert
    let revoked = service
        .get_snapshot(Request::new(GetSnapshotRequest {
            lease: Some(old_lease),
        }))
        .await;
    assert_eq!(revoked.unwrap_err().code(), tonic::Code::PermissionDenied);
    let active = service
        .get_snapshot(Request::new(GetSnapshotRequest {
            lease: Some(new_lease),
        }))
        .await;
    assert!(active.is_ok());
}

#[tokio::test]
async fn validate_lease_rejects_expired_lease() {
    // Arrange: clock fixed at 10_000.
    let clock = Arc::new(FakeClock::new(10_000));
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
        clock,
    ));
    let service = AgentService::with_fleet(gateway, fleet);
    let context = |lease_id: &str, token: &str, expires: i64| LeaseContext {
        lease_id: lease_id.to_string(),
        profile_id: "youtube-main".to_string(),
        fencing_token: token.to_string(),
        expires_at_unix_ms: expires,
    };

    // Act: install a lease already past its deadline, then one in the future.
    service.install_lease("lease-exp", "youtube-main", "fence-1", 5_000);
    let expired = service
        .get_snapshot(Request::new(GetSnapshotRequest {
            lease: Some(context("lease-exp", "fence-1", 5_000)),
        }))
        .await;
    service.install_lease("lease-live", "youtube-main", "fence-2", 999_999);
    let live = service
        .get_snapshot(Request::new(GetSnapshotRequest {
            lease: Some(context("lease-live", "fence-2", 999_999)),
        }))
        .await;

    // Assert
    assert_eq!(
        expired.unwrap_err().code(),
        tonic::Code::PermissionDenied,
        "an expired lease must be rejected even without a revocation RPC"
    );
    assert!(live.is_ok());
}

#[test]
fn sync_leases_reconciles_install_map() {
    // Arrange: a stale local lease that the controller no longer holds.
    let service = AgentService::new(Arc::new(EmptyPwrightGateway));
    service.install_lease("stale", "p1", "token-stale", 0);
    let context = |lease_id: &str, token: &str| {
        Some(LeaseContext {
            lease_id: lease_id.to_string(),
            profile_id: "p1".to_string(),
            fencing_token: token.to_string(),
            expires_at_unix_ms: 0,
        })
    };

    // Act: reconcile to the controller's authoritative set (only "fresh").
    service.sync_leases(&[BrowserLease {
        lease_id: "fresh".to_string(),
        profile_id: "p1".to_string(),
        machine_id: "m".to_string(),
        fencing_token: "token-fresh".to_string(),
        expires_at_unix_ms: 0,
        ..Default::default()
    }]);

    // Assert: the stale lease is pruned; the controller's lease is installed.
    assert!(
        service
            .validate_lease(context("stale", "token-stale"))
            .is_err()
    );
    assert_eq!(
        service
            .validate_lease(context("fresh", "token-fresh"))
            .unwrap(),
        "p1"
    );
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
    service.install_lease("lease-1", "youtube-main", "fence-1", 0);

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
