# Observability Design

## Purpose

Browser Control Plane needs an observability plane for fleet health and browser
usage. Operators need to understand machine status, browser health, profile
health, account health, lease pressure, and web access patterns without storing
full browsing history or page contents.

The observability design follows an OpenTelemetry-like model:

- metrics are aggregated and bounded
- events are structured and sampled/retained for a short window
- high-cardinality raw data is avoided by default
- full URLs, HTML, request bodies, and response bodies are not stored

## Signals

### Machine Health

Collected from each machine controller.

Metrics:

- heartbeat age
- agent process uptime
- local profile count
- running Chrome count
- failed Chrome launch count
- local disk free bytes
- CPU load
- memory usage

### Browser Health

Collected per browser profile.

Metrics:

- browser process running
- CDP reachable
- last successful command timestamp
- command success/error counts
- launch latency
- crash count
- restart count
- memory usage, if available

### Account/Profile Health

Collected by machine controller or explicit health probes.

Metrics:

- login status: logged_in, challenge, expired, banned, unknown
- last health check timestamp
- consecutive health check failures
- platform capability availability

### Lease Health

Collected by global controller.

Metrics:

- active lease count
- lease acquisition count
- lease rejection count
- lease expiration count
- lease duration histogram
- profile contention count

### Web Access Aggregation

Collected by the machine controller through the pwright layer.

Metrics:

- requests by domain
- responses by domain and status class
- bytes sent/received by domain
- page navigations by domain
- command latency by action type
- network error counts by domain/error class

Raw data that must not be stored by default:

- full URL
- query strings
- request headers
- response headers
- request body
- response body
- HTML content
- screenshot content

## Cardinality Rules

Observability data must be bounded.

Allowed dimensions:

- machine ID
- profile ID
- platform
- account health enum
- domain
- status class, such as `2xx`, `3xx`, `4xx`, `5xx`
- action type, such as `snapshot`, `click`, `evaluate`
- error class

Avoid dimensions:

- full URL
- URL path
- arbitrary page title
- request ID
- account handle, unless explicitly needed for operator display
- user-provided purpose strings as metric labels

If URL-level debugging is required, it should be an opt-in short-lived trace
mode with a strict retention window and redaction.

## Domain Extraction

The machine controller should reduce URLs to registrable domains before
aggregation.

Examples:

```text
https://studio.youtube.com/video/abc/edit?token=secret -> youtube.com
https://x.com/home                                 -> x.com
https://api.twitter.com/2/timeline                 -> twitter.com
```

The first implementation may use hostnames. Later versions can use the public
suffix list to compute registrable domains correctly.

## Data Model

### MetricSample

One observed metric increment or gauge update.

Fields:

- metric name
- timestamp
- machine ID
- profile ID
- platform
- domain
- action
- status class
- error class
- value

### Aggregation Bucket

Metrics are stored in fixed time buckets.

Recommended initial bucket:

```text
1 minute
```

Recommended initial retention:

```text
24 hours in memory
7 days in persistent storage later
```

Bucket key:

```text
metric_name + bucket_start + machine_id + profile_id + platform + domain + action + status_class + error_class
```

### ControlPlaneEvent

Structured event for operational state changes.

Examples:

- machine heartbeat accepted
- machine marked offline
- browser health changed
- profile quarantined
- lease acquired
- lease released
- pwright command failed

Events should be retained for a short window and should avoid raw web content.

## APIs

Global controller should expose:

- `ReportTelemetry`: machine controllers push aggregated samples/events.
- `GetMetricSummary`: operator/client query aggregated data.
- `ListControlPlaneEvents`: query recent structured events.

Machine controller should aggregate locally before reporting. This reduces
traffic and avoids shipping raw browsing details to the global controller.

## OpenTelemetry Dashboard Strategy

Browser Control Plane should not build its own dashboard UI for fleet health.
The controller and machine controllers should emit OpenTelemetry-compatible
signals and let existing observability tools provide dashboards.

Recommended dashboard surfaces:

- fleet overview: machine count, online/offline/degraded state
- browser overview: running browsers, crashed browsers, restart rate
- profile/account health: logged-in/challenge/unknown counts
- lease pressure: active leases, acquisition failures, contention
- web access: domain-level request/byte/error aggregation

Metric naming should stay stable and low-cardinality:

```text
bcp.machine.heartbeat.age_ms
bcp.browser.running
bcp.browser.restart.count
bcp.browser.command.count
bcp.browser.command.error.count
bcp.lease.active
bcp.lease.acquire.count
bcp.web.request.count
bcp.web.bytes_received
```

The in-memory `ReportTelemetry` API is the first implementation. A later
exporter can translate the same metric/event model to OTLP without changing the
fleet manager or pwright proxy logic.

## Local Aggregation

The machine controller should aggregate web access metrics locally:

```text
pwright/network event -> domain reducer -> local minute bucket -> periodic report
```

The local aggregator should flush periodically and on shutdown.

## Privacy And Safety

- Store domain-level metrics, not full browsing history.
- Do not store page content.
- Do not store request/response payloads.
- Treat profile/account IDs as sensitive.
- Make raw trace mode explicit, temporary, and audited.

## First Implementation

Phase 1 should implement in-memory global aggregation:

- Add telemetry proto messages.
- Add `ReportTelemetry`.
- Add `GetMetricSummary`.
- Add `ListControlPlaneEvents`.
- Store metric buckets in memory.
- Add tests proving multiple URL paths aggregate into one domain bucket.
- Add tests proving full URLs are not returned by summary APIs.

Persistence can come after the aggregation semantics are stable.
