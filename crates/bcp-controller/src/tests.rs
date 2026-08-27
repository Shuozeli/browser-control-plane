
use std::collections::HashMap;
use std::sync::Arc;

use bcp_core::id::FakeIdGenerator;
use bcp_core::network::StaticNetworkDirectory;
use bcp_core::time::FakeClock;
use bcp_proto::browsercontrol::v1::global_controller_server::GlobalController;

use super::*;

fn test_service() -> ControllerService {
    let (service, _) = test_service_with_clock();
    service
}

fn test_service_with_clock() -> (ControllerService, FakeClock) {
    let clock = FakeClock::new(1_000);
    let service = ControllerService::new(
        Arc::new(clock.clone()),
        Arc::new(FakeIdGenerator::new([
            "lease_1", "fence_1", "lease_2", "fence_2",
        ])),
        Arc::new(StaticNetworkDirectory::default()),
    );
    (service, clock)
}

#[tokio::test]
async fn sweep_marks_stale_online_machine_offline() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(Machine {
                machine_id: "m1".to_string(),
                status: MachineStatus::Online as i32,
                ..Default::default()
            }),
            profiles: vec![],
        }))
        .await
        .unwrap();

    // Act: sweep far past the machine's registration heartbeat.
    service.sweep_at(5_000_000, 60_000);

    // Assert
    let machines = service
        .list_machines(Request::new(ListMachinesRequest {
            label_selector: HashMap::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    let machine = machines
        .machines
        .iter()
        .find(|machine| machine.machine_id == "m1")
        .unwrap();
    assert_eq!(machine.status, MachineStatus::Offline as i32);
}

#[tokio::test]
async fn acquire_clamps_excessive_ttl_without_overflow() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(Machine {
                machine_id: "m1".to_string(),
                status: MachineStatus::Online as i32,
                ..Default::default()
            }),
            profiles: vec![BrowserProfile {
                profile_id: "p1".to_string(),
                machine_id: "m1".to_string(),
                status: ProfileStatus::Available as i32,
                accounts: vec![Account {
                    account_id: "acct".to_string(),
                    platform: AccountPlatform::Youtube as i32,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }))
        .await
        .unwrap();

    // Act: a huge TTL would overflow `now + ttl * 1000` without clamping.
    let acquired = service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "c".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "acct".to_string(),
            ttl_seconds: i64::MAX,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();

    // Assert: no panic, and expiry is clamped to the max TTL from `now` (1000).
    let lease = acquired.lease.unwrap();
    assert_eq!(
        lease.expires_at_unix_ms,
        1_000 + MAX_LEASE_TTL_SECONDS * 1000
    );
}

#[tokio::test]
async fn list_machine_leases_returns_only_active_leases_for_machine() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(Machine {
                machine_id: "m1".to_string(),
                status: MachineStatus::Online as i32,
                ..Default::default()
            }),
            profiles: vec![BrowserProfile {
                profile_id: "p1".to_string(),
                machine_id: "m1".to_string(),
                status: ProfileStatus::Available as i32,
                accounts: vec![Account {
                    account_id: "acct".to_string(),
                    platform: AccountPlatform::Youtube as i32,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }))
        .await
        .unwrap();
    let acquired = service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "c".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "acct".to_string(),
            ttl_seconds: 60,
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();

    // Act + Assert: the owning machine sees exactly its lease.
    let owned = service
        .list_machine_leases(Request::new(ListMachineLeasesRequest {
            machine_id: "m1".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(owned.leases.len(), 1);
    assert_eq!(owned.leases[0].lease_id, acquired.lease.unwrap().lease_id);

    // A different machine sees none.
    let other = service
        .list_machine_leases(Request::new(ListMachineLeasesRequest {
            machine_id: "m2".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(other.leases.is_empty());
}

#[tokio::test]
async fn quarantine_blocks_acquire_until_released() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(Machine {
                machine_id: "m1".to_string(),
                status: MachineStatus::Online as i32,
                ..Default::default()
            }),
            profiles: vec![BrowserProfile {
                profile_id: "p1".to_string(),
                machine_id: "m1".to_string(),
                status: ProfileStatus::Available as i32,
                accounts: vec![Account {
                    account_id: "acct".to_string(),
                    platform: AccountPlatform::Youtube as i32,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }))
        .await
        .unwrap();
    let acquire = || {
        Request::new(AcquireBrowserRequest {
            client_id: "c".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "acct".to_string(),
            ttl_seconds: 60,
            ..Default::default()
        })
    };

    // Act
    service
        .quarantine_profile(Request::new(QuarantineProfileRequest {
            profile_id: "p1".to_string(),
            reason: "flaky".to_string(),
        }))
        .await
        .unwrap();
    let blocked = service.acquire_browser(acquire()).await.unwrap_err();
    service
        .release_quarantine(Request::new(ReleaseQuarantineRequest {
            profile_id: "p1".to_string(),
        }))
        .await
        .unwrap();
    let allowed = service.acquire_browser(acquire()).await;

    // Assert
    assert_eq!(blocked.code(), tonic::Code::NotFound);
    assert!(allowed.is_ok());
}

#[tokio::test]
async fn quarantine_survives_agent_reregistration() {
    // Arrange
    let service = test_service();
    let register = || {
        Request::new(RegisterMachineRequest {
            machine: Some(Machine {
                machine_id: "m1".to_string(),
                status: MachineStatus::Online as i32,
                ..Default::default()
            }),
            profiles: vec![BrowserProfile {
                profile_id: "p1".to_string(),
                machine_id: "m1".to_string(),
                status: ProfileStatus::Available as i32,
                accounts: vec![Account {
                    account_id: "acct".to_string(),
                    platform: AccountPlatform::Youtube as i32,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        })
    };
    service.register_machine(register()).await.unwrap();
    service
        .quarantine_profile(Request::new(QuarantineProfileRequest {
            profile_id: "p1".to_string(),
            reason: "flaky".to_string(),
        }))
        .await
        .unwrap();

    // Act: the agent re-registers, reporting the profile as Available again.
    service.register_machine(register()).await.unwrap();

    // Assert: quarantine survived, so acquire is still blocked.
    let blocked = service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "c".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "acct".to_string(),
            ttl_seconds: 60,
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(blocked.code(), tonic::Code::NotFound);
}

fn sqlite_test_service(path: &std::path::Path) -> ControllerService {
    ControllerService::new_with_sqlite(
        path,
        Arc::new(FakeClock::new(1_000)),
        Arc::new(FakeIdGenerator::new([
            "lease_1", "fence_1", "lease_2", "fence_2",
        ])),
        Arc::new(StaticNetworkDirectory::default()),
    )
    .unwrap()
}

fn machine() -> Machine {
    Machine {
        machine_id: "machine-a".to_string(),
        hostname: "machine-a".to_string(),
        tailscale_host: "machine-a.tail.test".to_string(),
        agent_grpc_addr: "http://machine-a.tail.test:7100".to_string(),
        status: MachineStatus::Online as i32,
        labels: HashMap::from([("pool".to_string(), "prod".to_string())]),
        last_heartbeat_unix_ms: 0,
    }
}

fn profile() -> BrowserProfile {
    BrowserProfile {
        profile_id: "youtube-main".to_string(),
        machine_id: "machine-a".to_string(),
        profile_path: "/profiles/youtube-main".to_string(),
        display_name: "YouTube Main".to_string(),
        status: ProfileStatus::Available as i32,
        cdp_url: String::new(),
        cdp_port: 0,
        accounts: vec![Account {
            account_id: "yt-1".to_string(),
            platform: AccountPlatform::Youtube as i32,
            handle: "@main".to_string(),
            health: "logged_in".to_string(),
            capabilities: vec!["upload".to_string()],
        }],
        labels: HashMap::from([("tier".to_string(), "prod".to_string())]),
        last_seen_unix_ms: 0,
    }
}

fn reddit_profile() -> BrowserProfile {
    BrowserProfile {
        profile_id: "reddit-main".to_string(),
        machine_id: "machine-a".to_string(),
        profile_path: "/profiles/reddit-main".to_string(),
        display_name: "Reddit Main".to_string(),
        status: ProfileStatus::Available as i32,
        cdp_url: String::new(),
        cdp_port: 0,
        accounts: vec![Account {
            account_id: "reddit-1".to_string(),
            platform: AccountPlatform::Reddit as i32,
            handle: "main".to_string(),
            health: "logged_in".to_string(),
            capabilities: vec!["post".to_string()],
        }],
        labels: HashMap::from([("tier".to_string(), "prod".to_string())]),
        last_seen_unix_ms: 0,
    }
}

#[tokio::test]
async fn register_machine_indexes_browser_account_bindings() {
    // Arrange
    let service = test_service();

    // Act
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();
    let response = service
        .list_browser_account_bindings(Request::new(ListBrowserAccountBindingsRequest {
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            machine_id: String::new(),
            profile_id: String::new(),
            include_unavailable: false,
        }))
        .await
        .unwrap()
        .into_inner();

    // Assert
    assert_eq!(response.bindings.len(), 1);
    let binding = &response.bindings[0];
    assert_eq!(binding.binding_id, "1:yt-1");
    assert_eq!(binding.profile_id, "youtube-main");
    assert_eq!(binding.machine_id, "machine-a");
    assert_eq!(binding.platform, AccountPlatform::Youtube as i32);
    assert_eq!(binding.account_id, "yt-1");
    assert_eq!(binding.handle, "@main");
    assert_eq!(binding.account_health, "logged_in");
    assert_eq!(binding.profile_status, ProfileStatus::Available as i32);
    assert_eq!(binding.agent_grpc_addr, "http://machine-a.tail.test:7100");
    assert_eq!(binding.last_seen_unix_ms, 1_000);
}

#[tokio::test]
async fn web_snapshot_summarizes_fleet_without_fencing_tokens() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();
    let lease = service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "client-1".to_string(),
            purpose: "dashboard".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 60,
        }))
        .await
        .unwrap()
        .into_inner()
        .lease
        .unwrap();

    // Act
    let snapshot = service.web_snapshot();
    let raw = serde_json::to_string(&snapshot).unwrap();

    // Assert
    assert_eq!(snapshot.counts.machines, 1);
    assert_eq!(snapshot.counts.profiles, 1);
    assert_eq!(snapshot.counts.accounts, 1);
    assert_eq!(snapshot.counts.active_leases, 1);
    assert_eq!(snapshot.leases[0].lease_id, lease.lease_id);
    assert!(!raw.contains(&lease.fencing_token));
}

#[tokio::test]
async fn sqlite_store_restores_registered_account_bindings() {
    // Arrange
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("controller.sqlite");
    let service = sqlite_test_service(&db_path);
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();
    drop(service);

    // Act
    let restored = sqlite_test_service(&db_path);
    let response = restored
        .lookup_browser_connection(Request::new(LookupBrowserConnectionRequest {
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            include_unavailable: true,
        }))
        .await
        .unwrap()
        .into_inner();

    // Assert
    let binding = response.binding.unwrap();
    let route = response.route_hint.unwrap();
    assert_eq!(binding.profile_id, "youtube-main");
    assert_eq!(binding.machine_id, "machine-a");
    assert_eq!(route.agent_grpc_addr, "http://machine-a.tail.test:7100");
}

#[tokio::test]
async fn sqlite_store_restores_active_lease_state() {
    // Arrange
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("controller.sqlite");
    let service = sqlite_test_service(&db_path);
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();
    let lease = service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "client-1".to_string(),
            purpose: "upload".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 60,
        }))
        .await
        .unwrap()
        .into_inner()
        .lease
        .unwrap();
    drop(service);

    // Act
    let restored = sqlite_test_service(&db_path);
    let response = restored
        .lookup_browser_connection(Request::new(LookupBrowserConnectionRequest {
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            include_unavailable: true,
        }))
        .await
        .unwrap()
        .into_inner();
    let second_acquire = restored
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "client-2".to_string(),
            purpose: "upload".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 60,
        }))
        .await;

    // Assert
    assert!(!response.available);
    assert_eq!(response.active_lease_id, lease.lease_id);
    assert_eq!(second_acquire.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn register_machine_rejects_duplicate_account_binding_on_different_profile() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();
    let mut duplicate = profile();
    duplicate.profile_id = "youtube-shadow".to_string();
    duplicate.profile_path = "/profiles/youtube-shadow".to_string();

    // Act
    let result = service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![duplicate],
        }))
        .await;

    // Assert
    assert_eq!(result.unwrap_err().code(), tonic::Code::AlreadyExists);
    let response = service
        .list_browser_account_bindings(Request::new(ListBrowserAccountBindingsRequest {
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            machine_id: String::new(),
            profile_id: String::new(),
            include_unavailable: true,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.bindings.len(), 1);
    assert_eq!(response.bindings[0].profile_id, "youtube-main");
}

#[tokio::test]
async fn failed_register_machine_does_not_partially_commit_profiles() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();
    let mut duplicate = profile();
    duplicate.profile_id = "youtube-shadow".to_string();

    // Act
    let result = service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![reddit_profile(), duplicate],
        }))
        .await;

    // Assert
    assert_eq!(result.unwrap_err().code(), tonic::Code::AlreadyExists);
    let profiles = service
        .list_profiles(Request::new(ListProfilesRequest {
            platform: AccountPlatform::Unspecified as i32,
            account_id: String::new(),
            label_selector: HashMap::new(),
            include_unavailable: true,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(profiles.profiles.len(), 1);
    assert_eq!(profiles.profiles[0].profile_id, "youtube-main");
}

#[tokio::test]
async fn list_browser_account_bindings_reflects_live_profile_status() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();

    // Act
    service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "client-1".to_string(),
            purpose: "upload".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 60,
        }))
        .await
        .unwrap();
    let hidden = service
        .list_browser_account_bindings(Request::new(ListBrowserAccountBindingsRequest {
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            machine_id: String::new(),
            profile_id: String::new(),
            include_unavailable: false,
        }))
        .await
        .unwrap()
        .into_inner();
    let visible = service
        .list_browser_account_bindings(Request::new(ListBrowserAccountBindingsRequest {
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            machine_id: String::new(),
            profile_id: String::new(),
            include_unavailable: true,
        }))
        .await
        .unwrap()
        .into_inner();

    // Assert
    assert!(hidden.bindings.is_empty());
    assert_eq!(visible.bindings.len(), 1);
    assert_eq!(
        visible.bindings[0].profile_status,
        ProfileStatus::Leased as i32
    );
}

