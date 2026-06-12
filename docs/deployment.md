# Deployment

This document describes how to deploy Browser Control Plane for the common
single-computer case and for a multi-computer fleet.

## Current Deployment Status

The controller and machine controller binaries are runnable today. The Docker
E2E suites also exercise the routing path end to end.

One production feature is still missing: `bcp-agent` does not yet
automatically register itself with the global controller. Current E2E tests
perform registration from the test client. Until agent auto-registration is
implemented, a real deployment must register machines/profiles through the
global gRPC API or through a small bootstrap client.

Do not treat an empty `bcp-client machines` result as a networking failure
until registration has been checked.

## Ports And Processes

Default processes:

| Process | Binary | Default bind | Purpose |
| --- | --- | --- | --- |
| Global controller | `bcp-controller` | `$TAILSCALE_IP:7000` | Fleet registry, account lookup, routing, leases |
| Machine controller | `bcp-agent` | `$TAILSCALE_IP:7100` | Local browser lifecycle, lease validation, pwright/CDP proxy |
| CLI | `bcp-client` | n/a | Operator/client lookup commands |

Default bind behavior:

1. Explicit `--addr` or env var wins.
2. If `$TAILSCALE_IP` is set, bind to that address.
3. Otherwise bind to `0.0.0.0` for local development.

Client-facing URLs should use the full Tailscale MagicDNS name when possible:

```bash
export TAILSCALE_IP=$(tailscale ip -4)
export TAILSCALE_HOST=$(tailscale status --json | jq -r '.Self.DNSName' | sed 's/\.$//')
export BCP_CONTROLLER=http://$TAILSCALE_HOST:7000
```

On Windows PowerShell:

```powershell
$env:TAILSCALE_IP = tailscale ip -4
$env:TAILSCALE_HOST = (tailscale status --json | ConvertFrom-Json).Self.DNSName.TrimEnd(".")
$env:BCP_CONTROLLER = "http://$env:TAILSCALE_HOST:7000"
```

## Single-Computer Deployment

Most users will run everything on one computer. In this mode the global
controller and one machine controller run on the same host.

### 1. Build

```bash
cargo build --release
```

### 2. Start The Global Controller

Linux/macOS:

```bash
export TAILSCALE_IP=$(tailscale ip -4 2>/dev/null || true)
export TAILSCALE_HOST=$(tailscale status --json 2>/dev/null | jq -r '.Self.DNSName' | sed 's/\.$//')
cargo run --release -p bcp-controller -- --addr ${TAILSCALE_IP:-0.0.0.0}:7000
```

Windows PowerShell:

```powershell
$env:TAILSCALE_IP = tailscale ip -4
cargo run --release -p bcp-controller -- --addr "$env:TAILSCALE_IP`:7000"
```

### 3. Start One Machine Controller

For a fake/recording profile, useful for local smoke tests:

```bash
export BCP_MACHINE_ID=$(hostname)
export BCP_E2E_PROFILE_ID=youtube-main
export BCP_E2E_ACCOUNT_ID=yt-main
export BCP_E2E_PLATFORM=youtube
export BCP_ARTIFACT_DIR=$HOME/.local/share/bcp/artifacts
cargo run --release -p bcp-agent -- --addr ${TAILSCALE_IP:-0.0.0.0}:7100
```

For real Chrome/CDP profiles, use `BCP_REAL_PROFILES`:

```bash
export BCP_MACHINE_ID=$(hostname)
export BCP_REAL_PROFILES='yt-main|yt-main|youtube|http://100.64.0.10:9222|https://studio.youtube.com'
cargo run --release -p bcp-agent -- --addr ${TAILSCALE_IP:-0.0.0.0}:7100
```

`BCP_REAL_PROFILES` is a semicolon-separated list:

```text
profile_id|account_id|platform|cdp_url|initial_url;profile_id2|account_id2|platform|cdp_url|initial_url
```

Supported platform names: `youtube`, `x`, `douyin`, `tiktok`, `reddit`,
`zhihu`, `weibo`.

### 4. Register The Machine

Current limitation: the machine controller does not auto-register with the
global controller yet.

For now, use one of these paths:

- Run the Docker E2E harness, which registers machines before testing routes.
- Use a small gRPC bootstrap client that calls `RegisterMachine`.
- Add auto-registration to `bcp-agent` before using this as a persistent
  production deployment.

The registration data must include:

- `Machine.machine_id`
- `Machine.agent_grpc_addr`, using the Tailscale MagicDNS URL, for example
  `http://my-host.tailnet.ts.net:7100`
- One or more `BrowserProfile` records with `accounts`

### 5. Verify Lookup

After registration, the user-facing lookup path is:

```bash
BCP_CONTROLLER=http://$TAILSCALE_HOST:7000 \
  cargo run --release -p bcp-client -- lookup --platform youtube --account-id yt-main
```

