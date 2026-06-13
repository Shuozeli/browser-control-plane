use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use bcp_core::id::{IdGenerator, UuidIdGenerator};
use bcp_core::network::{NetworkDirectory, StaticNetworkDirectory};
use bcp_core::time::{Clock, SystemClock};
use bcp_proto::browsercontrol::v1::global_controller_server::GlobalController;
use bcp_proto::browsercontrol::v1::*;
use prost::Message;
use rusqlite::{Connection, params};
use tonic::{Request, Response, Status};

pub mod web;

const DEFAULT_LEASE_TTL_SECONDS: i64 = 300;
const DEFAULT_HEARTBEAT_AFTER_SECONDS: i32 = 10;
const METRIC_BUCKET_MS: i64 = 60_000;
const DEFAULT_EVENT_LIMIT: usize = 100;

#[derive(Clone)]
pub struct ControllerService {
    state: Arc<RwLock<ControllerState>>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    network: Arc<StaticNetworkDirectory>,
    store: Option<Arc<Mutex<Connection>>>,
}

#[derive(Debug, Clone, Default)]
struct ControllerState {
    machines: HashMap<String, Machine>,
    profiles: HashMap<String, BrowserProfile>,
    account_bindings: HashMap<AccountBindingKey, BrowserAccountBinding>,
    leases: HashMap<String, BrowserLease>,
    metrics: HashMap<MetricBucketKey, MetricPoint>,
    events: Vec<ControlPlaneEvent>,
    artifacts: HashMap<String, Artifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AccountBindingKey {
    platform: i32,
    account_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MetricBucketKey {
    name: String,
    bucket_start_unix_ms: i64,
    machine_id: String,
    profile_id: String,
    platform: i32,
    domain: String,
    action: String,
    status_class: String,
    error_class: String,
}

impl Default for ControllerService {
    fn default() -> Self {
        Self::new(
            Arc::new(SystemClock),
            Arc::new(UuidIdGenerator),
            Arc::new(StaticNetworkDirectory::default()),
        )
    }
}

impl ControllerService {
    pub fn new(
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        network: Arc<StaticNetworkDirectory>,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(ControllerState::default())),
            clock,
            ids,
            network,
            store: None,
        }
    }

