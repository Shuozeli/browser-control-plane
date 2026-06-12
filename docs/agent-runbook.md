# Agent Runbook

This runbook is for coding agents and operations agents that need to explore,
deploy, test, debug, and report issues in Browser Control Plane.

## First Read

Read these files before changing behavior:

1. `README.md`
2. `docs/architecture.md`
3. `docs/deployment.md`
4. `proto/browsercontrol/v1/control_plane.proto`
5. `crates/bcp-controller/src/lib.rs`
6. `crates/bcp-agent/src/main.rs`
7. `crates/bcp-agent/src/lib.rs`
8. `crates/bcp-e2e/src/main.rs`

Important architecture rule: clients do not call Chrome CDP directly. Browser
work should route through the machine controller, and the machine controller
routes through the pwright layer.

## Mental Model

The system has two layers:

- Global controller: machine/profile/account registry, lookup, route selection,
  lease grant/renew/release, fleet metadata.
- Machine controller: local browser lifecycle, lease validation, artifact
  storage, pwright/CDP proxy.

The most common user deployment is one computer:

```text
same host:
  bcp-controller :7000
  bcp-agent      :7100
  Chrome/CDP     local implementation detail
```

The multi-computer deployment adds one `bcp-agent` per browser host:

```text
controller host:
  bcp-controller :7000

browser host A:
  bcp-agent :7100
  Chrome profiles

browser host B:
  bcp-agent :7100
  Chrome profiles
```

## Registration And State

`bcp-agent` auto-registers itself when `BCP_CONTROLLER` is set. It reports the
machine id, public agent gRPC address, labels, and current browser profiles.
The loop repeats on `BCP_CONTROLLER_REGISTER_SECONDS`, defaulting to 10 seconds.

If `bcp-client machines` returns `0`, first determine whether registration
happened. Do not immediately report a network bug. Check:

- Is `BCP_CONTROLLER` set in the agent environment?
- Does the agent log show controller registration errors?
- Does `BCP_AGENT_PUBLIC_ADDR` or `TAILSCALE_HOST` point to a client-reachable
  address?
- Does the agent have profiles from `BCP_REAL_PROFILES`, `BCP_E2E_*`, or
  `BCP_AGENT_DB`?

Durable state is SQLite-backed. The global controller uses
`BCP_CONTROLLER_DB` or `.bcp/controller.sqlite`; the machine controller uses
`BCP_AGENT_DB` or `.bcp/agent.sqlite`.

## Fast Exploration Checklist

Run these from the repo root:

```bash
cargo fmt --all --check
cargo test
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.yml up --build --abort-on-container-exit --exit-code-from e2e-client
docker compose -f tests/e2e/docker-compose.yml down
```

If the task involves real Chrome/CDP routing, also run:

```bash
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.real-browser.yml up --build --abort-on-container-exit --exit-code-from e2e-client
docker compose -f tests/e2e/docker-compose.real-browser.yml down
```

Use the real-browser suite only when the local `pwright` repo path exists and
Docker can pull `chromedp/headless-shell`.

## Deployment Verification

### Single Host

1. Start `bcp-controller`.
2. Start `bcp-agent` with `BCP_CONTROLLER=http://<controller-host>:7000`.
3. Confirm registration has occurred.
4. Run:

```bash
cargo run --release -p bcp-client -- machines
cargo run --release -p bcp-client -- lookup --platform youtube --account-id yt-main
```

Expected lookup behavior:

- It returns `machine_id`, `profile_id`, `agent_grpc_addr`, and availability.
- It does not return a fencing token.
- If a profile is busy, it may return `active_lease_id`, but not the
  `fencing_token`.

### Multi Host

1. Confirm every machine has Tailscale connectivity.
2. Confirm controller bind address is the controller host's Tailscale IP.
3. Confirm every `agent_grpc_addr` uses MagicDNS, not a raw IP, for client
   visibility.
4. From a client, run `lookup`.
5. Confirm the returned machine controller address is reachable.

Reachability probe:

```bash
curl -v http://<machine-magicdns-name>:7100
```

A protocol-level failure is acceptable because the service is gRPC. Timeout or
connection refused is not acceptable.

## Bug Discovery Strategy

### Registry Bugs

Symptoms:

- `machines` returns zero.
- `lookup` returns NotFound for an account that should exist.
- A profile appears under the wrong machine.

Check:

- Did `RegisterMachine` run?
- Is `BCP_CONTROLLER` configured on the agent?
- Does `BrowserProfile.machine_id` match the reporting `Machine.machine_id`?
- Does `Machine.agent_grpc_addr` point to the machine controller MagicDNS URL?
- Are `Account.platform` and `Account.account_id` exactly what lookup uses?

Relevant files:

- `crates/bcp-controller/src/lib.rs`
- `proto/browsercontrol/v1/control_plane.proto`
- `crates/bcp-e2e/src/main.rs`

### Lease Bugs

Symptoms:

- Two clients can acquire the same profile.
- Expired leases still work.
- Lookup exposes a fencing token.
- Machine controller accepts work without an installed lease.

Check:

- `AcquireBrowser` should mark the profile leased.
- Heartbeat/register must not make a leased profile available while a lease
  exists.
- `RenewLease`, `ReleaseLease`, and `GetRoute` should clean expired leases
  before accepting tokens.
- Lookup must never return `fencing_token`.
- Machine controller operations should call local lease validation.

Relevant files:

- `crates/bcp-controller/src/lib.rs`
- `crates/bcp-agent/src/lib.rs`

### Network Bugs

Symptoms:

- Acquire succeeds, but connecting to returned `agent_grpc_addr` fails.
- Docker real-browser test can reach controller but not agent.
- Host rejects Chrome CDP connections.

Check:

- Controller and agent bind addresses.
- Docker network membership.
- Tailscale ACLs or host firewall.
- Whether `agent_grpc_addr` is client-routable.
- Chrome DevTools Host header restrictions. In Docker real-browser tests,
  fixed IP CDP URLs are used because Chrome rejects some container hostname
  Host headers.

Relevant files:

- `crates/bcp-core/src/network.rs`
- `tests/e2e/docker-compose.yml`
- `tests/e2e/docker-compose.real-browser.yml`

### Pwright/CDP Bugs

Symptoms:

- Agent can list local profiles but snapshot/action/evaluate fails.
- Real browser compose fails after lease installation.
- Fake recording gateway passes but real-browser test fails.

Check:

- `BCP_REAL_PROFILES` format.
- CDP endpoint reachability from the agent container or host.
- `initial_url` loads successfully.
- Whether the failure is in `RealPwrightGateway` or in control-plane routing.

Relevant files:

- `crates/bcp-core/src/pwright.rs`
- `crates/bcp-agent/src/main.rs`
- `crates/bcp-agent/src/lib.rs`

### Artifact Bugs

Symptoms:

- Upload succeeds but file is not visible.
- Expired file stays on disk.
- Upload accepts missing or too-long TTL.

Check:

- `BCP_ARTIFACT_DIR`
- `BCP_ARTIFACT_MAX_TTL_SECONDS`
- `BCP_ARTIFACT_CLEANUP_SECONDS`
- Machine-local artifact list before global `ReportArtifacts`

Relevant files:

- `crates/bcp-core/src/artifact.rs`
- `crates/bcp-agent/src/lib.rs`
- `docs/artifact-store-design.md`

## Test Selection

Use focused tests first:

```bash
cargo test -p bcp-controller
cargo test -p bcp-agent
cargo test -p bcp-core
```

Use full test suite before final response:

```bash
cargo test
```

Use Docker E2E for topology or routing changes:

```bash
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.yml up --build --abort-on-container-exit --exit-code-from e2e-client
```

Use real-browser E2E for CDP/pwright changes:

```bash
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.real-browser.yml up --build --abort-on-container-exit --exit-code-from e2e-client
```

Always clean Docker topology after a run:

```bash
docker compose -f tests/e2e/docker-compose.yml down
docker compose -f tests/e2e/docker-compose.real-browser.yml down
```

## Bug Report Format

When reporting a bug, include:

```text
Title:
Severity: Critical | High | Medium | Low
Area: registry | lease | network | pwright | artifact | docs | client

Environment:
- OS:
- Commit or branch:
- Command:
- Tailscale used: yes/no
- Docker used: yes/no

Reproduction:
1.
2.
3.

Expected:

Actual:

Evidence:
- Logs:
- gRPC status codes:
- Test output:
- Relevant machine_id/profile_id/account_id:

Suspected files:
-

Notes:
```

Do not include fencing tokens, cookies, raw URLs with secrets, uploaded file
contents, page HTML, or account credentials in a bug report.

## Common False Positives

- Empty machine list before registration is expected with current binaries.
- `curl` against a gRPC endpoint may return a protocol error even when the
  network path is healthy.
- Lookup is not an acquire operation. A profile can be visible but unavailable.
- Lookup should not expose a fencing token; that is intentional.
- Recording fake pwright tests do not prove real CDP behavior.

## Escalation Rules

Escalate immediately if:

- A fencing token is exposed outside `AcquireBrowser`, `RenewLease`, or
  authenticated lease flows.
- Two clients can concurrently acquire the same profile.
- A machine can register profiles for another machine.
- A machine controller accepts browser work without an installed matching
  lease.
- Raw page content, full URLs with secrets, cookies, or uploaded file bytes are
  retained in global telemetry.
