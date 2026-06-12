# Codelabs

These codelabs are short operator paths for validating Browser Control Plane.
For full deployment details, see [Deployment](deployment.md). For debugging and
bug reporting, see [Agent Runbook](agent-runbook.md).

## Codelab 1: Single-Computer Smoke Test

Use this when developing on one computer.

### Start the global controller

```bash
export TAILSCALE_IP=$(tailscale ip -4 2>/dev/null || true)
cargo run --release -p bcp-controller -- --addr ${TAILSCALE_IP:-0.0.0.0}:7000
```

### Start one recording machine controller

In another shell:

```bash
export TAILSCALE_IP=$(tailscale ip -4 2>/dev/null || true)
export BCP_MACHINE_ID=$(hostname)
export BCP_E2E_PROFILE_ID=youtube-main
export BCP_E2E_ACCOUNT_ID=yt-main
export BCP_E2E_PLATFORM=youtube
cargo run --release -p bcp-agent -- --addr ${TAILSCALE_IP:-0.0.0.0}:7100
```

### Register profiles

Current limitation: `bcp-agent` does not auto-register with the global
controller. Use a bootstrap client or the Docker E2E harness to call
`RegisterMachine`.

After registration, verify lookup:

```bash
export BCP_CONTROLLER=http://<controller-magicdns-name>:7000
cargo run --release -p bcp-client -- lookup --platform youtube --account-id yt-main
```

Expected result:

```text
profile_id=youtube-main
available=true
agent_grpc_addr=http://<machine-magicdns-name>:7100
```

## Codelab 2: Recording Docker E2E

This test uses fake/recording pwright gateways and validates:

- machine registration
- account/profile routing
- exclusive lease flow
- machine-controller lease installation
- snapshot/action proxy path
- artifact upload and global artifact reporting
- wrong-machine lease rejection

Run:

```bash
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.yml up --build --abort-on-container-exit --exit-code-from e2e-client
docker compose -f tests/e2e/docker-compose.yml down
```

Expected terminal line:

```text
docker e2e passed: controller routed to recording pwright gateway
```

## Codelab 3: Real-Browser Docker E2E

This test runs:

- one global controller
- two machine controllers
- three Chrome/CDP containers per machine
- one static web site per machine network
- one E2E client that registers both machines and routes through the controller

Run:

```bash
COMPOSE_PROGRESS=plain docker compose -f tests/e2e/docker-compose.real-browser.yml up --build --abort-on-container-exit --exit-code-from e2e-client
docker compose -f tests/e2e/docker-compose.real-browser.yml down
```

Expected terminal line:

```text
docker real-browser e2e passed: controller routed to real CDP browsers
```

If this fails before building, check whether Cargo can fetch the `Shuozeli/pwright`
git dependency used by the `bcp-agent/real-pwright` feature.

## Codelab 4: Lookup Versus Acquire

Lookup answers where an account lives and whether it is available. It does not
grant permission to operate the browser.

```bash
cargo run --release -p bcp-client -- lookup --platform youtube --account-id yt-main
```

Acquire grants a lease and returns a fencing token. Browser operations must use
the machine controller route and the lease context. The lower-level gRPC API is
implemented; CLI helpers for acquire/release/action are still tracked in
[Tasks](tasks.md).

Important security invariant:

- Lookup may show `active_lease_id`.
- Lookup must not show `fencing_token`.
- Browser work must go through the machine controller, not directly to CDP.

## Codelab 5: Bug Report Drill

When a codelab fails, create a bug report using the template in
[Agent Runbook](agent-runbook.md#bug-report-format).

Minimum useful report:

```text
Title:
Severity:
Area:
Command:
Expected:
Actual:
Evidence:
Suspected files:
```

Do not include fencing tokens, cookies, raw page content, uploaded file bytes,
or full URLs containing secrets.
