# CDP Coverage & Raw Proxy Design

## Problem

Chrome binds its DevTools (CDP) endpoint to `localhost` only. To drive a browser
on a machine from off-box, the machine controller (agent) has to bridge CDP out
over Tailscale. Historically the agent only exposed a thin *semantic* gateway
(`goto / click / fill / type / press / eval` + a limited snapshot) — a tiny
slice of CDP's ~57 domains / ~700 commands.

Two forces shape the design:

1. **Coverage.** Hand-porting 700 commands into typed RPCs is neither feasible
   nor maintainable. Some domains (`Emulation`, dialog handling) aren't even in
   the pinned `pwright-bridge` typed surface.
2. **Not everything proxies.** CDP is JSON-RPC over a WebSocket, so *almost*
   everything is in-band and forwards transparently. The exceptions touch the
   Chrome machine's local filesystem and are inherently out-of-band:
   - **Upload** — `DOM.setFileInputFiles` takes machine-local *paths*, not bytes.
   - **Download** — files land on the machine's disk.

## Two layers

### 1. Semantic gateway (typed, in-band)

Convenience RPCs for the common, safe operations — the high-level path most
callers want. Current surface (Phase 7):

| Capability | RPC / script step |
| --- | --- |
| Navigate | `goto`, `reload`, `back`, `forward` |
| Input | `click`, `fill`, `type`, `press`, `hover`, `dblclick`, `select`, `scroll` |
| Waits | `wait_for_selector` (real), `wait_ms` |
| JS | `Evaluate` |
| Snapshot | `GetSnapshot` (interactive elements) |
| Capture | `CaptureScreenshot`, `PrintPdf` |
| Page info | `GetPage` (url / title / full HTML) |
| Cookies | `GetCookies` / `SetCookies` (name/value via `document.cookie`) |
| File in | `SetInputFiles` (attach machine-local artifact to `<input type=file>`) |
| File out | `UploadArtifact` (bytes → disk) / `DownloadArtifact` (disk → bytes) |

Every op validates the lease (installed + profile + fencing token + not expired)
and emits a `browser.*` audit event.

### 2. Raw CDP proxy (transparent, full-fidelity)

For everything the semantic layer doesn't hand-port — emulation, dialogs,
fine-grained `Network`/`Fetch` interception, multi-tab `Target` control, httpOnly
cookies, tracing, … — the agent runs a lease-gated transparent proxy
(`crates/bcp-agent/src/proxy.rs`) on a sibling port (gRPC port + 1, or
`BCP_AGENT_PROXY_ADDR`).

```
external DevTools client (pwright / puppeteer / chrome-devtools-mcp)
        │  http GET /{profile}/json/version?bcpLease=<lease_id>:<fencing>
        ▼
   ┌─────────────┐   fetch /json/*      ┌──────────────┐
   │  bcp-agent  │ ───────────────────▶ │ localhost    │
   │  cdp proxy  │ ◀─────────────────── │ Chrome :9222 │
   └─────────────┘  rewrite ws URL      └──────────────┘
        │  ws  /{profile}/devtools/{id}?bcpLease=…  (frame-for-frame relay)
        ▼
```

- **HTTP `/{profile}/json*`** — fetched from the profile's local Chrome; every
  `webSocketDebuggerUrl` is rewritten to
  `ws://<proxy_host>/<profile>/devtools/…?bcpLease=<lease>` so the returned URL
  is dialable off-box and carries the lease.
- **WebSocket `/{profile}/devtools/*`** — lease-checked, then relayed
  frame-for-frame to Chrome's real ws. The DevTools JSON protocol rides it
  untouched → full CDP surface.
- **Lease credential** — accepted from either the `?bcpLease=<lease_id>:<fencing>`
  query (preferred: rewritten ws URLs are then self-contained) **or** an
  `Authorization: Bearer <lease_id>:<fencing>` header, for DevTools clients that
  only do `/json` discovery and inject headers. Whichever source supplied the
  token is embedded into the rewritten ws URL, so the follow-up ws connection
  works even when the `/json` fetch authed via header.
- **Lease gating** — `AgentService::check_lease` applies the same rules as the
  gRPC `validate_lease`; `profile_cdp_url` resolves the profile's local Chrome.
- **Multi-profile** — the `{profile}` path segment routes to that profile's
  Chrome, so one agent proxies many browsers.

## In-band vs out-of-band boundary

| | Mechanism | Notes |
| --- | --- | --- |
| Navigate / input / eval / screenshots / PDF / cookies / emulation / dialogs / network / multi-tab / tracing | **in-band** | semantic RPCs *and/or* raw proxy |
| **File upload** (`DOM.setFileInputFiles`) | **out-of-band** | `UploadArtifact` bytes → machine disk, then `SetInputFiles` references the local path |
| **File download** | **out-of-band** | file lands on machine disk; `DownloadArtifact` streams it back |

The proxy deliberately does **not** carry file transfer — those go through the
artifact RPCs above.

## Constraints / follow-ups

- The pinned `pwright-bridge` (`a74000f2`) exposes no public raw CDP `session()`
  accessor, so the semantic cookie path is `document.cookie`-based (no httpOnly)
  and `Emulation.*` isn't typed. Bumping the pin to a build that re-exports the
  session would let more of these move in-band; until then the raw proxy covers
  them.
- `snapshot` is still a `querySelectorAll` shim, not a real a11y tree
  (`Accessibility.getFullAXTree`); the raw proxy provides the full tree today.
- The proxy authenticates via the lease token (either the `bcpLease` query or an
  `Authorization: Bearer` header). A future hardening could add per-connection
  origin/PSK checks and TLS on the proxy port.