    pub fn new_with_sqlite(
        path: impl AsRef<Path>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
        network: Arc<StaticNetworkDirectory>,
    ) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        init_store(&conn)?;
        let state = load_state(&conn)?;
        for machine in state.machines.values() {
            network.upsert_machine(machine.clone());
        }
        Ok(Self {
            state: Arc::new(RwLock::new(state)),
            clock,
            ids,
            network,
            store: Some(Arc::new(Mutex::new(conn))),
        })
    }

    pub fn web_snapshot(&self) -> web::ControllerWebSnapshot {
        let now = self.clock.now_unix_ms();
        let mut state = self.state.write().expect("controller state lock poisoned");
        let mut next_state = state.clone();
        let expired = Self::mark_expired_leases(&mut next_state, now);
        if expired && self.persist_state(&next_state).is_ok() {
            *state = next_state.clone();
        }
        drop(state);
        let state = next_state;

        let mut machines: Vec<_> = state.machines.values().cloned().map(machine_view).collect();
        machines.sort_by(|left, right| left.machine_id.cmp(&right.machine_id));

        let mut profiles: Vec<_> = state.profiles.values().cloned().map(profile_view).collect();
        profiles.sort_by(|left, right| {
            left.machine_id
                .cmp(&right.machine_id)
                .then(left.profile_id.cmp(&right.profile_id))
        });

        let mut accounts: Vec<_> = state
            .account_bindings
            .values()
            .filter_map(|binding| Self::live_binding(&state, binding))
            .map(account_binding_view)
            .collect();
        accounts.sort_by(|left, right| {
            left.platform
                .cmp(&right.platform)
                .then(left.account_id.cmp(&right.account_id))
        });

        let mut leases: Vec<_> = state.leases.values().cloned().map(lease_view).collect();
        leases.sort_by_key(|lease| lease.expires_at_unix_ms);

        let mut metrics: Vec<_> = state.metrics.values().cloned().map(metric_view).collect();
        metrics.sort_by_key(|metric| std::cmp::Reverse(metric.bucket_start_unix_ms));

        let mut events: Vec<_> = state.events.iter().cloned().map(event_view).collect();
        events.sort_by_key(|event| std::cmp::Reverse(event.observed_at_unix_ms));
        events.truncate(DEFAULT_EVENT_LIMIT);

        let mut artifacts: Vec<_> = state
            .artifacts
            .values()
            .cloned()
            .map(artifact_view)
            .collect();
        artifacts.sort_by_key(|artifact| artifact.expires_at_unix_ms);

        web::ControllerWebSnapshot {
            generated_at_unix_ms: now,
            counts: web::FleetCounts {
                machines: machines.len(),
                profiles: profiles.len(),
                accounts: accounts.len(),
                active_leases: leases.len(),
                metrics: metrics.len(),
                events: state.events.len(),
                artifacts: artifacts.len(),
            },
            machines,
            profiles,
            accounts,
            leases,
            metrics,
            events,
            artifacts,
        }
    }

    #[allow(clippy::result_large_err)]
    fn persist_state(&self, state: &ControllerState) -> Result<(), Status> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let mut conn = store
            .lock()
            .map_err(|_| Status::internal("controller sqlite lock poisoned"))?;
        persist_state(&mut conn, state).map_err(|error| Status::internal(error.to_string()))
    }

    fn machine_matches(machine: &Machine, selector: &HashMap<String, String>) -> bool {
        selector
            .iter()
            .all(|(key, value)| machine.labels.get(key) == Some(value))
    }

    fn profile_matches(
        profile: &BrowserProfile,
        platform: i32,
        account_id: &str,
        selector: &HashMap<String, String>,
        include_unavailable: bool,
    ) -> bool {
        if !include_unavailable && profile.status != ProfileStatus::Available as i32 {
            return false;
        }
        if !selector
            .iter()
            .all(|(key, value)| profile.labels.get(key) == Some(value))
        {
            return false;
        }
        if platform != AccountPlatform::Unspecified as i32
            && !profile
                .accounts
                .iter()
                .any(|account| account.platform == platform)
        {
            return false;
        }
        if !account_id.is_empty()
            && !profile
                .accounts
                .iter()
                .any(|account| account.account_id == account_id)
        {
            return false;
        }
        true
    }

    fn account_binding_key(platform: i32, account_id: &str) -> Option<AccountBindingKey> {
        if platform == AccountPlatform::Unspecified as i32 || account_id.is_empty() {
            return None;
        }
        Some(AccountBindingKey {
            platform,
            account_id: account_id.to_string(),
        })
    }

    fn binding_id(platform: i32, account_id: &str) -> String {
        format!("{platform}:{account_id}")
    }

    #[allow(clippy::result_large_err)]
    fn upsert_profile_and_bindings(
        state: &mut ControllerState,
        mut profile: BrowserProfile,
        fallback_machine_id: &str,
        now_unix_ms: i64,
    ) -> Result<(), Status> {
        if profile.profile_id.is_empty() {
            return Err(Status::invalid_argument("profile.profile_id is required"));
        }
        if profile.machine_id.is_empty() {
            profile.machine_id = fallback_machine_id.to_string();
        }
        if profile.machine_id != fallback_machine_id {
            return Err(Status::invalid_argument(format!(
                "profile '{}' belongs to machine '{}' but was reported by '{}'",
                profile.profile_id, profile.machine_id, fallback_machine_id
            )));
        }
        if profile.status == ProfileStatus::Unspecified as i32 {
            profile.status = ProfileStatus::Available as i32;
        }
        if Self::active_lease_for_profile(state, &profile.profile_id).is_some() {
            profile.status = ProfileStatus::Leased as i32;
        }
        if profile.last_seen_unix_ms == 0 {
            profile.last_seen_unix_ms = now_unix_ms;
        }

        for account in &profile.accounts {
            let Some(key) = Self::account_binding_key(account.platform, &account.account_id) else {
                continue;
            };
            if let Some(existing) = state.account_bindings.get(&key)
                && existing.profile_id != profile.profile_id
            {
                return Err(Status::already_exists(format!(
                    "account mapping already exists for platform {} account '{}' on profile '{}'",
                    key.platform, key.account_id, existing.profile_id
                )));
            }
        }

        state
            .account_bindings
            .retain(|_, binding| binding.profile_id != profile.profile_id);

        let machine = state.machines.get(&profile.machine_id);
        for account in &profile.accounts {
            let Some(key) = Self::account_binding_key(account.platform, &account.account_id) else {
                continue;
            };
            state.account_bindings.insert(
                key,
                BrowserAccountBinding {
                    binding_id: Self::binding_id(account.platform, &account.account_id),
                    profile_id: profile.profile_id.clone(),
                    machine_id: profile.machine_id.clone(),
                    platform: account.platform,
                    account_id: account.account_id.clone(),
                    handle: account.handle.clone(),
                    account_health: account.health.clone(),
                    profile_status: profile.status,
                    agent_grpc_addr: machine
                        .map(|machine| machine.agent_grpc_addr.clone())
                        .unwrap_or_default(),
                    cdp_url: profile.cdp_url.clone(),
                    last_seen_unix_ms: profile.last_seen_unix_ms,
                },
            );
        }

        state.profiles.insert(profile.profile_id.clone(), profile);
        Ok(())
    }

    fn live_binding(
        state: &ControllerState,
        binding: &BrowserAccountBinding,
    ) -> Option<BrowserAccountBinding> {
        let profile = state.profiles.get(&binding.profile_id)?;
        let machine = state.machines.get(&profile.machine_id);
        let account = profile.accounts.iter().find(|account| {
            account.platform == binding.platform && account.account_id == binding.account_id
        });
        Some(BrowserAccountBinding {
            binding_id: binding.binding_id.clone(),
            profile_id: profile.profile_id.clone(),
            machine_id: profile.machine_id.clone(),
            platform: binding.platform,
            account_id: binding.account_id.clone(),
            handle: account
                .map(|account| account.handle.clone())
                .unwrap_or_else(|| binding.handle.clone()),
            account_health: account
                .map(|account| account.health.clone())
                .unwrap_or_else(|| binding.account_health.clone()),
            profile_status: profile.status,
            agent_grpc_addr: machine
                .map(|machine| machine.agent_grpc_addr.clone())
                .unwrap_or_default(),
            cdp_url: profile.cdp_url.clone(),
            last_seen_unix_ms: profile.last_seen_unix_ms,
        })
    }

    fn binding_matches(
        binding: &BrowserAccountBinding,
        request: &ListBrowserAccountBindingsRequest,
    ) -> bool {
        (request.include_unavailable || binding.profile_status == ProfileStatus::Available as i32)
            && (request.platform == AccountPlatform::Unspecified as i32
                || binding.platform == request.platform)
            && (request.account_id.is_empty() || binding.account_id == request.account_id)
            && (request.machine_id.is_empty() || binding.machine_id == request.machine_id)
            && (request.profile_id.is_empty() || binding.profile_id == request.profile_id)
    }

    fn binding_matches_lookup(
        state: &ControllerState,
        binding: &BrowserAccountBinding,
        request: &LookupBrowserConnectionRequest,
    ) -> bool {
        let Some(profile) = state.profiles.get(&binding.profile_id) else {
            return false;
        };
        Self::profile_matches(
            profile,
            request.platform,
            &request.account_id,
            &request.label_selector,
            request.include_unavailable,
        )
    }

    fn select_binding_for_lookup(
        state: &ControllerState,
        request: &LookupBrowserConnectionRequest,
    ) -> Option<BrowserAccountBinding> {
        let key = Self::account_binding_key(request.platform, &request.account_id)?;
        state
            .account_bindings
            .get(&key)
            .and_then(|binding| Self::live_binding(state, binding))
            .filter(|binding| Self::binding_matches_lookup(state, binding, request))
    }

    fn active_lease_for_profile(state: &ControllerState, profile_id: &str) -> Option<BrowserLease> {
        state
            .leases
            .values()
            .find(|lease| lease.profile_id == profile_id)
            .cloned()
    }

    fn has_active_lease_for_profile(state: &ControllerState, profile_id: &str) -> bool {
        state
            .leases
            .values()
            .any(|lease| lease.profile_id == profile_id)
    }

    fn select_profile_for_acquire(
        state: &ControllerState,
        request: &AcquireBrowserRequest,
    ) -> Option<BrowserProfile> {
        if let Some(key) = Self::account_binding_key(request.platform, &request.account_id) {
            return state
                .account_bindings
                .get(&key)
                .and_then(|binding| state.profiles.get(&binding.profile_id))
                .filter(|profile| {
                    !Self::has_active_lease_for_profile(state, &profile.profile_id)
                        && Self::profile_matches(
                            profile,
                            request.platform,
                            &request.account_id,
                            &request.label_selector,
                            false,
                        )
                })
                .cloned();
        }
        state
            .profiles
            .values()
            .find(|profile| {
                !Self::has_active_lease_for_profile(state, &profile.profile_id)
                    && Self::profile_matches(
                        profile,
                        request.platform,
                        &request.account_id,
                        &request.label_selector,
                        false,
                    )
            })
            .cloned()
    }

    fn mark_expired_leases(state: &mut ControllerState, now_unix_ms: i64) -> bool {
        let expired_profile_ids: Vec<String> = state
            .leases
            .values()
            .filter(|lease| lease.expires_at_unix_ms <= now_unix_ms)
            .map(profile_id_from_lease)
            .collect();
        let changed = !expired_profile_ids.is_empty();

        state
            .leases
            .retain(|_, lease| lease.expires_at_unix_ms > now_unix_ms);

        for profile_id in expired_profile_ids {
            if let Some(profile) = state.profiles.get_mut(&profile_id)
                && profile.status == ProfileStatus::Leased as i32
            {
                profile.status = ProfileStatus::Available as i32;
            }
        }
        changed
    }

    fn metric_key(sample: &MetricSample) -> MetricBucketKey {
        MetricBucketKey {
            name: sample.name.clone(),
            bucket_start_unix_ms: bucket_start(sample.observed_at_unix_ms),
            machine_id: sample.machine_id.clone(),
            profile_id: sample.profile_id.clone(),
            platform: sample.platform,
            domain: sanitize_domain(&sample.domain),
            action: sample.action.clone(),
            status_class: sample.status_class.clone(),
            error_class: sample.error_class.clone(),
        }
    }

    fn point_from_key(key: &MetricBucketKey, value: f64) -> MetricPoint {
        MetricPoint {
            name: key.name.clone(),
            bucket_start_unix_ms: key.bucket_start_unix_ms,
            machine_id: key.machine_id.clone(),
            profile_id: key.profile_id.clone(),
            platform: key.platform,
            domain: key.domain.clone(),
            action: key.action.clone(),
            status_class: key.status_class.clone(),
            error_class: key.error_class.clone(),
            value,
        }
    }
}