Expected output includes:

```text
account_id=yt-main
profile_id=youtube-main
machine_id=<machine-id>
agent_grpc_addr=http://<tailscale-host>:7100
available=true
```

If `available=false`, the profile is leased, launching, broken, or quarantined.
Lookup does not return fencing tokens. A client must call `AcquireBrowser`
before executing browser work.

## Multi-Computer Deployment

Use this mode when several computers each own local Chrome profiles.

### Topology

```text
Client(s)
   |
   | gRPC to global controller
   v
Global controller on one stable host
   |
   | returns machine controller route
   v
Machine controller on each computer
   |
   | local pwright/CDP proxy
   v
Chrome profiles on that computer
```

### 1. Install Tailscale Everywhere

Each computer should be reachable by MagicDNS. Record:

```bash
tailscale ip -4
tailscale status --json | jq -r '.Self.DNSName' | sed 's/\.$//'
```

Use MagicDNS hostnames in `agent_grpc_addr`; use raw Tailscale IPs only for
binding.

### 2. Choose The Global Controller Host

On the selected host:

```bash
export TAILSCALE_IP=$(tailscale ip -4)
cargo run --release -p bcp-controller -- --addr $TAILSCALE_IP:7000
```

Clients should use:

```bash
export BCP_CONTROLLER=http://<controller-magicdns-name>:7000
```

### 3. Start One Agent Per Machine

On every browser host:

```bash
export TAILSCALE_IP=$(tailscale ip -4)
export BCP_MACHINE_ID=$(hostname)
export BCP_ARTIFACT_DIR=$HOME/.local/share/bcp/artifacts
export BCP_REAL_PROFILES='yt-main|yt-main|youtube|http://100.64.0.20:9222|https://studio.youtube.com'
cargo run --release -p bcp-agent -- --addr $TAILSCALE_IP:7100
```

Register each machine in the global controller with its MagicDNS address:

```text
agent_grpc_addr = http://<machine-magicdns-name>:7100
```

### 4. Verify Fleet Routing

From a client machine:

```bash
export BCP_CONTROLLER=http://<controller-magicdns-name>:7000
cargo run --release -p bcp-client -- machines
cargo run --release -p bcp-client -- lookup --platform youtube --account-id yt-main
```

Then verify the returned machine controller is reachable:

```bash
curl -v http://<machine-magicdns-name>:7100
```

The curl request is expected to fail at the HTTP/gRPC protocol layer, but it
should connect. Connection refused or timeout indicates a bind, firewall, or
Tailscale ACL problem.

## Docker E2E Deployment Tests

Recording fake pwright topology:

```bash
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.yml up --build --abort-on-container-exit --exit-code-from e2e-client
docker compose -f tests/e2e/docker-compose.yml down
```

Real browser topology with two isolated machine networks and three Chrome
instances per machine:

```bash
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.real-browser.yml up --build --abort-on-container-exit --exit-code-from e2e-client
docker compose -f tests/e2e/docker-compose.real-browser.yml down
```

The real-browser image enables the `bcp-agent/real-pwright` Cargo feature and
fetches `pwright-bridge` from the `Shuozeli/pwright` git dependency.

## Environment Variables

| Variable | Used by | Meaning |
| --- | --- | --- |
| `TAILSCALE_IP` | controller, agent | Bind address host |
| `BCP_CONTROLLER_ADDR` | controller | Explicit global controller bind address |
| `BCP_AGENT_ADDR` | agent | Explicit machine controller bind address |
| `BCP_CONTROLLER` | client, E2E | Global controller endpoint |
| `BCP_MACHINE_ID` | agent | Machine identity for local profiles |
| `BCP_E2E_PROFILE_ID` | agent | Fake profile id for recording gateway |
| `BCP_E2E_ACCOUNT_ID` | agent | Fake account id for recording gateway |
| `BCP_E2E_PLATFORM` | agent | Fake account platform |
| `BCP_REAL_PROFILES` | agent | Real CDP profile mapping |
| `BCP_BROWSER_HEARTBEAT_SECONDS` | agent | Local fleet reconcile interval |
| `BCP_ARTIFACT_DIR` | agent | Dedicated local artifact directory |
| `BCP_ARTIFACT_MAX_TTL_SECONDS` | agent | Max accepted upload TTL |
| `BCP_ARTIFACT_CLEANUP_SECONDS` | agent | Artifact cleanup scan interval |

## Production Gaps To Track

- Agent auto-registration and heartbeat reporting to the global controller.
- Persistent global controller storage.
- Service manager examples for systemd, launchd, and Windows services.
- Authn/authz for controller and machine-controller APIs.
- TLS or mesh-level policy for gRPC traffic.
