pub(crate) use std::collections::HashMap;
pub(crate) use std::path::Path;
pub(crate) use std::sync::{Arc, Mutex, RwLock};

pub(crate) use bcp_core::id::{IdGenerator, UuidIdGenerator};
pub(crate) use bcp_core::network::{NetworkDirectory, StaticNetworkDirectory};
pub(crate) use bcp_core::time::{Clock, SystemClock};
pub(crate) use bcp_proto::browsercontrol::v1::global_controller_server::GlobalController;
pub(crate) use bcp_proto::browsercontrol::v1::machine_controller_client::MachineControllerClient;
pub(crate) use bcp_proto::browsercontrol::v1::*;
pub(crate) use prost::Message;
pub(crate) use rusqlite::{Connection, params};
pub(crate) use tonic::{Request, Response, Status};

mod persistence;
mod service;
mod views;
pub mod web;
pub(crate) use persistence::*;
pub(crate) use views::*;
#[cfg(test)]
mod tests;

const DEFAULT_LEASE_TTL_SECONDS: i64 = 300;
const DEFAULT_HEARTBEAT_AFTER_SECONDS: i32 = 10;
const METRIC_BUCKET_MS: i64 = 60_000;
const DEFAULT_EVENT_LIMIT: usize = 100;
const DEFAULT_MACHINE_OFFLINE_AFTER_MS: i64 = 60_000;
const DEFAULT_SWEEP_INTERVAL_SECONDS: u64 = 15;
/// Upper bound on a lease TTL so `now + ttl * 1000` can never overflow i64 and a
/// client cannot pin a profile with a near-immortal lease.
const MAX_LEASE_TTL_SECONDS: i64 = 86_400;
/// Bounds on in-memory telemetry so a flooding agent cannot exhaust memory/disk.
const MAX_STORED_EVENTS: usize = 5_000;
const MAX_STORED_METRICS: usize = 5_000;
const MAX_EVENT_LIST_LIMIT: usize = 1_000;
const MAX_EVENT_MESSAGE_LEN: usize = 512;

/// Resolves and clamps a requested lease TTL: non-positive falls back to the
/// default, and the result is bounded to `[1, MAX_LEASE_TTL_SECONDS]`.
fn resolve_ttl_seconds(requested: i64) -> i64 {
    let ttl = if requested > 0 {
        requested
    } else {
        DEFAULT_LEASE_TTL_SECONDS
    };
    ttl.clamp(1, MAX_LEASE_TTL_SECONDS)
}