#[tonic::async_trait]
impl GlobalController for ControllerService {
    async fn register_machine(
        &self,
        request: Request<RegisterMachineRequest>,
    ) -> Result<Response<RegisterMachineResponse>, Status> {
        let request = request.into_inner();
        let mut machine = request
            .machine
            .ok_or_else(|| Status::invalid_argument("machine is required"))?;
        if machine.machine_id.is_empty() {
            return Err(Status::invalid_argument("machine.machine_id is required"));
        }
        let now = self.clock.now_unix_ms();
        machine.last_heartbeat_unix_ms = now;
        if machine.status == MachineStatus::Unspecified as i32 {
            machine.status = MachineStatus::Online as i32;
        }

        let mut state = self.state.write().expect("controller state lock poisoned");
        let mut next_state = state.clone();
        Self::mark_expired_leases(&mut next_state, now);
        next_state
            .machines
            .insert(machine.machine_id.clone(), machine.clone());
        for profile in request.profiles {
            Self::upsert_profile_and_bindings(&mut next_state, profile, &machine.machine_id, now)?;
        }
        self.persist_state(&next_state)?;
        *state = next_state;
        self.network.upsert_machine(machine.clone());

        Ok(Response::new(RegisterMachineResponse {
            machine_id: machine.machine_id,
            accepted: true,
        }))
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let request = request.into_inner();
        if request.machine_id.is_empty() {
            return Err(Status::invalid_argument("machine_id is required"));
        }

        let mut state = self.state.write().expect("controller state lock poisoned");
        let now = self.clock.now_unix_ms();
        let mut next_state = state.clone();
        Self::mark_expired_leases(&mut next_state, now);
        let machine = {
            let machine = next_state
                .machines
                .get_mut(&request.machine_id)
                .ok_or_else(|| Status::not_found("machine is not registered"))?;
            machine.status = request.status;
            machine.last_heartbeat_unix_ms = now;
            machine.clone()
        };

        for profile in request.profiles {
            Self::upsert_profile_and_bindings(&mut next_state, profile, &request.machine_id, now)?;
        }
        self.persist_state(&next_state)?;
        *state = next_state;
        self.network.upsert_machine(machine);

        Ok(Response::new(HeartbeatResponse {
            accepted: true,
            heartbeat_after_seconds: DEFAULT_HEARTBEAT_AFTER_SECONDS,
        }))
    }

