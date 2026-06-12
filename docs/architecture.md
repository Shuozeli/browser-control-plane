# Architecture

## Problem

We operate several machines. Each machine may run many Chrome instances. Each
Chrome instance is tied to a profile directory, and each profile may contain
logged-in accounts for platforms such as YouTube, X/Twitter, Douyin, TikTok,
Reddit, Zhihu, or Weibo.

Browser automation clients need to operate a specific account or a compatible
profile without knowing which machine owns it, which CDP port it uses, or where
the profile lives on disk.

## Two-Layer Control Plane

Browser Control Plane has two control layers:

1. Global controller
2. Machine controller

The global controller is the fleet registry and router. It tracks machines,
profiles, accounts, heartbeats, profile status, and leases. It decides which
machine should handle a request, but it does not directly talk to Chrome.

The machine controller runs once per machine. It is the local authority for
Chrome process lifecycle, profile paths, CDP ports, local health checks, and
browser command execution. It acts as a local proxy for clients that hold a
valid lease from the global controller.

```text
                  control plane
Client ───────► Global Controller
   ▲                  │
   │ route + lease    │ registry / routing / lease
   │                  ▼
   └──────────► Machine Controller
                    │
                    │ local lifecycle / proxy
                    ▼
             Chrome profile instance
```

## Request Flow

1. Client asks the global controller for a browser lease.
2. Global controller chooses a profile and machine.
3. Global controller returns a route to the machine controller and a lease.
4. Client sends browser work to the machine controller with the lease context.
5. Machine controller validates the lease context locally.
6. Machine controller ensures the browser is running.
7. Machine controller executes the operation through the local browser bridge.

The global controller is not in the browser data path after route acquisition.
That keeps high-volume browser interactions local to the owning machine.

## Resource Model

### Machine

A physical or virtual host that can run Chrome instances.

Important fields:

- `machine_id`
- `hostname`
- `tailscale_host`
- `agent_grpc_addr`
- `labels`
- `status`
- `last_heartbeat_unix_ms`

### BrowserProfile

A local Chrome profile managed by a machine controller.

Important fields:

- `profile_id`
- `machine_id`
- `profile_path`
- `display_name`
- `status`
- `cdp_url`
- `cdp_port`
- `accounts`
- `labels`

### Account

A platform account known to live inside a browser profile.

Important fields:

- `account_id`
- `platform`
- `handle`
- `health`
- `capabilities`

### BrowserLease

An exclusive right for one client to operate one profile for a bounded time.

Important fields:

- `lease_id`
- `profile_id`
- `machine_id`
- `client_id`
- `purpose`
- `fencing_token`
- `expires_at_unix_ms`

### Artifact

A short-lived file uploaded to a machine controller for browser work, such as a
video that should be selected in a YouTube upload flow.

Important fields:

- `artifact_id`
- `machine_id`
- `lease_id`
- `profile_id`
- `original_filename`
- `stored_filename`
- `content_type`
- `size_bytes`
- `uploaded_at_unix_ms`
- `expires_at_unix_ms`
- `status`

Artifacts are machine-local. The machine controller stores bytes on disk under a
dedicated artifact directory and records metadata in SQLite. The global
controller only stores reported metadata for fleet visibility.

## Data Plane Boundary

Chrome CDP ports must not be treated as public client endpoints. CDP is a
local implementation detail owned by the machine controller. This lets the
machine controller enforce lease checks, lifecycle policy, logging, and future
security rules before any browser action runs.

Machine controller browser operations route through a `PwrightGateway`
abstraction. Production implementations should embed or call the pwright layer.
Control-plane tests use a thin `RecordingPwrightGateway` to validate routing
and lease enforcement. Browser-level fake behavior belongs in the `pwright`
repo's `pwright-fake` crate, exposed as `FakePwright`.

The network topology is also abstracted behind a network directory. The global
controller asks this layer for machine endpoints instead of hard-coding network
addresses. Unit tests and Docker topology tests can replace the network layer
without changing routing or lease logic.

## Relationship To pwright

`pwright` is the browser automation primitive. Browser Control Plane decides
which browser to use and provides a stable fleet API.

The first implementation can call into a local `pwright`-style bridge directly.
Later, the machine controller may embed `pwright-bridge` as a library instead of
shelling out or proxying to `pwright-server`.
