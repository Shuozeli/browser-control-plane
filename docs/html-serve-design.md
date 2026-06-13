# HTML Serve Design

## Purpose

Browser Control Plane needs a lightweight browser-based console for operators to
inspect machines, profiles, accounts, leases, and browser health. This console
is not the primary automation API. It is an operational surface on top of the
global controller and machine controllers.

The first version should be an HTML UI served by the global controller. It
should help answer:

- Which machines are online?
- Which profiles/accounts exist?
- Which Chrome profile is running on which machine?
- Which profile is leased, by whom, and for what purpose?
- Which profiles are broken, quarantined, or need login repair?
- What route would a client receive for a given account/platform request?

## Implementation Status

The first native controller web UI is implemented in `bcp-controller`:

- `GET /`: self-contained read-only HTML dashboard
- `GET /api/snapshot`: fleet snapshot JSON
- `GET /healthz`: HTTP liveness probe
- default bind: `$TAILSCALE_IP:7080`, or `0.0.0.0:7080` when `TAILSCALE_IP` is
  not set
- override: `--web-addr` or `BCP_CONTROLLER_WEB_ADDR`
- disable: `--disable-web` or `BCP_CONTROLLER_DISABLE_WEB=true`

The implementation intentionally omits fencing tokens and write operations.
Route dry-runs, mutation actions, authentication, and public URL advertisement
remain future work.

## Non-Goals

- Do not build browser automation workflows in the UI first.
- Do not expose raw Chrome CDP ports in the UI.
- Do not make this a replacement for the gRPC client API.
- Do not make the first version a complex frontend application.
- Do not start with live browser streaming or remote desktop.

## Placement

The HTML server should live in the global controller process.

```text
Operator Browser
  |
  | HTTP HTML + JSON
  v
Global Controller
  |
  | gRPC registry / route / lease
  v
Machine Controllers
```

Reasons:

- The global controller owns the fleet-wide view.
- Operators should not need to know individual machine controller addresses.
- The machine controller remains a local browser proxy and lifecycle manager.
- The UI can display routes without becoming the browser data path.

Machine controllers may later expose a small local diagnostics page, but that is
separate from the fleet console.

## Server Shape

The global controller should expose both gRPC and HTTP from the same binary.

```text
bcp-controller
├── gRPC API
│   └── GlobalController service
└── HTTP API
    ├── /                 HTML app
    ├── /assets/*         static assets
    ├── /api/*            JSON view API
    └── /healthz          HTTP health check
```

The HTTP surface should be a thin adapter over the same application services
used by gRPC. Business logic should not be duplicated in route handlers.

## UI Model

The initial UI should be operational and dense. It should not be a marketing
page.

Primary views:

1. Fleet
2. Profiles
3. Accounts
4. Leases
5. Routes
6. Events

### Fleet

Shows all machines.

Columns:

- machine ID
- hostname
- Tailscale host
- agent address
- status
- open/running profile count
- leased profile count
- last heartbeat
- labels

Actions:

- inspect machine
- filter by status
- filter by label

### Profiles

Shows browser profiles across all machines.

Columns:

- profile ID
- display name
- machine
- profile path
- status
- CDP port, hidden by default
- accounts
- labels
- last seen

Actions:

- inspect profile
- show current route
- mark quarantined
- clear quarantine

### Accounts

Shows platform accounts mapped to profiles.

Columns:

- account ID
- platform
- handle
- profile
- machine
- account health
- capabilities

Actions:

- filter by platform
- filter by account health
- inspect owning profile
- dry-run route request

### Leases

Shows active and recently expired leases.

Columns:

- lease ID
- profile ID
- machine ID
- client ID
- purpose
- expires at
- age
- fencing token fingerprint

Actions:

- release lease
- extend lease
- inspect route

Lease mutation actions should require explicit confirmation.

### Routes

A route debugger for operators.

Inputs:

- platform
- account ID
- label selector
- purpose
- TTL

Outputs:

- selected profile
- selected machine
- machine controller address
- reason for selection
- reason no route is available

The route debugger should support a dry-run mode that does not create a lease.

### Events

Shows recent control-plane events.

Event types:

- machine registered
- heartbeat accepted
- machine offline
- profile discovered
- profile status changed
- lease acquired
- lease renewed
- lease released
- lease expired
- profile quarantined
- browser health failed

## HTTP API

The HTML UI should call JSON endpoints. These endpoints are controller-local
view APIs, not public replacement APIs for gRPC.

Initial endpoints:

```text
GET  /api/machines
GET  /api/profiles
GET  /api/accounts
GET  /api/leases
GET  /api/events
POST /api/routes:dryRun
POST /api/leases/{lease_id}:release
POST /api/leases/{lease_id}:renew
POST /api/profiles/{profile_id}:quarantine
POST /api/profiles/{profile_id}:clearQuarantine
```

The JSON shapes should reuse protobuf-derived domain models where practical,
but HTTP responses may add view-only fields such as formatted status,
heartbeat age, and route selection reasons.

## Static Asset Strategy