    async fn list_machines(
        &self,
        request: Request<ListMachinesRequest>,
    ) -> Result<Response<ListMachinesResponse>, Status> {
        let request = request.into_inner();
        let state = self.state.read().expect("controller state lock poisoned");
        let machines = state
            .machines
            .values()
            .filter(|machine| Self::machine_matches(machine, &request.label_selector))
            .cloned()
            .collect();
        Ok(Response::new(ListMachinesResponse { machines }))
    }

    async fn list_profiles(
        &self,
        request: Request<ListProfilesRequest>,
    ) -> Result<Response<ListProfilesResponse>, Status> {
        let request = request.into_inner();
        let now = self.clock.now_unix_ms();
        let mut state = self.state.write().expect("controller state lock poisoned");
        let mut next_state = state.clone();
        let expired = Self::mark_expired_leases(&mut next_state, now);
        if expired {
            self.persist_state(&next_state)?;
            *state = next_state.clone();
        }
        let profiles = next_state
            .profiles
            .values()
            .filter(|profile| {
                Self::profile_matches(
                    profile,
                    request.platform,
                    &request.account_id,
                    &request.label_selector,
                    request.include_unavailable,
                )
            })
            .cloned()
            .collect();
        Ok(Response::new(ListProfilesResponse { profiles }))
    }

    async fn list_browser_account_bindings(
        &self,
        request: Request<ListBrowserAccountBindingsRequest>,
    ) -> Result<Response<ListBrowserAccountBindingsResponse>, Status> {
        let request = request.into_inner();
        let now = self.clock.now_unix_ms();
        let mut state = self.state.write().expect("controller state lock poisoned");
        let mut next_state = state.clone();
        let expired = Self::mark_expired_leases(&mut next_state, now);
        if expired {
            self.persist_state(&next_state)?;
            *state = next_state.clone();
        }
        let bindings = next_state
            .account_bindings
            .values()
            .filter_map(|binding| Self::live_binding(&next_state, binding))
            .filter(|binding| Self::binding_matches(binding, &request))
            .collect();
        Ok(Response::new(ListBrowserAccountBindingsResponse {
            bindings,
        }))
    }

    async fn lookup_browser_connection(
        &self,
        request: Request<LookupBrowserConnectionRequest>,
    ) -> Result<Response<LookupBrowserConnectionResponse>, Status> {
        let request = request.into_inner();
        if Self::account_binding_key(request.platform, &request.account_id).is_none() {
            return Err(Status::invalid_argument(
                "lookup requires both platform and account_id",
            ));
        }
        let now = self.clock.now_unix_ms();
        let mut state = self.state.write().expect("controller state lock poisoned");
        let mut next_state = state.clone();
        let expired = Self::mark_expired_leases(&mut next_state, now);
        if expired {
            self.persist_state(&next_state)?;
            *state = next_state.clone();
        }
        let binding = Self::select_binding_for_lookup(&next_state, &request)
            .ok_or_else(|| Status::not_found("no browser account binding matched request"))?;
        let active_lease = Self::active_lease_for_profile(&next_state, &binding.profile_id);
        let available = binding.profile_status == ProfileStatus::Available as i32;
        let route_hint = BrowserRoute {
            machine_id: binding.machine_id.clone(),
            agent_grpc_addr: binding.agent_grpc_addr.clone(),
            profile_id: binding.profile_id.clone(),
            lease_id: String::new(),
            fencing_token: String::new(),
        };
        let connection_state = if available {
            "available: call AcquireBrowser before executing browser work".to_string()
        } else if active_lease.is_some() {
            "leased: browser is currently held by another client".to_string()
        } else {
            "unavailable: browser is not ready for new work".to_string()
        };

        Ok(Response::new(LookupBrowserConnectionResponse {
            binding: Some(binding),
            route_hint: Some(route_hint),
            active_lease_id: active_lease
                .as_ref()
                .map(|lease| lease.lease_id.clone())
                .unwrap_or_default(),
            available,
            connection_state,
            active_lease_expires_at_unix_ms: active_lease
                .as_ref()
                .map(|lease| lease.expires_at_unix_ms)
                .unwrap_or_default(),
        }))
    }

