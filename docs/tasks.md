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
- [ ] Implement CDP port allocation.
- [x] Implement config-driven browser process launch and stop.
- [x] Implement browser health check.
- [ ] Add lease cache and local fencing validation.

## Phase 2: Global Controller

- [x] Add SQLite controller storage.
- [x] Implement machine registration.
- [ ] Implement heartbeat ingestion.
- [ ] Implement profile/account registry updates.
- [ ] Implement profile selection.
- [ ] Implement lease grant, renewal, expiration, and release.
- [ ] Implement route lookup.
- [x] Add agent auto-registration.
- [ ] Add richer controller heartbeat reporting.

## Phase 3: Browser Proxy

- [ ] Integrate local browser operations with `pwright-bridge`.
- [ ] Implement snapshot proxy.
- [ ] Implement action proxy.
- [ ] Implement evaluate proxy.
- [ ] Implement script proxy.
- [ ] Add structured audit events for all browser operations.

## Phase 4: Client

- [ ] Implement `acquire`.
- [ ] Implement `release`.
- [ ] Implement `route`.
- [ ] Implement browser command helpers that use machine-controller routes.
- [ ] Add JSON output for automation.

## Phase 5: Reliability

- [ ] Add stale machine detection.
- [ ] Add stale lease cleanup.
- [ ] Add profile quarantine.
- [ ] Add retry policy for controller-to-agent sync.
- [ ] Add integration tests with fake machine controllers.
- [x] Turn Docker multi-network topology skeleton into a runnable e2e suite.

## Phase 5.5: Observability

- [ ] Add telemetry proto API.
- [ ] Add in-memory metric aggregation buckets.
- [ ] Add machine/browser/profile/account health samples.
- [ ] Add domain-level web access aggregation.
- [ ] Add recent structured control-plane events.
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

- [ ] Add read-only HTML console design review.
- [ ] Add controller HTTP listener.
- [ ] Bind HTTP listener to Tailscale IP by default.
- [ ] Advertise full Tailscale MagicDNS URL in logs and UI.
- [ ] Serve static HTML and assets.
- [ ] Add JSON view endpoints for machines, profiles, accounts, leases, and events.
- [ ] Add route dry-run view.
- [ ] Add lease release/renew operations with audit events.
- [ ] Add profile quarantine operations with audit events.
- [ ] Add authentication and role checks.