Phase 1 should use server-rendered or embedded static HTML with minimal
JavaScript.

Recommended approach:

- Handwritten HTML shell.
- Handwritten TypeScript or plain JavaScript for table rendering and filters.
- CSS served as a static asset.
- Assets embedded into the binary for simple deployment.

Avoid adding a React/Vite application until the UI needs complex state or rich
interaction. The first console should prioritize operational clarity and a
small deployment footprint.

## Authentication

Phase 1 can assume Tailscale network access, but the design should leave room
for authentication.

Supported later:

- bearer token
- mTLS
- Tailscale identity headers, if behind a Tailscale-aware proxy

All mutation endpoints must be auditable.

## Authorization

Initial roles:

- viewer: read-only access
- operator: can release/renew leases and quarantine profiles
- admin: can change machine/profile registry settings

The first implementation may start with a single operator token, but API
boundaries should not assume everyone is admin.

## Safety Rules

- Never expose raw CDP URLs as primary action targets.
- Hide CDP ports by default; show only in detail views.
- Mutating actions require confirmation.
- Lease release should include current fencing token validation.
- Quarantine changes should create audit events.
- UI actions should call global controller services, not reach into storage
  directly.

## Deployment

The HTTP server should bind consistently with the existing server policy:

- bind to `$TAILSCALE_IP` when present
- otherwise bind to `0.0.0.0`
- surface URLs using the full Tailscale MagicDNS hostname when possible

Suggested defaults:

```text
gRPC:  :7000
HTTP:  :7080
```

The ports are intentionally separate. This keeps gRPC client config and browser
operator access easy to reason about.

## Tailscale Serving

The operator console should be served as a tailnet service by default. The
controller should not advertise `localhost`, raw Chrome CDP ports, or public
internet URLs.

### Binding

At startup, the controller should resolve:

```bash
TAILSCALE_IP=$(tailscale ip -4)
TAILSCALE_HOST=$(tailscale status --json | jq -r '.Self.DNSName' | sed 's/\.$//')
```

Then bind HTTP to:

```text
$TAILSCALE_IP:7080
```

If `$TAILSCALE_IP` is not available, local development may bind to:

```text
0.0.0.0:7080
```

The controller should log and display the MagicDNS URL:

```text
http://$TAILSCALE_HOST:7080
```

The MagicDNS hostname with tailnet suffix is the canonical operator URL because
it survives Tailscale IP rotation and works across tailnet devices.

### Configuration

Suggested environment variables:

```text
BCP_CONTROLLER_WEB_ADDR     optional explicit bind address
BCP_CONTROLLER_DISABLE_WEB  disable the native web UI
BCP_PUBLIC_BASE_URL         optional explicit operator URL, future work
TAILSCALE_IP           bind host, usually exported by operator shell/service
TAILSCALE_HOST         canonical MagicDNS host with suffix
```

Resolution order for bind address:

1. `BCP_CONTROLLER_WEB_ADDR`
2. `$TAILSCALE_IP:7080`
3. `0.0.0.0:7080`

Resolution order for advertised URL:

1. `BCP_PUBLIC_BASE_URL`
2. `http://$TAILSCALE_HOST:7080`
3. `http://<system-hostname>:7080`

### Access Model

Tailnet reachability is the network boundary for Phase 1. This is not full
authentication. Mutation endpoints still need explicit operator confirmation
and audit events, and later phases should add bearer token or mTLS.

The UI should show a warning if it detects that the public base URL is not a
Tailscale MagicDNS hostname.

### Machine Controller Links

Machine controller addresses shown in the UI should also use Tailscale MagicDNS
names where possible:

```text
http://machine-name.tailnet.ts.net:7100
```

Raw Tailscale IPs may be stored for binding and diagnostics, but MagicDNS names
should be used in operator-facing URLs, client examples, route details, and
documentation.

## Implementation Phases

### Phase 1: Read-Only Console

- Add HTTP listener to `bcp-controller`.
- Serve one static HTML page.
- Add JSON endpoints for machines, profiles, accounts, leases, and events.
- Show empty states clearly.
- No mutations.

### Phase 2: Route Debugger

- Add dry-run route endpoint.
- Show selected machine/profile and rejection reasons.
- Support label selector filtering.

### Phase 3: Lease Operations

- Add release lease.
- Add renew lease.
- Add audit events for UI operations.
- Add confirmation dialogs.

### Phase 4: Profile Operations

- Add quarantine and clear quarantine.
- Add browser health detail.
- Add links from account to profile to machine.

### Phase 5: Auth

- Add bearer token or mTLS.
- Add viewer/operator/admin role checks.
- Add auth context to audit events.

## Open Questions

- Should the controller serve HTTP and gRPC from separate ports or use one
  multiplexed port?
- Should the first UI be server-rendered HTML or static HTML plus JSON?
- Should route dry-run be a gRPC method too, or only an HTTP operator endpoint?
- What is the minimum useful event retention window?
- Do we need a local machine-controller diagnostics page in addition to the
  global console?