    async fn acquire_browser(
        &self,
        request: Request<AcquireBrowserRequest>,
    ) -> Result<Response<AcquireBrowserResponse>, Status> {
        let request = request.into_inner();
        if request.client_id.is_empty() {
            return Err(Status::invalid_argument("client_id is required"));
        }
        let now = self.clock.now_unix_ms();
        let selected_profile = {
            let mut state = self.state.write().expect("controller state lock poisoned");
            let mut next_state = state.clone();
            Self::mark_expired_leases(&mut next_state, now);
            let profile = Self::select_profile_for_acquire(&next_state, &request)
                .ok_or_else(|| Status::not_found("no available profile matched request"))?;
            let lease_id = self.ids.next_id("lease");
            let fencing_token = self.ids.next_id("fence");
            let ttl_seconds = if request.ttl_seconds > 0 {
                request.ttl_seconds
            } else {
                DEFAULT_LEASE_TTL_SECONDS
            };
            let lease = BrowserLease {
                lease_id: lease_id.clone(),
                profile_id: profile.profile_id.clone(),
                machine_id: profile.machine_id.clone(),
                client_id: request.client_id,
                purpose: request.purpose,
                fencing_token: fencing_token.clone(),
                expires_at_unix_ms: now + ttl_seconds * 1000,
            };
            next_state.leases.insert(lease_id, lease.clone());
            if let Some(stored_profile) = next_state.profiles.get_mut(&profile.profile_id) {
                stored_profile.status = ProfileStatus::Leased as i32;
            }
            self.persist_state(&next_state)?;
            *state = next_state;
            (profile, lease)
        };

        let endpoint = self
            .network
            .endpoint_for_machine(&selected_profile.0.machine_id)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        let route = BrowserRoute {
            machine_id: endpoint.machine_id,
            agent_grpc_addr: endpoint.agent_grpc_addr,
            profile_id: selected_profile.1.profile_id.clone(),
            lease_id: selected_profile.1.lease_id.clone(),
            fencing_token: selected_profile.1.fencing_token.clone(),
        };

        Ok(Response::new(AcquireBrowserResponse {
            lease: Some(selected_profile.1),
            route: Some(route),
        }))
    }

    async fn renew_lease(
        &self,
        request: Request<RenewLeaseRequest>,
    ) -> Result<Response<RenewLeaseResponse>, Status> {
        let request = request.into_inner();
        let ttl_seconds = if request.ttl_seconds > 0 {
            request.ttl_seconds
        } else {
            DEFAULT_LEASE_TTL_SECONDS
        };
        let now = self.clock.now_unix_ms();
        let mut state = self.state.write().expect("controller state lock poisoned");
        let mut next_state = state.clone();
        Self::mark_expired_leases(&mut next_state, now);
        let lease = next_state
            .leases
            .get_mut(&request.lease_id)
            .ok_or_else(|| Status::not_found("lease not found"))?;
        if lease.fencing_token != request.fencing_token {
            return Err(Status::permission_denied("invalid fencing token"));
        }
        lease.expires_at_unix_ms = now + ttl_seconds * 1000;
        let lease = lease.clone();
        self.persist_state(&next_state)?;
        *state = next_state;
        Ok(Response::new(RenewLeaseResponse { lease: Some(lease) }))
    }

    async fn release_lease(
        &self,
        request: Request<ReleaseLeaseRequest>,
    ) -> Result<Response<ReleaseLeaseResponse>, Status> {
        let request = request.into_inner();
        let now = self.clock.now_unix_ms();
        let mut state = self.state.write().expect("controller state lock poisoned");
        let mut next_state = state.clone();
        Self::mark_expired_leases(&mut next_state, now);
        let lease = next_state
            .leases
            .get(&request.lease_id)
            .ok_or_else(|| Status::not_found("lease not found"))?;
        if lease.fencing_token != request.fencing_token {
            return Err(Status::permission_denied("invalid fencing token"));
        }
        let profile_id = lease.profile_id.clone();
        next_state.leases.remove(&request.lease_id);
        if let Some(profile) = next_state.profiles.get_mut(&profile_id) {
            profile.status = ProfileStatus::Available as i32;
        }
        self.persist_state(&next_state)?;
        *state = next_state;
        Ok(Response::new(ReleaseLeaseResponse { released: true }))
    }

    async fn get_route(
        &self,
        request: Request<GetRouteRequest>,
    ) -> Result<Response<GetRouteResponse>, Status> {
        let request = request.into_inner();
        let lease = {
            let now = self.clock.now_unix_ms();
            let mut state = self.state.write().expect("controller state lock poisoned");
            let mut next_state = state.clone();
            let expired = Self::mark_expired_leases(&mut next_state, now);
            let lease = next_state
                .leases
                .get(&request.lease_id)
                .ok_or_else(|| Status::not_found("lease not found"))?;
            if lease.fencing_token != request.fencing_token {
                return Err(Status::permission_denied("invalid fencing token"));
            }
            let lease = lease.clone();
            if expired {
                self.persist_state(&next_state)?;
                *state = next_state;
            }
            lease
        };
        let endpoint = self
            .network
            .endpoint_for_machine(&lease.machine_id)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        Ok(Response::new(GetRouteResponse {
            route: Some(BrowserRoute {
                machine_id: endpoint.machine_id,
                agent_grpc_addr: endpoint.agent_grpc_addr,
                profile_id: lease.profile_id,
                lease_id: lease.lease_id,
                fencing_token: lease.fencing_token,
            }),
        }))
    }