#[tokio::test]
async fn lookup_browser_connection_returns_route_hint_for_account() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();

    // Act
    let response = service
        .lookup_browser_connection(Request::new(LookupBrowserConnectionRequest {
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::from([("tier".to_string(), "prod".to_string())]),
            include_unavailable: false,
        }))
        .await
        .unwrap()
        .into_inner();

    // Assert
    let binding = response.binding.unwrap();
    let route = response.route_hint.unwrap();
    assert!(response.available);
    assert!(response.active_lease_id.is_empty());
    assert_eq!(binding.profile_id, "youtube-main");
    assert_eq!(route.machine_id, "machine-a");
    assert_eq!(route.agent_grpc_addr, "http://machine-a.tail.test:7100");
    assert_eq!(route.profile_id, "youtube-main");
    assert!(route.lease_id.is_empty());
    assert!(route.fencing_token.is_empty());
    assert!(response.connection_state.contains("AcquireBrowser"));
}

#[tokio::test]
async fn lookup_browser_connection_reports_active_lease_for_busy_account() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();
    let lease = service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "client-1".to_string(),
            purpose: "upload".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 60,
        }))
        .await
        .unwrap()
        .into_inner()
        .lease
        .unwrap();

    // Act
    let response = service
        .lookup_browser_connection(Request::new(LookupBrowserConnectionRequest {
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            include_unavailable: true,
        }))
        .await
        .unwrap()
        .into_inner();

    // Assert
    assert!(!response.available);
    assert_eq!(response.active_lease_id, lease.lease_id);
    assert_eq!(response.active_lease_expires_at_unix_ms, 61_000);
    let route = response.route_hint.unwrap();
    assert!(route.lease_id.is_empty());
    assert!(route.fencing_token.is_empty());
    assert!(response.connection_state.contains("leased"));
}

