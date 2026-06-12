# Browser Control Plane

Pure Rust gRPC control plane for routing browser automation work across many
machines, Chrome instances, profiles, and logged-in accounts.

The project is intentionally separate from `pwright`. `pwright` remains the
single-browser CDP automation data plane. Browser Control Plane manages a fleet:
where profiles live, which machine owns them, which client currently has a
lease, and which local machine controller should execute browser work.

## Architecture

```text
Client
  |
  | LookupBrowserConnection / AcquireBrowser / GetRoute
  v
Global Controller
  |
  | returns route + lease
  v
Client
  |
  | browser work with lease token
  v
Machine Controller
  |
  | local proxy to Chrome / pwright bridge
  v
Chrome instance for one profile/account
```

Clients do not call Chrome CDP ports directly. They also do not call `pwright`
directly. The global controller is a routing and lease service; the machine
controller is the local proxy and lifecycle manager for browsers on one host.
Machine controllers also own local short-lived artifacts such as upload videos:
clients stream files to the routed machine controller with a required TTL, and
metadata can be reported back to the global controller for fleet visibility.

User flow:

1. Call `LookupBrowserConnection` with `platform` and `account_id` to see which
   machine/profile owns the account and whether it is currently available.
2. Call `AcquireBrowser` before executing work. The lease returns the fencing
   token and route to the machine controller.
3. Send browser actions to the machine controller. The machine controller routes
   traffic through the local `pwright` layer and Chrome CDP endpoint.

## Crates

- `bcp-proto`: generated protobuf/gRPC bindings.
- `bcp-controller`: global controller binary.
- `bcp-agent`: per-machine controller binary.
- `bcp-client`: initial CLI/client for control-plane operations.

## Development

```bash
cargo check
cargo run --release -p bcp-controller
cargo run --release -p bcp-agent
cargo run --release -p bcp-client -- machines
cargo run --release -p bcp-client -- lookup --platform youtube --account-id yt-1
```

Server binaries bind to `$TAILSCALE_IP` when it is set, otherwise `0.0.0.0`.
Machine artifact storage is controlled by `BCP_ARTIFACT_DIR`,
`BCP_ARTIFACT_MAX_TTL_SECONDS`, and `BCP_ARTIFACT_CLEANUP_SECONDS`.

For deployment, start with the single-computer path in
[Deployment](docs/deployment.md). It also documents the current production gap:
`bcp-agent` does not yet auto-register with the global controller, so real
deployments need a bootstrap registration step until that feature is added.

## Documentation

- [Architecture](docs/architecture.md)
- [Deployment](docs/deployment.md)
- [Agent Runbook](docs/agent-runbook.md)
- [Design](docs/design.md)
- [HTML Serve Design](docs/html-serve-design.md)
- [Observability Design](docs/observability-design.md)
- [Artifact Store Design](docs/artifact-store-design.md)
- [Tasks](docs/tasks.md)
- [Codelabs](docs/codelabs.md)