    async fn report_telemetry(
        &self,
        request: Request<ReportTelemetryRequest>,
    ) -> Result<Response<ReportTelemetryResponse>, Status> {
        let request = request.into_inner();
        if request.reporter_machine_id.is_empty() {
            return Err(Status::invalid_argument("reporter_machine_id is required"));
        }

        let mut state = self.state.write().expect("controller state lock poisoned");
        let mut next_state = state.clone();
        let mut accepted_samples = 0;
        for mut sample in request.samples {
            if sample.name.is_empty() {
                continue;
            }
            if sample.machine_id.is_empty() {
                sample.machine_id = request.reporter_machine_id.clone();
            }
            sample.domain = sanitize_domain(&sample.domain);
            let key = Self::metric_key(&sample);
            let point = next_state
                .metrics
                .entry(key.clone())
                .or_insert_with(|| Self::point_from_key(&key, 0.0));
            if sample.kind == MetricKind::Gauge as i32 {
                point.value = sample.value;
            } else {
                point.value += sample.value;
            }
            accepted_samples += 1;
        }

        let accepted_events = request.events.len() as i32;
        next_state
            .events
            .extend(request.events.into_iter().map(redact_event));
        self.persist_state(&next_state)?;
        *state = next_state;
        Ok(Response::new(ReportTelemetryResponse {
            accepted_samples,
            accepted_events,
        }))
    }

    async fn get_metric_summary(
        &self,
        request: Request<GetMetricSummaryRequest>,
    ) -> Result<Response<GetMetricSummaryResponse>, Status> {
        let request = request.into_inner();
        let domain = sanitize_domain(&request.domain);
        let state = self.state.read().expect("controller state lock poisoned");
        let points = state
            .metrics
            .values()
            .filter(|point| {
                (request.name.is_empty() || point.name == request.name)
                    && (request.start_unix_ms <= 0
                        || point.bucket_start_unix_ms >= bucket_start(request.start_unix_ms))
                    && (request.end_unix_ms <= 0
                        || point.bucket_start_unix_ms <= bucket_start(request.end_unix_ms))
                    && (request.machine_id.is_empty() || point.machine_id == request.machine_id)
                    && (request.profile_id.is_empty() || point.profile_id == request.profile_id)
                    && (request.platform == AccountPlatform::Unspecified as i32
                        || point.platform == request.platform)
                    && (domain.is_empty() || point.domain == domain)
            })
            .cloned()
            .collect();
        Ok(Response::new(GetMetricSummaryResponse { points }))
    }

    async fn list_control_plane_events(
        &self,
        request: Request<ListControlPlaneEventsRequest>,
    ) -> Result<Response<ListControlPlaneEventsResponse>, Status> {
        let request = request.into_inner();
        let limit = if request.limit > 0 {
            request.limit as usize
        } else {
            DEFAULT_EVENT_LIMIT
        };
        let state = self.state.read().expect("controller state lock poisoned");
        let mut events: Vec<ControlPlaneEvent> = state
            .events
            .iter()
            .filter(|event| {
                (request.start_unix_ms <= 0 || event.observed_at_unix_ms >= request.start_unix_ms)
                    && (request.end_unix_ms <= 0
                        || event.observed_at_unix_ms <= request.end_unix_ms)
                    && (request.machine_id.is_empty() || event.machine_id == request.machine_id)
                    && (request.profile_id.is_empty() || event.profile_id == request.profile_id)
            })
            .cloned()
            .collect();
        events.sort_by_key(|event| std::cmp::Reverse(event.observed_at_unix_ms));
        events.truncate(limit);
        Ok(Response::new(ListControlPlaneEventsResponse { events }))
    }

    async fn report_artifacts(
        &self,
        request: Request<ReportArtifactsRequest>,
    ) -> Result<Response<ReportArtifactsResponse>, Status> {
        let request = request.into_inner();
        if request.reporter_machine_id.is_empty() {
            return Err(Status::invalid_argument("reporter_machine_id is required"));
        }
        let mut state = self.state.write().expect("controller state lock poisoned");
        let mut next_state = state.clone();
        let mut accepted_artifacts = 0;
        for mut artifact in request.artifacts {
            if artifact.artifact_id.is_empty() {
                continue;
            }
            if artifact.machine_id.is_empty() {
                artifact.machine_id = request.reporter_machine_id.clone();
            }
            next_state
                .artifacts
                .insert(artifact.artifact_id.clone(), artifact);
            accepted_artifacts += 1;
        }
        self.persist_state(&next_state)?;
        *state = next_state;
        Ok(Response::new(ReportArtifactsResponse {
            accepted_artifacts,
        }))
    }

    async fn list_artifacts(
        &self,
        request: Request<ListArtifactsRequest>,
    ) -> Result<Response<ListArtifactsResponse>, Status> {
        let request = request.into_inner();
        let now = self.clock.now_unix_ms();
        let state = self.state.read().expect("controller state lock poisoned");
        let mut artifacts: Vec<Artifact> = state
            .artifacts
            .values()
            .filter(|artifact| {
                (request.machine_id.is_empty() || artifact.machine_id == request.machine_id)
                    && (request.profile_id.is_empty() || artifact.profile_id == request.profile_id)
                    && (request.lease_id.is_empty() || artifact.lease_id == request.lease_id)
                    && (request.include_expired
                        || (artifact.expires_at_unix_ms > now
                            && artifact.status == ArtifactStatus::Available as i32))
            })
            .cloned()
            .collect();
        artifacts.sort_by_key(|artifact| std::cmp::Reverse(artifact.uploaded_at_unix_ms));
        Ok(Response::new(ListArtifactsResponse { artifacts }))
    }
}

fn profile_id_from_lease(lease: &BrowserLease) -> String {
    lease.profile_id.clone()
}

fn bucket_start(observed_at_unix_ms: i64) -> i64 {
    if observed_at_unix_ms <= 0 {
        return 0;
    }
    observed_at_unix_ms - (observed_at_unix_ms % METRIC_BUCKET_MS)
}

fn sanitize_domain(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let host_port = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let host = host_port.split('@').next_back().unwrap_or(host_port);
    host.split(':').next().unwrap_or(host).to_ascii_lowercase()
}

