# Tasks

## Phase 0: Design Skeleton

- [x] Create Rust workspace.
- [x] Define initial global controller and machine controller protobuf API.
- [x] Add minimal controller, agent, and client binaries.
- [x] Document two-layer routing architecture.
- [x] Document single-machine and multi-machine deployment paths.
- [x] Add agent runbook for exploration, verification, and bug reporting.

## Phase 1: Local Machine Controller

- [x] Add local SQLite state.
- [x] Add profile config file format.
- [x] Implement config-driven local profile discovery.
- [x] Implement CDP port allocation.
- [x] Implement config-driven browser process launch and stop.
- [x] Implement browser health check and CDP readiness probe.
- [x] Add lease cache and local fencing validation. Installing a lease enforces
      one active lease per profile and revokes the prior one; the controller also
      pushes an `UninstallLease` to the agent on release and on expiry (via the
      sweep), so a released or superseded lease can never pass `validate_lease`.

## Phase 2: Global Controller

- [x] Add SQLite controller storage.
- [x] Implement machine registration.
- [x] Implement heartbeat ingestion.
- [x] Implement profile/account registry updates.
- [x] Implement profile selection.
- [x] Implement lease grant, renewal, expiration, and release.
- [x] Implement route lookup.
- [x] Add agent auto-registration.
- [ ] Add richer controller heartbeat reporting.

## Phase 3: Browser Proxy

- [ ] Integrate local browser operations with `pwright-bridge`.
- [x] Implement snapshot proxy.
- [x] Implement action proxy.
- [x] Implement evaluate proxy.
- [x] Implement script proxy. The real gateway now executes a YAML step program
      (goto / click / fill / type / press / eval / wait_ms) against the page and
      streams a JSON result per step, with `${param}` substitution.
- [x] Add structured audit events for all browser operations. Snapshot, action,
      eval, and run_script emit a `browser.*` `ControlPlaneEvent` that the agent
      reports to the controller, queryable via `ListControlPlaneEvents`.

## Phase 4: Client

- [x] Implement `acquire`.
- [x] Implement `release`.
- [x] Implement `route`.
- [x] Implement browser command helpers that use machine-controller routes
      (`snapshot`, `eval`, `run-script`: acquire -> install -> drive -> release).
- [x] Add JSON output for automation (`run-script` streams per-step JSON).

## Phase 5: Reliability

- [x] Add stale machine detection. A background sweep marks a machine offline
      once its registration heartbeat is older than `BCP_MACHINE_OFFLINE_MS`.
- [x] Add stale lease cleanup. The sweep reclaims expired leases and leases whose
      machine went offline, freeing the profile and revoking the lease at the agent.
- [x] Add profile quarantine. `QuarantineProfile` / `ReleaseQuarantine` mark a
      profile quarantined (evicting any active lease) so it is excluded from
      acquire until released.
- [ ] Add retry policy for controller-to-agent sync.
- [ ] Add integration tests with fake machine controllers.
- [x] Turn Docker multi-network topology skeleton into a runnable e2e suite.
- [x] Add a real VirtualBox VM fleet test (`tests/vm-fleet`) plus `bcp-e2e`
      `vm-fleet` and `scenarios` (exclusivity / fencing / fencing-release /
      auto-offline / quarantine / audit / failover / persistence) modes driven
      by `BCP_FLEET`.

## Phase 5.5: Observability

- [x] Add telemetry proto API.
- [x] Add in-memory metric aggregation buckets.
- [x] Add machine/browser/profile/account health samples.
- [x] Add domain-level web access aggregation.
- [x] Add recent structured control-plane events.
- [x] Add tests proving full URLs and page content are not retained by default.
- [ ] Add OTLP exporter for controller and machine-controller metrics.
- [ ] Add dashboard JSON/templates for OpenTelemetry-compatible tooling.

## Phase 5.7: Artifact Store

- [x] Add machine-controller upload artifact API.
- [x] Add dedicated artifact directory config.
- [x] Add SQLite artifact metadata table.
- [x] Require TTL on all uploads.
- [x] Add cleanup scanner for expired artifacts.
- [x] Add fleet artifact metadata listing in global controller.
- [ ] Add artifact metrics and structured events.

## Phase 6: HTML Console

- [x] Add read-only HTML console design review.
- [x] Add controller HTTP listener.
- [x] Bind HTTP listener to Tailscale IP by default.
- [ ] Advertise full Tailscale MagicDNS URL in logs and UI.
- [x] Serve static HTML and assets.
- [x] Add JSON view endpoints for machines, profiles, accounts, leases, and events.
- [ ] Add route dry-run view.
- [ ] Add lease release/renew operations with audit events.
- [ ] Add profile quarantine operations with audit events.
- [ ] Add authentication and role checks.
