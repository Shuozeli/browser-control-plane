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
- [~] Add lease cache and local fencing validation. Installing a lease now
      enforces one active lease per profile and revokes the prior one, so a
      released/superseded lease can no longer pass `validate_lease`. Residual:
      a released lease with no successor is not yet invalidated (needs the
      controller to push an uninstall on release/expiry).

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
- [x] Implement script proxy.
- [ ] Add structured audit events for all browser operations.

## Phase 4: Client

- [ ] Implement `acquire`.
- [ ] Implement `release`.
- [ ] Implement `route`.
- [ ] Implement browser command helpers that use machine-controller routes.
- [ ] Add JSON output for automation.

## Phase 5: Reliability

- [ ] Add stale machine detection. (Gap confirmed by the `failover` scenario:
      a downed machine stays `online` until it is used.)
- [ ] Add stale lease cleanup.
- [ ] Add profile quarantine.
- [ ] Add retry policy for controller-to-agent sync.
- [ ] Add integration tests with fake machine controllers.
- [x] Turn Docker multi-network topology skeleton into a runnable e2e suite.
- [x] Add a real VirtualBox VM fleet test (`tests/vm-fleet`) plus `bcp-e2e`
      `vm-fleet` and `scenarios` (exclusivity / fencing / failover /
      persistence) modes driven by `BCP_FLEET`.

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