fn redact_event(mut event: ControlPlaneEvent) -> ControlPlaneEvent {
    event.attributes.remove("url");
    event.attributes.remove("full_url");
    event.attributes.remove("html");
    event.attributes.remove("request_body");
    event.attributes.remove("response_body");
    if let Some(domain) = event.attributes.get("domain").cloned() {
        event
            .attributes
            .insert("domain".to_string(), sanitize_domain(&domain));
    }
    event
}

fn init_store(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS controller_records (
            kind TEXT NOT NULL,
            id TEXT NOT NULL,
            payload BLOB NOT NULL,
            PRIMARY KEY (kind, id)
        );",
    )?;
    Ok(())
}

fn persist_state(conn: &mut Connection, state: &ControllerState) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    for kind in [
        "machine", "profile", "binding", "lease", "metric", "event", "artifact",
    ] {
        tx.execute("DELETE FROM controller_records WHERE kind = ?1", [kind])?;
    }

    {
        let mut stmt =
            tx.prepare("INSERT INTO controller_records (kind, id, payload) VALUES (?1, ?2, ?3)")?;
        for machine in state.machines.values() {
            insert_message(&mut stmt, "machine", &machine.machine_id, machine)?;
        }
        for profile in state.profiles.values() {
            insert_message(&mut stmt, "profile", &profile.profile_id, profile)?;
        }
        for binding in state.account_bindings.values() {
            insert_message(&mut stmt, "binding", &binding.binding_id, binding)?;
        }
        for lease in state.leases.values() {
            insert_message(&mut stmt, "lease", &lease.lease_id, lease)?;
        }
        for point in state.metrics.values() {
            insert_message(&mut stmt, "metric", &metric_point_id(point), point)?;
        }
        for (index, event) in state.events.iter().enumerate() {
            let id = if event.event_id.is_empty() {
                format!("{}:{index}", event.observed_at_unix_ms)
            } else {
                event.event_id.clone()
            };
            insert_message(&mut stmt, "event", &id, event)?;
        }
        for artifact in state.artifacts.values() {
            insert_message(&mut stmt, "artifact", &artifact.artifact_id, artifact)?;
        }
    }

    tx.commit()?;
    Ok(())
}

fn insert_message<M: Message>(
    stmt: &mut rusqlite::Statement<'_>,
    kind: &str,
    id: &str,
    message: &M,
) -> anyhow::Result<()> {
    stmt.execute(params![kind, id, message.encode_to_vec()])?;
    Ok(())
}

fn load_state(conn: &Connection) -> anyhow::Result<ControllerState> {
    let mut state = ControllerState::default();

    for machine in load_messages::<Machine>(conn, "machine")? {
        state.machines.insert(machine.machine_id.clone(), machine);
    }
    for lease in load_messages::<BrowserLease>(conn, "lease")? {
        state.leases.insert(lease.lease_id.clone(), lease);
    }
    for profile in load_messages::<BrowserProfile>(conn, "profile")? {
        state.profiles.insert(profile.profile_id.clone(), profile);
    }
    for binding in load_messages::<BrowserAccountBinding>(conn, "binding")? {
        if let Some(key) =
            ControllerService::account_binding_key(binding.platform, &binding.account_id)
        {
            state.account_bindings.insert(key, binding);
        }
    }
    for point in load_messages::<MetricPoint>(conn, "metric")? {
        state.metrics.insert(metric_key_from_point(&point), point);
    }
    state.events = load_messages::<ControlPlaneEvent>(conn, "event")?;
    for artifact in load_messages::<Artifact>(conn, "artifact")? {
        state
            .artifacts
            .insert(artifact.artifact_id.clone(), artifact);
    }

    Ok(state)
}

fn load_messages<M: Message + Default>(conn: &Connection, kind: &str) -> anyhow::Result<Vec<M>> {
    let mut stmt =
        conn.prepare("SELECT payload FROM controller_records WHERE kind = ?1 ORDER BY id ASC")?;
    let rows = stmt.query_map([kind], |row| row.get::<_, Vec<u8>>(0))?;
    let mut messages = Vec::new();
    for row in rows {
        let payload = row?;
        messages.push(M::decode(payload.as_slice())?);
    }
    Ok(messages)
}

fn metric_key_from_point(point: &MetricPoint) -> MetricBucketKey {
    MetricBucketKey {
        name: point.name.clone(),
        bucket_start_unix_ms: point.bucket_start_unix_ms,
        machine_id: point.machine_id.clone(),
        profile_id: point.profile_id.clone(),
        platform: point.platform,
        domain: point.domain.clone(),
        action: point.action.clone(),
        status_class: point.status_class.clone(),
        error_class: point.error_class.clone(),
    }
}

fn metric_point_id(point: &MetricPoint) -> String {
    let key = metric_key_from_point(point);
    [
        key.name,
        key.bucket_start_unix_ms.to_string(),
        key.machine_id,
        key.profile_id,
        key.platform.to_string(),
        key.domain,
        key.action,
        key.status_class,
        key.error_class,
    ]
    .join("\u{1f}")
}

fn machine_view(machine: Machine) -> web::MachineView {
    web::MachineView {
        machine_id: machine.machine_id,
        hostname: machine.hostname,
        status: machine_status_label(machine.status),
        agent_grpc_addr: machine.agent_grpc_addr,
        tailscale_host: machine.tailscale_host,
        last_heartbeat_unix_ms: machine.last_heartbeat_unix_ms,
        labels: key_values(machine.labels),
    }
}

