# Deployment

This document describes how to deploy Browser Control Plane for the common
single-computer case and for a multi-computer fleet.

## Current Deployment Status

The controller and machine controller binaries are runnable today. The Docker
E2E suites also exercise the routing path end to end.

All control-plane persistence uses SQLite by default:

- `bcp-controller` stores global fleet state in `BCP_CONTROLLER_DB`, or
  `.bcp/controller.sqlite` when unset.
- `bcp-agent` stores local profile state in `BCP_AGENT_DB`, or
  `.bcp/agent.sqlite` when unset.
- Uploaded artifact metadata is stored in `artifacts.sqlite` under
  `BCP_ARTIFACT_DIR`.

`bcp-agent` auto-registers with the global controller when `BCP_CONTROLLER` is
set. Do not treat an empty `bcp-client machines` result as a networking failure
until the agent logs and `BCP_CONTROLLER` value have been checked.

## Ports And Processes

Default processes:

| Process | Binary | Default bind | Purpose |
| --- | --- | --- | --- |
| Global controller | `bcp-controller` | `$TAILSCALE_IP:7000` | Fleet registry, account lookup, routing, leases |
| Controller web UI | `bcp-controller` | `$TAILSCALE_IP:7080` | Read-only fleet dashboard and JSON snapshot |
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

The controller also starts a read-only native web UI by default:

```bash
open "http://${TAILSCALE_HOST:-127.0.0.1}:7080/"
curl "http://${TAILSCALE_HOST:-127.0.0.1}:7080/api/snapshot"
```

Use `--web-addr ${TAILSCALE_IP:-0.0.0.0}:7081` to move the web port, or
`--disable-web` to run only the gRPC controller.

Windows PowerShell:

```powershell
$env:TAILSCALE_IP = tailscale ip -4
cargo run --release -p bcp-controller -- --addr "$env:TAILSCALE_IP`:7000"
```

Optional durable path override:

```bash
export BCP_CONTROLLER_DB=$HOME/.local/share/bcp/controller.sqlite
```

### 3. Start One Machine Controller

For a fake/recording profile, useful for local smoke tests:

```bash
export BCP_MACHINE_ID=$(hostname)
export BCP_E2E_PROFILE_ID=youtube-main
export BCP_E2E_ACCOUNT_ID=yt-main
export BCP_E2E_PLATFORM=youtube
export BCP_ARTIFACT_DIR=$HOME/.local/share/bcp/artifacts
export BCP_CONTROLLER=http://${TAILSCALE_HOST:-127.0.0.1}:7000
cargo run --release -p bcp-agent -- --addr ${TAILSCALE_IP:-0.0.0.0}:7100
```

For real Chrome/CDP profiles, use `BCP_REAL_PROFILES`:

```bash
export BCP_MACHINE_ID=$(hostname)
export BCP_REAL_PROFILES='yt-main|yt-main|youtube|http://100.64.0.10:9222|https://studio.youtube.com'
export BCP_CONTROLLER=http://${TAILSCALE_HOST:-127.0.0.1}:7000
cargo run --release -p bcp-agent -- --addr ${TAILSCALE_IP:-0.0.0.0}:7100
```

`BCP_REAL_PROFILES` is a semicolon-separated list:

```text
profile_id|account_id|platform|cdp_url|initial_url;profile_id2|account_id2|platform|cdp_url|initial_url
```

Supported platform names: `youtube`, `x`, `douyin`, `tiktok`, `reddit`,
`zhihu`, `weibo`, `wsj`, `hn`, `hacker-news`, `hackernews`.

For persistent deployments, prefer a TOML config file over env profile strings:

```bash
mkdir -p .bcp
$EDITOR .bcp/agent.toml
cargo run --release -p bcp-agent -- --addr ${TAILSCALE_IP:-0.0.0.0}:7100 --config-path .bcp/agent.toml
```

See [Agent Profile Config](agent-config.md) for the schema and lifecycle
fields.

Optional durable path override:

```bash
export BCP_AGENT_DB=$HOME/.local/share/bcp/agent.sqlite
```

### 4. Confirm Auto-Registration

When `BCP_CONTROLLER` is set, `bcp-agent` periodically sends `RegisterMachine`
to the global controller with:

- `Machine.machine_id`
- `Machine.agent_grpc_addr`, preferably a Tailscale MagicDNS URL such as
  `http://my-host.tailnet.ts.net:7100`
- the current local `BrowserProfile` records and accounts

Use `BCP_AGENT_PUBLIC_ADDR` when the address clients should use is different
from the bind address.

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
export TAILSCALE_HOST=$(tailscale status --json | jq -r '.Self.DNSName' | sed 's/\.$//')
export BCP_MACHINE_ID=$(hostname)
export BCP_ARTIFACT_DIR=$HOME/.local/share/bcp/artifacts
export BCP_REAL_PROFILES='yt-main|yt-main|youtube|http://100.64.0.20:9222|https://studio.youtube.com'
export BCP_CONTROLLER=http://<controller-magicdns-name>:7000
export BCP_AGENT_PUBLIC_ADDR=http://$TAILSCALE_HOST:7100
cargo run --release -p bcp-agent -- --addr $TAILSCALE_IP:7100
```

The agent registers itself with `agent_grpc_addr =
http://<machine-magicdns-name>:7100`.

## PM2 Guide