#[tokio::test]
async fn lookup_browser_connection_requires_exact_account_lookup() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();

    // Act
    let result = service
        .lookup_browser_connection(Request::new(LookupBrowserConnectionRequest {
            platform: AccountPlatform::Youtube as i32,
            account_id: String::new(),
            label_selector: HashMap::new(),
            include_unavailable: true,
        }))
        .await;

    // Assert
    assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn heartbeat_does_not_make_leased_profile_available() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();
    service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "client-1".to_string(),
            purpose: "upload".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 60,
        }))
        .await
        .unwrap();

    // Act
    service
        .heartbeat(Request::new(HeartbeatRequest {
            machine_id: "machine-a".to_string(),
            status: MachineStatus::Online as i32,
            profiles: vec![profile()],
        }))
        .await
        .unwrap();
    let second = service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "client-2".to_string(),
            purpose: "upload".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 60,
        }))
        .await;

    // Assert
    assert_eq!(second.unwrap_err().code(), tonic::Code::NotFound);
    let visible = service
        .list_browser_account_bindings(Request::new(ListBrowserAccountBindingsRequest {
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            machine_id: String::new(),
            profile_id: String::new(),
            include_unavailable: true,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        visible.bindings[0].profile_status,
        ProfileStatus::Leased as i32
    );
}

#[tokio::test]
async fn expired_lease_cannot_be_renewed_or_routed() {
    // Arrange
    let (service, clock) = test_service_with_clock();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();
    let lease = service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "client-1".to_string(),
            purpose: "upload".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 1,
        }))
        .await
        .unwrap()
        .into_inner()
        .lease
        .unwrap();
    clock.advance_ms(1_001);

    // Act
    let renew = service
        .renew_lease(Request::new(RenewLeaseRequest {
            lease_id: lease.lease_id.clone(),
            fencing_token: lease.fencing_token.clone(),
            ttl_seconds: 60,
        }))
        .await;
    let route = service
        .get_route(Request::new(GetRouteRequest {
            lease_id: lease.lease_id,
            fencing_token: lease.fencing_token,
        }))
        .await;

    // Assert
    assert_eq!(renew.unwrap_err().code(), tonic::Code::NotFound);
    assert_eq!(route.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn register_machine_rejects_profile_owned_by_another_machine() {
    // Arrange
    let service = test_service();
    let mut mismatched = profile();
    mismatched.machine_id = "machine-b".to_string();

    // Act
    let result = service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![mismatched],
        }))
        .await;

    // Assert
    assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    let profiles = service
        .list_profiles(Request::new(ListProfilesRequest {
            platform: AccountPlatform::Unspecified as i32,
            account_id: String::new(),
            label_selector: HashMap::new(),
            include_unavailable: true,
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(profiles.profiles.is_empty());
}

#[tokio::test]
async fn acquire_browser_returns_route_and_exclusive_lease() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();

    // Act
    let response = service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "client-1".to_string(),
            purpose: "upload".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::from([("tier".to_string(), "prod".to_string())]),
            ttl_seconds: 60,
        }))
        .await
        .unwrap()
        .into_inner();

    // Assert
    let lease = response.lease.unwrap();
    let route = response.route.unwrap();
    assert_eq!(lease.lease_id, "lease_1");
    assert_eq!(lease.fencing_token, "fence_1");
    assert_eq!(lease.profile_id, "youtube-main");
    assert_eq!(route.agent_grpc_addr, "http://machine-a.tail.test:7100");
    assert_eq!(route.lease_id, lease.lease_id);

    let second = service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "client-2".to_string(),
            purpose: "upload".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 60,
        }))
        .await;
    assert_eq!(second.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn release_lease_makes_profile_available_again() {
    // Arrange
    let service = test_service();
    service
        .register_machine(Request::new(RegisterMachineRequest {
            machine: Some(machine()),
            profiles: vec![profile()],
        }))
        .await
        .unwrap();
    let lease = service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "client-1".to_string(),
            purpose: "upload".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 60,
        }))
        .await
        .unwrap()
        .into_inner()
        .lease
        .unwrap();

    // Act
    service
        .release_lease(Request::new(ReleaseLeaseRequest {
            lease_id: lease.lease_id,
            fencing_token: lease.fencing_token,
        }))
        .await
        .unwrap();
    let response = service
        .acquire_browser(Request::new(AcquireBrowserRequest {
            client_id: "client-2".to_string(),
            purpose: "upload".to_string(),
            platform: AccountPlatform::Youtube as i32,
            account_id: "yt-1".to_string(),
            label_selector: HashMap::new(),
            ttl_seconds: 60,
        }))
        .await
        .unwrap()
        .into_inner();

    // Assert
    assert_eq!(response.lease.unwrap().lease_id, "lease_2");
}