fn profile_view(profile: BrowserProfile) -> web::ProfileView {
    web::ProfileView {
        profile_id: profile.profile_id,
        machine_id: profile.machine_id,
        display_name: profile.display_name,
        status: profile_status_label(profile.status),
        cdp_url: profile.cdp_url,
        cdp_port: profile.cdp_port,
        last_seen_unix_ms: profile.last_seen_unix_ms,
        accounts: profile.accounts.into_iter().map(account_view).collect(),
        labels: key_values(profile.labels),
    }
}

fn account_view(account: Account) -> web::AccountView {
    web::AccountView {
        account_id: account.account_id,
        platform: platform_label(account.platform),
        handle: account.handle,
        health: account.health,
        capabilities: account.capabilities,
    }
}

fn account_binding_view(binding: BrowserAccountBinding) -> web::AccountBindingView {
    web::AccountBindingView {
        binding_id: binding.binding_id,
        profile_id: binding.profile_id,
        machine_id: binding.machine_id,
        platform: platform_label(binding.platform),
        account_id: binding.account_id,
        handle: binding.handle,
        account_health: binding.account_health,
        profile_status: profile_status_label(binding.profile_status),
        agent_grpc_addr: binding.agent_grpc_addr,
        cdp_url: binding.cdp_url,
        last_seen_unix_ms: binding.last_seen_unix_ms,
    }
}

fn lease_view(lease: BrowserLease) -> web::LeaseView {
    web::LeaseView {
        lease_id: lease.lease_id,
        profile_id: lease.profile_id,
        machine_id: lease.machine_id,
        client_id: lease.client_id,
        purpose: lease.purpose,
        expires_at_unix_ms: lease.expires_at_unix_ms,
    }
}

fn metric_view(metric: MetricPoint) -> web::MetricView {
    web::MetricView {
        name: metric.name,
        value: metric.value,
        bucket_start_unix_ms: metric.bucket_start_unix_ms,
        machine_id: metric.machine_id,
        profile_id: metric.profile_id,
        platform: platform_label(metric.platform),
        domain: metric.domain,
        action: metric.action,
        status_class: metric.status_class,
        error_class: metric.error_class,
    }
}

fn event_view(event: ControlPlaneEvent) -> web::EventView {
    web::EventView {
        event_type: event.event_type,
        severity: event_severity_label(event.severity),
        observed_at_unix_ms: event.observed_at_unix_ms,
        machine_id: event.machine_id,
        profile_id: event.profile_id,
        message: event.message,
        attributes: key_values(event.attributes),
    }
}

fn artifact_view(artifact: Artifact) -> web::ArtifactView {
    web::ArtifactView {
        artifact_id: artifact.artifact_id,
        machine_id: artifact.machine_id,
        profile_id: artifact.profile_id,
        lease_id: artifact.lease_id,
        original_filename: artifact.original_filename,
        content_type: artifact.content_type,
        purpose: artifact.purpose,
        status: artifact_status_label(artifact.status),
        size_bytes: artifact.size_bytes,
        expires_at_unix_ms: artifact.expires_at_unix_ms,
    }
}

fn key_values(values: HashMap<String, String>) -> Vec<web::KeyValueView> {
    let mut values: Vec<_> = values
        .into_iter()
        .map(|(key, value)| web::KeyValueView { key, value })
        .collect();
    values.sort_by(|left, right| left.key.cmp(&right.key));
    values
}

fn machine_status_label(status: i32) -> String {
    match MachineStatus::try_from(status).unwrap_or(MachineStatus::Unspecified) {
        MachineStatus::Online => "online",
        MachineStatus::Degraded => "degraded",
        MachineStatus::Offline => "offline",
        MachineStatus::Unspecified => "unspecified",
    }
    .to_string()
}

fn profile_status_label(status: i32) -> String {
    match ProfileStatus::try_from(status).unwrap_or(ProfileStatus::Unspecified) {
        ProfileStatus::Available => "available",
        ProfileStatus::Leased => "leased",
        ProfileStatus::Launching => "launching",
        ProfileStatus::Broken => "broken",
        ProfileStatus::Quarantined => "quarantined",
        ProfileStatus::Unspecified => "unspecified",
    }
    .to_string()
}

fn platform_label(platform: i32) -> String {
    match AccountPlatform::try_from(platform).unwrap_or(AccountPlatform::Unspecified) {
        AccountPlatform::Youtube => "youtube",
        AccountPlatform::X => "x",
        AccountPlatform::Douyin => "douyin",
        AccountPlatform::Tiktok => "tiktok",
        AccountPlatform::Reddit => "reddit",
        AccountPlatform::Zhihu => "zhihu",
        AccountPlatform::Weibo => "weibo",
        AccountPlatform::Wsj => "wsj",
        AccountPlatform::HackerNews => "hacker-news",
        AccountPlatform::Unspecified => "unspecified",
    }
    .to_string()
}

fn event_severity_label(severity: i32) -> String {
    match EventSeverity::try_from(severity).unwrap_or(EventSeverity::Unspecified) {
        EventSeverity::Info => "info",
        EventSeverity::Warn => "warn",
        EventSeverity::Error => "error",
        EventSeverity::Unspecified => "unspecified",
    }
    .to_string()
}

fn artifact_status_label(status: i32) -> String {
    match ArtifactStatus::try_from(status).unwrap_or(ArtifactStatus::Unspecified) {
        ArtifactStatus::Uploading => "uploading",
        ArtifactStatus::Available => "available",
        ArtifactStatus::Expired => "expired",
        ArtifactStatus::Deleted => "deleted",
        ArtifactStatus::Failed => "failed",
        ArtifactStatus::Unspecified => "unspecified",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
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
}