This project does not prescribe a production service manager. For lightweight
single-user deployments, PM2 is the recommended guide path because it works
across Linux, macOS, and Windows.

Build release binaries once:

```bash
cargo build --release
```

Example `ecosystem.config.cjs`:

```javascript
const ip = process.env.TAILSCALE_IP || "0.0.0.0";
const host = process.env.TAILSCALE_HOST || "127.0.0.1";

module.exports = {
  apps: [
    {
      name: "bcp-controller",
      script: "./target/release/bcp-controller",
      args: `--addr ${ip}:7000 --db-path .bcp/controller.sqlite`,
      env: {
        RUST_LOG: "info"
      }
    },
    {
      name: "bcp-agent",
      script: "./target/release/bcp-agent",
      args: `--addr ${ip}:7100 --db-path .bcp/agent.sqlite --config-path .bcp/agent.toml`,
      env: {
        BCP_CONTROLLER: `http://${host}:7000`,
        BCP_AGENT_PUBLIC_ADDR: `http://${host}:7100`,
        RUST_LOG: "info"
      }
    }
  ]
};
```

Start and inspect:

```bash
pm2 start ecosystem.config.cjs
pm2 status
pm2 logs bcp-agent
```

Use full Tailscale MagicDNS hostnames in `BCP_CONTROLLER` and
`BCP_AGENT_PUBLIC_ADDR` when the processes are accessed from another machine.

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

SQLite persistence and auto-registration topology:

```bash
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.sqlite.yml up --build --abort-on-container-exit --exit-code-from sqlite-e2e
docker compose -f tests/e2e/docker-compose.sqlite.yml down
```

Fake failure topology for browser recovery, agent restart, and controller
re-registration:

```bash
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.failures.yml up --build --abort-on-container-exit --exit-code-from fake-failures-e2e
docker compose -f tests/e2e/docker-compose.failures.yml down
```

Real browser topology with two isolated machine networks and three Chrome
instances per machine:

```bash
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.real-browser.yml up --build --abort-on-container-exit --exit-code-from e2e-client
docker compose -f tests/e2e/docker-compose.real-browser.yml down
```

The real-browser image enables the `bcp-agent/real-pwright` Cargo feature and
fetches `pwright-bridge` from the `Shuozeli/pwright` git dependency.

Manual real-web Hacker News topology with three machine controllers and nine
Chrome instances:

```bash
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.hn.yml up --build --abort-on-container-exit --exit-code-from e2e-client
docker compose -f tests/e2e/docker-compose.hn.yml down
```

This is the recommended live-web smoke test because Hacker News is mostly
static and does not require an account.

Manual real-web WSJ topology with three machine controllers and nine Chrome
instances:

```bash
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.wsj.yml up --build --abort-on-container-exit --exit-code-from e2e-client
docker compose -f tests/e2e/docker-compose.wsj.yml down
```

This is an external-network smoke test, not a deterministic CI test.

## Environment Variables

| Variable | Used by | Meaning |
| --- | --- | --- |
| `TAILSCALE_IP` | controller, agent | Bind address host |
| `BCP_CONTROLLER_ADDR` | controller | Explicit global controller bind address |
| `BCP_CONTROLLER_DB` | controller | SQLite path for global fleet state |
| `BCP_CONTROLLER_WEB_ADDR` | controller | Explicit native web UI bind address |
| `BCP_CONTROLLER_DISABLE_WEB` | controller | Disable the native web UI when set to true |
| `BCP_AGENT_ADDR` | agent | Explicit machine controller bind address |
| `BCP_AGENT_DB` | agent | SQLite path for local profile state |
| `BCP_AGENT_CONFIG` | agent | TOML profile discovery config path |
| `BCP_AGENT_PUBLIC_ADDR` | agent | Public machine-controller URL reported to the global controller |
| `BCP_AGENT_GRPC_ADDR` | agent | Fallback public machine-controller URL |
| `BCP_CONTROLLER` | client, E2E | Global controller endpoint |
| `BCP_MACHINE_ID` | agent | Machine identity for local profiles |
| `BCP_E2E_PROFILE_ID` | agent | Fake profile id for recording gateway |
| `BCP_E2E_ACCOUNT_ID` | agent | Fake account id for recording gateway |
| `BCP_E2E_PLATFORM` | agent | Fake account platform |
| `BCP_E2E_HEALTHY` | agent | Initial fake browser health for recording gateway |
| `BCP_E2E_HEALTH_MESSAGE` | agent | Initial fake browser health message |
| `BCP_REAL_PROFILES` | agent | Real CDP profile mapping |
| `BCP_BROWSER_HEARTBEAT_SECONDS` | agent | Local fleet reconcile interval |
| `BCP_CONTROLLER_REGISTER_SECONDS` | agent | Agent auto-registration interval |
| `BCP_ARTIFACT_DIR` | agent | Dedicated local artifact directory |
| `BCP_ARTIFACT_MAX_TTL_SECONDS` | agent | Max accepted upload TTL |
| `BCP_ARTIFACT_CLEANUP_SECONDS` | agent | Artifact cleanup scan interval |

## Production Gaps To Track

- Service manager examples for systemd, launchd, and Windows services.
- Richer controller-to-agent health heartbeat reporting beyond registration refresh.
- Authn/authz for controller and machine-controller APIs.
- TLS or mesh-level policy for gRPC traffic.