#[tokio::test]
async fn report_telemetry_aggregates_full_urls_into_domain_bucket() {
    // Arrange
    let service = test_service();

    // Act
    service
        .report_telemetry(Request::new(ReportTelemetryRequest {
            reporter_machine_id: "machine-a".to_string(),
            samples: vec![
                MetricSample {
                    name: "web.bytes_received".to_string(),
                    kind: MetricKind::Counter as i32,
                    observed_at_unix_ms: 61_000,
                    machine_id: String::new(),
                    profile_id: "youtube-main".to_string(),
                    platform: AccountPlatform::Youtube as i32,
                    domain: "https://studio.youtube.com/video/a/edit?token=secret".to_string(),
                    action: "navigate".to_string(),
                    status_class: "2xx".to_string(),
                    error_class: String::new(),
                    value: 100.0,
                },
                MetricSample {
                    name: "web.bytes_received".to_string(),
                    kind: MetricKind::Counter as i32,
                    observed_at_unix_ms: 62_000,
                    machine_id: String::new(),
                    profile_id: "youtube-main".to_string(),
                    platform: AccountPlatform::Youtube as i32,
                    domain: "studio.youtube.com/another/path".to_string(),
                    action: "navigate".to_string(),
                    status_class: "2xx".to_string(),
                    error_class: String::new(),
                    value: 50.0,
                },
            ],
            events: vec![],
        }))
        .await
        .unwrap();
    let summary = service
        .get_metric_summary(Request::new(GetMetricSummaryRequest {
            name: "web.bytes_received".to_string(),
            start_unix_ms: 0,
            end_unix_ms: 0,
            machine_id: "machine-a".to_string(),
            profile_id: "youtube-main".to_string(),
            platform: AccountPlatform::Youtube as i32,
            domain: "https://studio.youtube.com/private/path?x=1".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();

    // Assert
    assert_eq!(summary.points.len(), 1);
    let point = &summary.points[0];
    assert_eq!(point.domain, "studio.youtube.com");
    assert_eq!(point.value, 150.0);
    assert!(!point.domain.contains('/'));
    assert!(!point.domain.contains('?'));
}

#[tokio::test]
async fn report_telemetry_redacts_raw_web_event_attributes() {
    // Arrange
    let service = test_service();

    // Act
    service
        .report_telemetry(Request::new(ReportTelemetryRequest {
            reporter_machine_id: "machine-a".to_string(),
            samples: vec![],
            events: vec![ControlPlaneEvent {
                event_id: "event-1".to_string(),
                event_type: "browser.navigation".to_string(),
                severity: EventSeverity::Info as i32,
                observed_at_unix_ms: 10_000,
                machine_id: "machine-a".to_string(),
                profile_id: "youtube-main".to_string(),
                platform: AccountPlatform::Youtube as i32,
                message: "navigation observed".to_string(),
                attributes: HashMap::from([
                    (
                        "url".to_string(),
                        "https://studio.youtube.com/video/a/edit?secret=1".to_string(),
                    ),
                    (
                        "domain".to_string(),
                        "https://studio.youtube.com/private".to_string(),
                    ),
                    ("html".to_string(), "<html>secret</html>".to_string()),
                ]),
            }],
        }))
        .await
        .unwrap();
    let events = service
        .list_control_plane_events(Request::new(ListControlPlaneEventsRequest {
            start_unix_ms: 0,
            end_unix_ms: 0,
            machine_id: String::new(),
            profile_id: String::new(),
            limit: 10,
        }))
        .await
        .unwrap()
        .into_inner();

    // Assert
    assert_eq!(events.events.len(), 1);
    let attributes = &events.events[0].attributes;
    assert!(!attributes.contains_key("url"));
    assert!(!attributes.contains_key("html"));
    assert_eq!(
        attributes.get("domain").map(String::as_str),
        Some("studio.youtube.com")
    );
}

#[tokio::test]
async fn report_artifacts_makes_available_files_visible_in_control_plane() {
    // Arrange
    let service = test_service();

    // Act
    service
        .report_artifacts(Request::new(ReportArtifactsRequest {
            reporter_machine_id: "machine-a".to_string(),
            artifacts: vec![Artifact {
                artifact_id: "artifact-1".to_string(),
                machine_id: String::new(),
                lease_id: "lease-1".to_string(),
                profile_id: "youtube-main".to_string(),
                original_filename: "video.mp4".to_string(),
                stored_filename: "artifact-1-video.mp4".to_string(),
                content_type: "video/mp4".to_string(),
                purpose: "youtube-upload".to_string(),
                size_bytes: 3,
                uploaded_at_unix_ms: 1_000,
                expires_at_unix_ms: 61_000,
                status: ArtifactStatus::Available as i32,
            }],
        }))
        .await
        .unwrap();
    let response = service
        .list_artifacts(Request::new(ListArtifactsRequest {
            machine_id: "machine-a".to_string(),
            profile_id: "youtube-main".to_string(),
            lease_id: String::new(),
            include_expired: false,
        }))
        .await
        .unwrap()
        .into_inner();

    // Assert
    assert_eq!(response.artifacts.len(), 1);
    assert_eq!(response.artifacts[0].machine_id, "machine-a");
    assert_eq!(response.artifacts[0].artifact_id, "artifact-1");
}

#[tokio::test]
async fn list_artifacts_hides_expired_files_by_default() {
    // Arrange
    let service = test_service();
    service
        .report_artifacts(Request::new(ReportArtifactsRequest {
            reporter_machine_id: "machine-a".to_string(),
            artifacts: vec![Artifact {
                artifact_id: "artifact-expired".to_string(),
                machine_id: "machine-a".to_string(),
                lease_id: "lease-1".to_string(),
                profile_id: "youtube-main".to_string(),
                original_filename: "old.mp4".to_string(),
                stored_filename: "artifact-expired-old.mp4".to_string(),
                content_type: "video/mp4".to_string(),
                purpose: "youtube-upload".to_string(),
                size_bytes: 3,
                uploaded_at_unix_ms: 0,
                expires_at_unix_ms: 999,
                status: ArtifactStatus::Expired as i32,
            }],
        }))
        .await
        .unwrap();

    // Act
    let hidden = service
        .list_artifacts(Request::new(ListArtifactsRequest {
            machine_id: String::new(),
            profile_id: String::new(),
            lease_id: String::new(),
            include_expired: false,
        }))
        .await
        .unwrap()
        .into_inner();
    let visible = service
        .list_artifacts(Request::new(ListArtifactsRequest {
            machine_id: String::new(),
            profile_id: String::new(),
            lease_id: String::new(),
            include_expired: true,
        }))
        .await
        .unwrap()
        .into_inner();

    // Assert
    assert!(hidden.artifacts.is_empty());
    assert_eq!(visible.artifacts.len(), 1);
    assert_eq!(visible.artifacts[0].artifact_id, "artifact-expired");
}
