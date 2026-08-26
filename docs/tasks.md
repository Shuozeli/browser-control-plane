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
- [x] Harden against an adversarial review: agent-side lease expiry
      (`validate_lease` rejects a lease past its deadline so fencing no longer
      depends on the revocation RPC landing), TTL clamp + checked math, sweep
      persistence gating, bounded telemetry (events/metrics caps + retry), lock
      poison recovery, and connect timeouts on controller<->agent dials.
- [x] Agent lease recovery on restart. Agents pull-reconcile their active leases
      from the controller (`ListMachineLeases`) on startup and every few seconds:
      a restarted agent recovers its install map, and released/expired leases the
      controller no longer holds are pruned locally (self-healing revocation that
      no longer depends on the best-effort uninstall RPC).
- [ ] Add retry policy for controller-to-agent sync (partly addressed: dials now
      time out; still no durable retry/queue for revocation).
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

## Phase 7: Full CDP Operation Coverage

The semantic gateway currently exposes only `goto / click / fill / type / press /
eval / wait_ms` plus a limited `snapshot`. That is a thin slice of CDP. This
phase closes the gap so a lease holder can drive the remote browser as fully as a
local one. Most items are wiring existing `pwright-bridge` methods
(`screenshot`, `pdf`, `get_cookies` / `set_cookies`, `select_option`, `hover`,
`scroll_page`, `wait_for`, `reload` / `go_back` / `go_forward`, `list_tabs` /
`create_tab`) onto RPCs and script steps; a few need lower-level CDP sessions.

Priority order (fill one by one, build + fleet-verify each):

- [x] 7.1 Screenshot + PDF. Added `CaptureScreenshot` / `PrintPdf` machine RPCs
      (base64 payload), gateway trait methods, agent handlers with `browser.*`
      audit events, `screenshot` / `pdf` script steps, and `bcp screenshot` /
      `bcp pdf` client verbs (decode base64 -> file).
- [x] 7.2 Expanded input actions. Added `hover`, `dblclick`, `select`
      (dropdown), `scroll` to `ExecuteAction` + `RunScript` steps.
- [x] 7.3 Real waits. Added `wait_for_selector` (bridge `wait_for` +
      `WaitState::Visible`), replacing blind `wait_ms` for correctness-sensitive
      flows.
- [x] 7.4 Navigation verbs. Added `reload`, `back`, `forward` actions/steps.
- [x] 7.5 Cookies. Added `GetCookies` / `SetCookies` RPCs + gateway methods.
      NOTE: the pinned `pwright-bridge` exposes no raw CDP session, so the
      semantic path reads/writes `document.cookie` (name/value only). httpOnly
      cookies are served full-fidelity by the raw proxy (7.12,
      `Network.getCookies`/`setCookies`).
- [x] 7.6 Page introspection. Added `GetPage` RPC returning `url` / `title` /
      full HTML `content`. (a11y-tree upgrade of `snapshot` left as a follow-up;
      the raw proxy exposes `Accessibility.getFullAXTree` meanwhile.)
- [x] 7.7 Emulation. Delivered via the raw CDP proxy (7.12) — `Emulation.*`
      (UA / geolocation / timezone / locale / device-metrics) is not in the
      pinned bridge's typed surface, so it rides the passthrough full-fidelity.
- [x] 7.8 File upload wired to the browser. Added `SetInputFiles` RPC
      (`Page::set_input_files`) attaching machine-local artifact paths to a file
      `<input>` — bridges the out-of-band upload into an in-page action.
- [x] 7.9 File download retrieval. Added `DownloadArtifact` streaming RPC
      (metadata + chunked bytes) to pull a machine-local file back off-band;
      `Browser.setDownloadBehavior` itself rides the raw proxy.
- [x] 7.10 Multi-tab / target. Delivered via the raw proxy — the CDP `Target`
      domain (create/attach/close targets) passes through untouched.
- [x] 7.11 Dialog handling. Delivered via the raw proxy —
      `Page.handleJavaScriptDialog` passes through.
- [x] 7.12 Raw CDP passthrough. Lease-gated transparent proxy in the agent
      (`proxy.rs`, sibling port): HTTP `/{profile}/json*` fetched from local
      Chrome with `webSocketDebuggerUrl` rewritten back to the proxy (carrying
      the lease token), and WebSocket `/{profile}/devtools/*` relayed
      frame-for-frame. External DevTools clients (pwright / puppeteer /
      chrome-devtools-mcp) get the full ~700-command surface. Out-of-band file
      transfer stays on 7.8 / 7.9. See `docs/cdp-proxy-design.md`.

## Phase 8: Raw CDP proxy robustness (browserless-informed)

Comparison against browserless's CDP ws proxy (thirdparty/browserless) surfaced
three gaps in `bcp-agent/src/proxy.rs`. Unlike browserless (fresh browser per
connection), BCP multiplexes many sequential client sessions onto one persistent
logged-in Chrome, so clean teardown matters *more* for us.

- [x] 8.1 Clean ws teardown. `bridge()` now, on any end (Close / read error /
      stream end), sends a `Close` frame and `close()`s the sink toward each side,
      so a shared long-lived Chrome is never left half-open (which reset the next
      session). Mirrors browserless `this.close()` / `finish(err)`.
- [x] 8.2 Log relay errors. Replaced `while let Some(Ok(..))` with an explicit
      match that logs which side's ws read/forward failed (debug level), so resets
      are diagnosable instead of silently swallowed.
- [x] 8.3 Proxy `/json/new`. Added lease-gated `/{profile}/json/new` (GET+PUT)
      that forwards to Chrome as PUT and rewrites the returned
      `webSocketDebuggerUrl`; `split_lease_from_query` strips a `bcpLease=` token
      from the query (or the lease comes via `Authorization`) so the remaining
      query is the untouched target URL.

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
