use super::*;

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

        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let mut state = self
                .state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut next_state = state.clone();
            Self::mark_expired_leases(&mut next_state, now);
            let profile = Self::select_profile_for_acquire(&next_state, &request)
                .ok_or_else(|| Status::not_found("no available profile matched request"))?;
            let lease_id = self.ids.next_id("lease");
            let fencing_token = self.ids.next_id("fence");
            let ttl_seconds = resolve_ttl_seconds(request.ttl_seconds);
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
        let ttl_seconds = resolve_ttl_seconds(request.ttl_seconds);
        let now = self.clock.now_unix_ms();
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next_state = state.clone();
        Self::mark_expired_leases(&mut next_state, now);
        let lease = next_state
            .leases
            .get(&request.lease_id)
            .ok_or_else(|| Status::not_found("lease not found"))?;
        if lease.fencing_token != request.fencing_token {
            return Err(Status::permission_denied("invalid fencing token"));
        }
        let released_lease = lease.clone();
        let profile_id = lease.profile_id.clone();
        next_state.leases.remove(&request.lease_id);
        if let Some(profile) = next_state.profiles.get_mut(&profile_id) {
            profile.status = ProfileStatus::Available as i32;
        }
        self.persist_state(&next_state)?;
        *state = next_state;
        drop(state);
        self.notify_agent_uninstall(&released_lease);
        Ok(Response::new(ReleaseLeaseResponse { released: true }))
    }

    async fn quarantine_profile(
        &self,
        request: Request<QuarantineProfileRequest>,
    ) -> Result<Response<QuarantineProfileResponse>, Status> {
        let request = request.into_inner();
        if request.profile_id.is_empty() {
            return Err(Status::invalid_argument("profile_id is required"));
        }
        let now = self.clock.now_unix_ms();
        let (profile, evicted) = {
            let mut state = self
                .state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut next_state = state.clone();
            Self::mark_expired_leases(&mut next_state, now);
            let profile = {
                let profile = next_state
                    .profiles
                    .get_mut(&request.profile_id)
                    .ok_or_else(|| Status::not_found("profile not found"))?;
                profile.status = ProfileStatus::Quarantined as i32;
                profile.clone()
            };
            // A quarantined profile must not keep serving: evict any active lease.
            let evicted: Vec<BrowserLease> = next_state
                .leases
                .values()
                .filter(|lease| lease.profile_id == request.profile_id)
                .cloned()
                .collect();
            for lease in &evicted {
                next_state.leases.remove(&lease.lease_id);
            }
            self.persist_state(&next_state)?;
            *state = next_state;
            (profile, evicted)
        };
        for lease in &evicted {
            self.notify_agent_uninstall(lease);
        }
        Ok(Response::new(QuarantineProfileResponse {
            profile: Some(profile),
        }))
    }

    async fn release_quarantine(
        &self,
        request: Request<ReleaseQuarantineRequest>,
    ) -> Result<Response<ReleaseQuarantineResponse>, Status> {
        let request = request.into_inner();
        if request.profile_id.is_empty() {
            return Err(Status::invalid_argument("profile_id is required"));
        }
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next_state = state.clone();
        let profile = {
            let profile = next_state
                .profiles
                .get_mut(&request.profile_id)
                .ok_or_else(|| Status::not_found("profile not found"))?;
            if profile.status == ProfileStatus::Quarantined as i32 {
                profile.status = ProfileStatus::Available as i32;
            }
            profile.clone()
        };
        self.persist_state(&next_state)?;
        *state = next_state;
        Ok(Response::new(ReleaseQuarantineResponse {
            profile: Some(profile),
        }))
    }

    async fn list_machine_leases(
        &self,
        request: Request<ListMachineLeasesRequest>,
    ) -> Result<Response<ListMachineLeasesResponse>, Status> {
        let request = request.into_inner();
        if request.machine_id.is_empty() {
            return Err(Status::invalid_argument("machine_id is required"));
        }
        let now = self.clock.now_unix_ms();
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let leases = state
            .leases
            .values()
            .filter(|lease| {
                lease.machine_id == request.machine_id && lease.expires_at_unix_ms > now
            })
            .cloned()
            .collect();
        Ok(Response::new(ListMachineLeasesResponse { leases }))
    }

    async fn get_route(
        &self,
        request: Request<GetRouteRequest>,
    ) -> Result<Response<GetRouteResponse>, Status> {
        let request = request.into_inner();
        let lease = {
            let now = self.clock.now_unix_ms();
            let mut state = self
                .state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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

        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

        // Bound the metric map: evict the oldest buckets once past the cap so a
        // flooding/high-cardinality reporter cannot grow it without limit.
        if next_state.metrics.len() > MAX_STORED_METRICS {
            let mut buckets: Vec<(i64, MetricBucketKey)> = next_state
                .metrics
                .keys()
                .map(|key| (key.bucket_start_unix_ms, key.clone()))
                .collect();
            buckets.sort_by_key(|(start, _)| *start);
            let excess = next_state.metrics.len() - MAX_STORED_METRICS;
            for (_, key) in buckets.into_iter().take(excess) {
                next_state.metrics.remove(&key);
            }
        }

        let accepted_events = request.events.len() as i32;
        next_state
            .events
            .extend(request.events.into_iter().map(redact_event));
        // Bound the stored event log to the newest events.
        if next_state.events.len() > MAX_STORED_EVENTS {
            let excess = next_state.events.len() - MAX_STORED_EVENTS;
            next_state.events.drain(0..excess);
        }
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
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            (request.limit as usize).min(MAX_EVENT_LIST_LIMIT)
        } else {
            DEFAULT_EVENT_LIMIT
        };
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
