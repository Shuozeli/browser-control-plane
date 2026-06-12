# Docker E2E Topology

This directory defines the intended multi-network end-to-end test topology for
Browser Control Plane.

The goal is to test control-plane routing without coupling tests to the real
host network or real Chrome processes.

## Network Shape

```text
k3s-pod-net
  ├── e2e-client
  ├── bcp-controller
  ├── bcp-agent-a
  └── bcp-agent-b
```

The Docker network intentionally behaves like a flat k3s pod network: every
container can resolve every service by DNS name. The test still validates that
browser operations require controller-issued leases and that the wrong machine
controller rejects a lease that was not installed locally.

Each machine agent runs with a recording pwright gateway configured through
environment variables. Browser-level fake behavior should come from
`pwright-fake::FakePwright`; this e2e gateway only verifies control-plane
routing and local lease enforcement.

## Intended Test Cases

- Register two machine agents with different profiles from the e2e runner.
- Acquire a YouTube account and receive a route to agent A.
- Install the lease on agent A.
- Execute a browser action through agent A using the lease context.
- Verify agent A's recording gateway returns a snapshot/action success.
- Verify agent B rejects the same lease because it was not installed there.

## Running

```bash
docker compose -f tests/e2e/docker-compose.yml up --build --abort-on-container-exit
```