fn machine_offline_after_ms() -> i64 {
    std::env::var("BCP_MACHINE_OFFLINE_MS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MACHINE_OFFLINE_AFTER_MS)
}

fn sweep_interval_seconds() -> u64 {
    std::env::var("BCP_SWEEP_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SWEEP_INTERVAL_SECONDS)
        .max(1)
}

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

    /// Best-effort revocation of a lease at its owning machine controller, so a
    /// released or expired lease can no longer pass the agent's fencing check
    /// even when no successor re-leases the profile.
    fn notify_agent_uninstall(&self, lease: &BrowserLease) {
        let network = self.network.clone();
        let machine_id = lease.machine_id.clone();
        let context = LeaseContext {
            lease_id: lease.lease_id.clone(),
            profile_id: lease.profile_id.clone(),
            fencing_token: lease.fencing_token.clone(),
            expires_at_unix_ms: lease.expires_at_unix_ms,
        };
        tokio::spawn(async move {
            let Ok(endpoint) = network.endpoint_for_machine(&machine_id).await else {
                return;
            };
            let Ok(Ok(mut client)) = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                MachineControllerClient::connect(endpoint.agent_grpc_addr),
            )
            .await
            else {
                return;
            };
            let _ = client
                .uninstall_lease(UninstallLeaseRequest {
                    lease: Some(context),
                })
                .await;
        });
    }

    /// Spawns the background reliability sweep: expires leases (revoking them at
    /// their agents) and marks machines offline once their heartbeat goes stale.
    pub fn spawn_sweep(&self) {
        let service = self.clone();
        let interval = sweep_interval_seconds();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval));
            loop {
                ticker.tick().await;
                service.sweep_once();
            }
        });
    }

    fn sweep_once(&self) {
        self.sweep_at(self.clock.now_unix_ms(), machine_offline_after_ms());
    }

    /// Runs one reliability pass at a given time with a given offline threshold.
    /// Returns the leases that expired so callers/tests can observe them; each is
    /// also revoked at its agent.
    fn sweep_at(&self, now_unix_ms: i64, offline_after_ms: i64) -> Vec<BrowserLease> {
        let revoked = {
            let mut state = self
                .state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut next_state = state.clone();

            // Mark machines offline once their registration heartbeat goes stale.
            for machine in next_state.machines.values_mut() {
                if machine.status == MachineStatus::Online as i32
                    && machine.last_heartbeat_unix_ms > 0
                    && now_unix_ms - machine.last_heartbeat_unix_ms > offline_after_ms
                {
                    machine.status = MachineStatus::Offline as i32;
                }
            }

            // Reclaim leases that expired or whose machine went offline: neither
            // can serve work, so free the profile and revoke the lease.
            let offline_machines: std::collections::HashSet<String> = next_state
                .machines
                .values()
                .filter(|machine| machine.status == MachineStatus::Offline as i32)
                .map(|machine| machine.machine_id.clone())
                .collect();
            let revoked: Vec<BrowserLease> = next_state
                .leases
                .values()
                .filter(|lease| {
                    lease.expires_at_unix_ms <= now_unix_ms
                        || offline_machines.contains(&lease.machine_id)
                })
                .cloned()
                .collect();
            for lease in &revoked {
                next_state.leases.remove(&lease.lease_id);
                if let Some(profile) = next_state.profiles.get_mut(&lease.profile_id)
                    && profile.status == ProfileStatus::Leased as i32
                {
                    profile.status = ProfileStatus::Available as i32;
                }
            }

            // Only commit (and revoke at agents) if the new state was durably
            // persisted; otherwise skip this tick so in-memory and disk cannot
            // diverge and resurrect reclaimed leases on restart.
            if self.persist_state(&next_state).is_ok() {
                *state = next_state;
                revoked
            } else {
                Vec::new()
            }
        };
        for lease in &revoked {
            self.notify_agent_uninstall(lease);
        }
        revoked
    }

    pub fn web_snapshot(&self) -> web::ControllerWebSnapshot {
        let now = self.clock.now_unix_ms();
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        // Operator/controller-owned statuses must survive an agent heartbeat: an
        // agent re-registers reporting `Available`, but a Quarantined, Broken, or
        // Launching profile keeps that status until the controller clears it. The
        // active-lease override only applies when the status is NOT operator-owned.
        let existing_status = state
            .profiles
            .get(&profile.profile_id)
            .map(|existing| existing.status);
        let operator_owned = matches!(
            existing_status.and_then(|status| ProfileStatus::try_from(status).ok()),
            Some(ProfileStatus::Quarantined | ProfileStatus::Broken | ProfileStatus::Launching)
        );
        if operator_owned {
            if let Some(status) = existing_status {
                profile.status = status;
            }
        } else if Self::active_lease_for_profile(state, &profile.profile_id).is_some() {
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
    // Bound the message so a misbehaving agent cannot leak large payloads or
    // bloat storage through the free-form message field. Truncate on a UTF-8
    // char boundary so this can never panic.
    if event.message.len() > MAX_EVENT_MESSAGE_LEN {
        let mut end = MAX_EVENT_MESSAGE_LEN;
        while end > 0 && !event.message.is_char_boundary(end) {
            end -= 1;
        }
        event.message.truncate(end);
    }
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
