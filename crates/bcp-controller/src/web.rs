use std::net::SocketAddr;

use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::ControllerService;

#[derive(Debug, Clone, Serialize)]
pub struct ControllerWebSnapshot {
    pub generated_at_unix_ms: i64,
    pub counts: FleetCounts,
    pub machines: Vec<MachineView>,
    pub profiles: Vec<ProfileView>,
    pub accounts: Vec<AccountBindingView>,
    pub leases: Vec<LeaseView>,
    pub metrics: Vec<MetricView>,
    pub events: Vec<EventView>,
    pub artifacts: Vec<ArtifactView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FleetCounts {
    pub machines: usize,
    pub profiles: usize,
    pub accounts: usize,
    pub active_leases: usize,
    pub metrics: usize,
    pub events: usize,
    pub artifacts: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineView {
    pub machine_id: String,
    pub hostname: String,
    pub status: String,
    pub agent_grpc_addr: String,
    pub tailscale_host: String,
    pub last_heartbeat_unix_ms: i64,
    pub labels: Vec<KeyValueView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileView {
    pub profile_id: String,
    pub machine_id: String,
    pub display_name: String,
    pub status: String,
    pub cdp_url: String,
    pub cdp_port: i32,
    pub last_seen_unix_ms: i64,
    pub accounts: Vec<AccountView>,
    pub labels: Vec<KeyValueView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    pub account_id: String,
    pub platform: String,
    pub handle: String,
    pub health: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountBindingView {
    pub binding_id: String,
    pub profile_id: String,
    pub machine_id: String,
    pub platform: String,
    pub account_id: String,
    pub handle: String,
    pub account_health: String,
    pub profile_status: String,
    pub agent_grpc_addr: String,
    pub cdp_url: String,
    pub last_seen_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaseView {
    pub lease_id: String,
    pub profile_id: String,
    pub machine_id: String,
    pub client_id: String,
    pub purpose: String,
    pub expires_at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricView {
    pub name: String,
    pub value: f64,
    pub bucket_start_unix_ms: i64,
    pub machine_id: String,
    pub profile_id: String,
    pub platform: String,
    pub domain: String,
    pub action: String,
    pub status_class: String,
    pub error_class: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventView {
    pub event_type: String,
    pub severity: String,
    pub observed_at_unix_ms: i64,
    pub machine_id: String,
    pub profile_id: String,
    pub message: String,
    pub attributes: Vec<KeyValueView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactView {
    pub artifact_id: String,
    pub machine_id: String,
    pub profile_id: String,
    pub lease_id: String,
    pub original_filename: String,
    pub content_type: String,
    pub purpose: String,
    pub status: String,
    pub size_bytes: i64,
    pub expires_at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct KeyValueView {
    pub key: String,
    pub value: String,
}

pub async fn serve(addr: SocketAddr, service: ControllerService) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/snapshot", get(snapshot))
        .route("/healthz", get(healthz))
        .with_state(service);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "starting global controller web ui");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn snapshot(State(service): State<ControllerService>) -> Json<ControllerWebSnapshot> {
    Json(service.web_snapshot())
}

async fn healthz() -> impl IntoResponse {
    "ok"
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Browser Control Plane</title>
  <style>
    :root {
      color-scheme: light;
      --bg: #f7f8fa;
      --panel: #ffffff;
      --line: #d9dee7;
      --text: #171a1f;
      --muted: #5c6472;
      --accent: #146b5f;
      --warn: #9a5b00;
      --bad: #b42318;
      --good: #067647;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      background: var(--bg);
      color: var(--text);
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      font-size: 14px;
    }
    header {
      border-bottom: 1px solid var(--line);
      background: var(--panel);
      padding: 14px 20px;
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 16px;
      position: sticky;
      top: 0;
      z-index: 2;
    }
    h1 { margin: 0; font-size: 18px; font-weight: 650; }
    main { padding: 18px 20px 28px; max-width: 1440px; margin: 0 auto; }
    .muted { color: var(--muted); }
    .summary {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
      gap: 10px;
      margin-bottom: 18px;
    }
    .metric {
      background: var(--panel);
      border: 1px solid var(--line);
      border-radius: 8px;
      padding: 12px;
    }
    .metric strong { display: block; font-size: 24px; margin-top: 4px; }
    section { margin-top: 18px; }
    h2 { font-size: 15px; margin: 0 0 8px; font-weight: 650; }
    .table-wrap {
      background: var(--panel);
      border: 1px solid var(--line);
      border-radius: 8px;
      overflow: auto;
    }
    table { border-collapse: collapse; width: 100%; min-width: 860px; }
    th, td {
      text-align: left;
      border-bottom: 1px solid var(--line);
      padding: 9px 10px;
      white-space: nowrap;
      vertical-align: top;
    }
    th { color: var(--muted); font-weight: 600; background: #fbfcfe; }
    tr:last-child td { border-bottom: 0; }
    code {
      font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      font-size: 12px;
    }
    .pill {
      display: inline-flex;
      align-items: center;
      border: 1px solid var(--line);
      border-radius: 999px;
      padding: 2px 8px;
      font-size: 12px;
      background: #fbfcfe;
    }
    .available, .online, .info { color: var(--good); }
    .leased, .warn, .degraded { color: var(--warn); }
    .broken, .quarantined, .offline, .error { color: var(--bad); }
    button {
      border: 1px solid var(--line);
      background: var(--panel);
      color: var(--text);
      border-radius: 6px;
      padding: 7px 10px;
      cursor: pointer;
    }
    button:hover { border-color: var(--accent); }
    .toolbar { display: flex; align-items: center; gap: 10px; }
    .empty { padding: 14px; color: var(--muted); }
  </style>
</head>
<body>
  <header>
    <div>
      <h1>Browser Control Plane</h1>
      <div class="muted" id="generated">Loading fleet snapshot</div>
    </div>
    <div class="toolbar">
      <button id="refresh">Refresh</button>
    </div>
  </header>
  <main>
    <div class="summary" id="summary"></div>
    <section>
      <h2>Machines</h2>
      <div class="table-wrap"><table id="machines"></table></div>
    </section>
    <section>
      <h2>Profiles</h2>
      <div class="table-wrap"><table id="profiles"></table></div>
    </section>
    <section>
      <h2>Accounts</h2>
      <div class="table-wrap"><table id="accounts"></table></div>
    </section>
    <section>
      <h2>Active Leases</h2>
      <div class="table-wrap"><table id="leases"></table></div>
    </section>
    <section>
      <h2>Recent Events</h2>
      <div class="table-wrap"><table id="events"></table></div>
    </section>
  </main>
  <script>
    const fmtTime = (ms) => ms ? new Date(ms).toLocaleString() : "";
    const text = (value) => value == null ? "" : String(value);
    const cls = (value) => text(value).toLowerCase().replace(/[^a-z0-9_-]/g, "-");
    const esc = (value) => text(value)
      .replaceAll("&", "&amp;")
      .replaceAll("<", "&lt;")
      .replaceAll(">", "&gt;")
      .replaceAll('"', "&quot;");
    const pill = (value) => `<span class="pill ${cls(value)}">${esc(value)}</span>`;
    const labels = (items) => (items || []).map((item) => `<code>${esc(item.key)}=${esc(item.value)}</code>`).join(" ");
    const renderTable = (id, columns, rows) => {
      const el = document.getElementById(id);
      if (!rows.length) {
        el.innerHTML = `<tbody><tr><td class="empty">No records</td></tr></tbody>`;
        return;
      }
      el.innerHTML = `<thead><tr>${columns.map((c) => `<th>${esc(c.label)}</th>`).join("")}</tr></thead><tbody>` +
        rows.map((row) => `<tr>${columns.map((c) => `<td>${c.render(row)}</td>`).join("")}</tr>`).join("") +
        `</tbody>`;
    };
    async function refresh() {
      const res = await fetch("/api/snapshot", { cache: "no-store" });
      if (!res.ok) throw new Error(`snapshot failed: ${res.status}`);
      const data = await res.json();
      document.getElementById("generated").textContent = `Generated ${fmtTime(data.generated_at_unix_ms)}`;
      document.getElementById("summary").innerHTML = [
        ["Machines", data.counts.machines],
        ["Profiles", data.counts.profiles],
        ["Accounts", data.counts.accounts],
        ["Active leases", data.counts.active_leases],
        ["Metrics", data.counts.metrics],
        ["Events", data.counts.events],
        ["Artifacts", data.counts.artifacts],
      ].map(([label, value]) => `<div class="metric"><span class="muted">${label}</span><strong>${value}</strong></div>`).join("");
      renderTable("machines", [
        { label: "Machine", render: (r) => `<code>${esc(r.machine_id)}</code>` },
        { label: "Status", render: (r) => pill(r.status) },
        { label: "Agent", render: (r) => `<code>${esc(r.agent_grpc_addr)}</code>` },
        { label: "Hostname", render: (r) => esc(r.hostname) },
        { label: "Heartbeat", render: (r) => fmtTime(r.last_heartbeat_unix_ms) },
        { label: "Labels", render: (r) => labels(r.labels) },
      ], data.machines);
      renderTable("profiles", [
        { label: "Profile", render: (r) => `<code>${esc(r.profile_id)}</code>` },
        { label: "Machine", render: (r) => `<code>${esc(r.machine_id)}</code>` },
        { label: "Status", render: (r) => pill(r.status) },
        { label: "Accounts", render: (r) => r.accounts.map((a) => `${pill(a.platform)} <code>${esc(a.account_id)}</code>`).join("<br>") },
        { label: "CDP", render: (r) => `<code>${esc(r.cdp_url)}</code>` },
        { label: "Last seen", render: (r) => fmtTime(r.last_seen_unix_ms) },
      ], data.profiles);
      renderTable("accounts", [
        { label: "Account", render: (r) => `<code>${esc(r.account_id)}</code>` },
        { label: "Platform", render: (r) => pill(r.platform) },
        { label: "Profile", render: (r) => `<code>${esc(r.profile_id)}</code>` },
        { label: "Machine", render: (r) => `<code>${esc(r.machine_id)}</code>` },
        { label: "Status", render: (r) => pill(r.profile_status) },
        { label: "Health", render: (r) => esc(r.account_health) },
      ], data.accounts);
      renderTable("leases", [
        { label: "Lease", render: (r) => `<code>${esc(r.lease_id)}</code>` },
        { label: "Profile", render: (r) => `<code>${esc(r.profile_id)}</code>` },
        { label: "Machine", render: (r) => `<code>${esc(r.machine_id)}</code>` },
        { label: "Client", render: (r) => esc(r.client_id) },
        { label: "Purpose", render: (r) => esc(r.purpose) },
        { label: "Expires", render: (r) => fmtTime(r.expires_at_unix_ms) },
      ], data.leases);
      renderTable("events", [
        { label: "Time", render: (r) => fmtTime(r.observed_at_unix_ms) },
        { label: "Severity", render: (r) => pill(r.severity) },
        { label: "Type", render: (r) => `<code>${esc(r.event_type)}</code>` },
        { label: "Machine", render: (r) => `<code>${esc(r.machine_id)}</code>` },
        { label: "Profile", render: (r) => `<code>${esc(r.profile_id)}</code>` },
        { label: "Message", render: (r) => esc(r.message) },
      ], data.events.slice(0, 50));
    }
    document.getElementById("refresh").addEventListener("click", () => refresh().catch(console.error));
    refresh().catch((error) => {
      document.getElementById("generated").textContent = error.message;
    });
  </script>
</body>
</html>
"#;
