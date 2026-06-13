# Design

## Goals

- Manage many machines, Chrome instances, browser profiles, and logged-in
  accounts.
- Route client work to the correct machine controller.
- Keep direct Chrome CDP access behind the local machine controller.
- Prevent concurrent clients from operating the same profile/account.
- Use pure Rust gRPC services and typed protobuf contracts.
- Keep `pwright` focused on single-browser automation.

## Non-Goals

- A write-capable web dashboard in the first phase.
- Browser stealth or anti-detection logic.
- Direct client access to Chrome CDP ports.
- Multi-tenant public internet exposure.
- Replacing `pwright` browser automation primitives.

## Services

### GlobalController

The global controller owns routing and lease decisions.

Responsibilities:

- Register machine controllers.
- Accept machine heartbeats.
- Maintain machine/profile/account registry.
- Select profiles by account ID, platform, labels, and availability.
- Grant, renew, and release leases.
- Return routes to machine controllers.
- Mark machines offline when heartbeats expire.
- Quarantine broken profiles.

### MachineController

The machine controller owns local runtime state.

Responsibilities:

- Discover configured local profiles.
- Allocate CDP ports.
- Launch Chrome with the correct `--user-data-dir`.
- Track Chrome process IDs and health.
- Validate lease context for every browser operation.
- Execute browser operations through a local automation bridge.
- Report profile and account health to the global controller.

All browser-facing methods on the machine controller must go through the local
pwright gateway. The machine controller is a lease-validating proxy; it should
not allow clients to bypass pwright or talk directly to Chrome CDP.

## Routing Model

Clients first call `GlobalController.AcquireBrowser`. The response contains:

- `BrowserLease`: the exclusive operating right.
- `BrowserRoute`: the machine controller address and lease context.

Clients then call the `MachineController` directly for browser operations.

This keeps the global controller as a routing system instead of a high-volume
browser proxy.

The global controller depends on a network directory abstraction to resolve a
machine into an agent endpoint. Tests can inject a fake/static topology, while
production can resolve Tailscale MagicDNS endpoints or service-discovery data.

## Operator UI

The first implementation includes a native read-only controller web UI. It is
for fleet inspection only:

- view machines, profiles, accounts, active leases, recent events, metrics, and
  artifacts
- fetch the same data as JSON from `/api/snapshot`
- avoid exposing `fencing_token` or write operations

Mutating actions such as lease release, profile quarantine, or browser restarts
remain future work and need audit events and authentication before they are
enabled from the UI.

## Lease Model

A profile should be exclusively leased before browser work starts.

```text
available -> leased -> available
available -> launching -> leased
leased -> expired -> available
leased -> broken -> quarantined
```

Every machine-controller operation must include:

- `lease_id`
- `profile_id`
- `fencing_token`

The fencing token prevents delayed or retried old requests from acting on a
profile after a newer lease has been granted.

## Local Chrome Runtime

Each profile maps to one Chrome runtime record:

```text
profile_id
profile_path
pid
cdp_port
cdp_url
started_at_unix_ms
health
```

The machine controller should allocate ports from a configured range, bind CDP
to the Tailscale interface or loopback, and avoid exposing CDP as the stable
client API.

Example launch shape:

```bash
google-chrome \
  --user-data-dir=/data/browser-profiles/youtube-main \
  --remote-debugging-address=$TAILSCALE_IP \
  --remote-debugging-port=9312 \
  --no-first-run \
  --disable-default-apps
```

## Storage

Global controller storage should be Postgres.

Initial tables:

- `machines`
- `browser_profiles`
- `accounts`
- `profile_accounts`
- `leases`
- `machine_heartbeats`
- `audit_events`
- `browser_events`

Machine controller storage should be local SQLite.

Initial tables:

- `local_profiles`
- `chrome_processes`
- `port_allocations`
- `lease_cache`
- `health_events`

## Security

First phase security is tailnet-scoped access plus lease fencing. Later phases
should add service tokens or mTLS between clients, global controller, and
machine controllers.

Machine controllers must reject browser operations without a valid current
lease context.

## Failure Handling

If a machine stops heartbeating, the global controller marks it offline and
stops routing new leases to its profiles.

If a lease expires, the profile becomes available only after the machine
controller confirms no active local operation is running or the local lease
cache has also expired.

If Chrome fails health checks, the machine controller marks the profile broken
and reports that status in the next heartbeat.

If account login status is unknown, the profile remains routable only for
operations that explicitly accept unknown account health.
