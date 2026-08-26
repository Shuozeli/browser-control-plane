//! Raw CDP transparent proxy.
//!
//! Chrome binds its DevTools endpoint to localhost only. This proxy re-exposes
//! it on the agent's network interface, lease-gated, so external DevTools
//! clients (pwright, puppeteer, chrome-devtools-mcp) can drive the remote
//! browser with the *full* CDP surface — everything the semantic gateway does
//! not hand-port (emulation, dialogs, fine-grained network, multi-tab, httpOnly
//! cookies, …).
//!
//! Two paths, both under `/{profile}`:
//!   * HTTP `/json`, `/json/list`, `/json/version`, `/json/protocol` — fetched
//!     from local Chrome; `webSocketDebuggerUrl` is rewritten so it points back
//!     at this proxy and carries the caller's lease token.
//!   * WebSocket `/devtools/{*rest}` — lease-checked, then relayed frame-for-frame
//!     to Chrome. The DevTools JSON protocol rides this untouched.
//!
//! Out-of-band file transfer is *not* carried here: `DOM.setFileInputFiles`
//! needs a machine-local path (use `UploadArtifact` + `SetInputFiles`) and
//! downloads land on the machine's disk (use `setDownloadBehavior` + retrieval).

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message as TMessage;

use crate::AgentService;

#[derive(Clone)]
struct ProxyState {
    service: AgentService,
    /// host:port this proxy is reachable at, used to rewrite ws URLs.
    public_host: String,
}

/// `?bcpLease=<lease_id>:<fencing_token>` — the lease credential. The profile is
/// already in the path, so only the id + fencing token travel in the query.
#[derive(Debug, Deserialize, Default)]
struct LeaseQuery {
    #[serde(default, rename = "bcpLease")]
    bcp_lease: String,
}

/// Extract the `<lease_id>:<fencing_token>` credential from either the
/// `?bcpLease=` query (preferred, so rewritten ws URLs are self-contained) or an
/// `Authorization: Bearer <lease_id>:<fencing_token>` header (for DevTools
/// clients that only do `/json` discovery and inject headers). The bare token
/// without a `Bearer` scheme is also accepted.
fn extract_token(headers: &HeaderMap, query: &LeaseQuery) -> Option<String> {
    if !query.bcp_lease.is_empty() {
        return Some(query.bcp_lease.clone());
    }
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .unwrap_or(value)
        .trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Build the proxy router. `public_host` is the `host:port` external clients use
/// to reach this proxy (so rewritten ws URLs are dialable from off-box).
pub fn router(service: AgentService, public_host: String) -> Router {
    let state = Arc::new(ProxyState {
        service,
        public_host,
    });
    Router::new()
        .route("/{profile}/json", get(json_list))
        .route("/{profile}/json/list", get(json_list))
        .route("/{profile}/json/version", get(json_version))
        .route("/{profile}/json/protocol", get(json_protocol))
        .route("/{profile}/json/new", get(json_new).put(json_new))
        .route("/{profile}/devtools/{*rest}", get(ws_upgrade))
        .with_state(state)
}

fn unauthorized() -> Response {
    (StatusCode::FORBIDDEN, "invalid or missing lease").into_response()
}

/// Validate a `<lease_id>:<fencing_token>` credential against the agent's
/// install map for `profile`.
fn authorize(state: &ProxyState, profile: &str, token: &str) -> bool {
    match token.split_once(':') {
        Some((lease_id, fencing)) => state.service.check_lease(lease_id, profile, fencing),
        None => false,
    }
}

/// Strip the scheme from a Chrome HTTP base URL, yielding `host:port`.
fn host_port(cdp_http_url: &str) -> &str {
    cdp_http_url
        .trim_end_matches('/')
        .trim_start_matches("http://")
        .trim_start_matches("https://")
}

async fn json_list(
    State(state): State<Arc<ProxyState>>,
    Path(profile): Path<String>,
    headers: HeaderMap,
    Query(query): Query<LeaseQuery>,
) -> Response {
    proxy_json(&state, &profile, &headers, &query, "list").await
}

async fn json_version(
    State(state): State<Arc<ProxyState>>,
    Path(profile): Path<String>,
    headers: HeaderMap,
    Query(query): Query<LeaseQuery>,
) -> Response {
    proxy_json(&state, &profile, &headers, &query, "version").await
}

async fn json_protocol(
    State(state): State<Arc<ProxyState>>,
    Path(profile): Path<String>,
    headers: HeaderMap,
    Query(query): Query<LeaseQuery>,
) -> Response {
    // The protocol descriptor has no ws URLs; pass it through verbatim.
    proxy_json(&state, &profile, &headers, &query, "protocol").await
}

/// Fetch `/json/<suffix>` from Chrome, rewrite any `webSocketDebuggerUrl` to
/// point back at this proxy (carrying the lease), and return the JSON.
async fn proxy_json(
    state: &ProxyState,
    profile: &str,
    headers: &HeaderMap,
    query: &LeaseQuery,
    suffix: &str,
) -> Response {
    let Some(token) = extract_token(headers, query) else {
        return unauthorized();
    };
    if !authorize(state, profile, &token) {
        return unauthorized();
    }
    let Some(base) = state.service.profile_cdp_url(profile) else {
        return (StatusCode::NOT_FOUND, "unknown profile").into_response();
    };
    let url = format!("{}/json/{suffix}", base.trim_end_matches('/'));
    let body = match reqwest::get(&url).await {
        Ok(response) => match response.text().await {
            Ok(text) => text,
            Err(error) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("chrome read error: {error}"),
                )
                    .into_response();
            }
        },
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("chrome fetch error: {error}"),
            )
                .into_response();
        }
    };

    let rewritten = match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(mut value) => {
            rewrite_ws_urls(&mut value, profile, &token, &state.public_host);
            serde_json::to_string(&value).unwrap_or(body)
        }
        // /json/protocol and any non-JSON payload pass through unchanged.
        Err(_) => body,
    };
    ([(header::CONTENT_TYPE, "application/json")], rewritten).into_response()
}

/// Split a `bcpLease=<lease>` credential out of Chrome's `/json/new` query,
/// returning `(lease_token, remaining_query)`. Chrome treats the whole query as
/// the URL to open, so the lease must be removed before forwarding; callers may
/// instead pass the lease via the `Authorization` header and leave the query as
/// the target URL.
fn split_lease_from_query(raw: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(raw) = raw else {
        return (None, None);
    };
    let mut token = None;
    let mut rest: Vec<&str> = Vec::new();
    for part in raw.split('&') {
        if let Some(value) = part.strip_prefix("bcpLease=") {
            // A literal ':' in the token is often percent-encoded in a query.
            token = Some(value.replace("%3A", ":").replace("%3a", ":"));
        } else if !part.is_empty() {
            rest.push(part);
        }
    }
    let remaining = (!rest.is_empty()).then(|| rest.join("&"));
    (token, remaining)
}

/// `PUT|GET /{profile}/json/new?<url>` — create a new target on Chrome and
/// return its rewritten `webSocketDebuggerUrl`. Chrome (modern) requires PUT.
async fn json_new(
    State(state): State<Arc<ProxyState>>,
    Path(profile): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let (query_token, target_query) = split_lease_from_query(raw.as_deref());
    let token = extract_token(&headers, &LeaseQuery::default()).or(query_token);
    let Some(token) = token else {
        return unauthorized();
    };
    if !authorize(&state, &profile, &token) {
        return unauthorized();
    }
    let Some(base) = state.service.profile_cdp_url(&profile) else {
        return (StatusCode::NOT_FOUND, "unknown profile").into_response();
    };
    let base = base.trim_end_matches('/');
    let url = match target_query.as_deref().filter(|query| !query.is_empty()) {
        Some(query) => format!("{base}/json/new?{query}"),
        None => format!("{base}/json/new"),
    };
    let response = match reqwest::Client::new().put(&url).send().await {
        Ok(response) => response,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("chrome /json/new error: {error}"),
            )
                .into_response();
        }
    };
    let body = match response.text().await {
        Ok(text) => text,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("chrome read error: {error}"),
            )
                .into_response();
        }
    };
    let rewritten = match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(mut value) => {
            rewrite_ws_urls(&mut value, &profile, &token, &state.public_host);
            serde_json::to_string(&value).unwrap_or(body)
        }
        Err(_) => body,
    };
    ([(header::CONTENT_TYPE, "application/json")], rewritten).into_response()
}

/// Recursively rewrite every `webSocketDebuggerUrl` field so it targets this
/// proxy: `ws://<public_host>/<profile>/devtools/...?bcpLease=<lease>`.
fn rewrite_ws_urls(value: &mut serde_json::Value, profile: &str, lease: &str, public_host: &str) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(ws)) = map.get_mut("webSocketDebuggerUrl") {
                *ws = rewrite_one(ws, profile, lease, public_host);
            }
            for (_, child) in map.iter_mut() {
                rewrite_ws_urls(child, profile, lease, public_host);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                rewrite_ws_urls(item, profile, lease, public_host);
            }
        }
        _ => {}
    }
}

fn rewrite_one(original: &str, profile: &str, lease: &str, public_host: &str) -> String {
    match original.find("/devtools/") {
        Some(index) => {
            let tail = &original[index..];
            format!("ws://{public_host}/{profile}{tail}?bcpLease={lease}")
        }
        None => original.to_string(),
    }
}

async fn ws_upgrade(
    State(state): State<Arc<ProxyState>>,
    Path((profile, rest)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<LeaseQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(token) = extract_token(&headers, &query) else {
        return unauthorized();
    };
    if !authorize(&state, &profile, &token) {
        return unauthorized();
    }
    let Some(base) = state.service.profile_cdp_url(&profile) else {
        return (StatusCode::NOT_FOUND, "unknown profile").into_response();
    };
    let chrome_ws = format!("ws://{}/devtools/{rest}", host_port(&base));
    upgrade.on_upgrade(move |socket| bridge(socket, chrome_ws))
}

/// Relay frames between the client WebSocket and Chrome's WebSocket until either
/// side closes.
async fn bridge(client: WebSocket, chrome_ws: String) {
    let chrome = match tokio_tungstenite::connect_async(&chrome_ws).await {
        Ok((stream, _response)) => stream,
        Err(error) => {
            tracing::warn!(%chrome_ws, %error, "cdp proxy: chrome ws connect failed");
            return;
        }
    };
    let (mut chrome_tx, mut chrome_rx) = chrome.split();
    let (mut client_tx, mut client_rx) = client.split();

    // client -> chrome. On any end (client Close, read error, or stream end) we
    // send a Close frame and close the upstream sink, so a shared long-lived
    // Chrome is never left half-open (which resets the *next* session).
    let to_chrome = async {
        loop {
            match client_rx.next().await {
                Some(Ok(message)) => {
                    let forwarded = match message {
                        Message::Text(text) => TMessage::Text(text.as_str().into()),
                        Message::Binary(bytes) => TMessage::Binary(bytes.to_vec().into()),
                        Message::Ping(bytes) => TMessage::Ping(bytes.to_vec().into()),
                        Message::Pong(bytes) => TMessage::Pong(bytes.to_vec().into()),
                        Message::Close(_) => break,
                    };
                    if let Err(error) = chrome_tx.send(forwarded).await {
                        tracing::debug!(%error, "cdp proxy: forward to chrome failed");
                        break;
                    }
                }
                Some(Err(error)) => {
                    tracing::debug!(%error, "cdp proxy: client ws read error");
                    break;
                }
                None => break,
            }
        }
        let _ = chrome_tx.send(TMessage::Close(None)).await;
        let _ = chrome_tx.close().await;
    };

    // chrome -> client. Same clean-close discipline toward the client.
    let to_client = async {
        loop {
            match chrome_rx.next().await {
                Some(Ok(message)) => {
                    let forwarded = match message {
                        TMessage::Text(text) => Message::Text(text.to_string().into()),
                        TMessage::Binary(bytes) => Message::Binary(bytes.to_vec().into()),
                        TMessage::Ping(bytes) => Message::Ping(bytes.to_vec().into()),
                        TMessage::Pong(bytes) => Message::Pong(bytes.to_vec().into()),
                        TMessage::Close(_) => break,
                        TMessage::Frame(_) => continue,
                    };
                    if let Err(error) = client_tx.send(forwarded).await {
                        tracing::debug!(%error, "cdp proxy: forward to client failed");
                        break;
                    }
                }
                Some(Err(error)) => {
                    tracing::debug!(%error, "cdp proxy: chrome ws read error");
                    break;
                }
                None => break,
            }
        }
        let _ = client_tx.send(Message::Close(None)).await;
        let _ = client_tx.close().await;
    };

    tokio::select! {
        _ = to_chrome => {}
        _ = to_client => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lease_from_query_separates_token_and_target() {
        // Arrange / Act / Assert: lease (percent-encoded ':') + target URL.
        let (token, target) =
            split_lease_from_query(Some("bcpLease=lease1%3Atok1&https://example.com"));
        assert_eq!(token.as_deref(), Some("lease1:tok1"));
        assert_eq!(target.as_deref(), Some("https://example.com"));

        // lease only (open about:blank)
        let (token, target) = split_lease_from_query(Some("bcpLease=lease1:tok1"));
        assert_eq!(token.as_deref(), Some("lease1:tok1"));
        assert_eq!(target, None);

        // target only (lease supplied via Authorization header)
        let (token, target) = split_lease_from_query(Some("https://example.com"));
        assert_eq!(token, None);
        assert_eq!(target.as_deref(), Some("https://example.com"));

        // nothing
        assert_eq!(split_lease_from_query(None), (None, None));
    }

    #[test]
    fn rewrite_one_points_ws_url_at_proxy_with_lease() {
        // Arrange
        let original = "ws://localhost:9222/devtools/page/ABC123";

        // Act
        let rewritten = rewrite_one(original, "youtube-main", "lease1:tok1", "agent.ts.net:7101");

        // Assert
        assert_eq!(
            rewritten,
            "ws://agent.ts.net:7101/youtube-main/devtools/page/ABC123?bcpLease=lease1:tok1"
        );
    }

    #[test]
    fn rewrite_ws_urls_walks_nested_targets() {
        // Arrange
        let mut value = serde_json::json!([
            { "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/A" },
            { "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/B" }
        ]);

        // Act
        rewrite_ws_urls(&mut value, "p1", "l:t", "host:7101");

        // Assert
        assert_eq!(
            value[0]["webSocketDebuggerUrl"],
            "ws://host:7101/p1/devtools/page/A?bcpLease=l:t"
        );
        assert_eq!(
            value[1]["webSocketDebuggerUrl"],
            "ws://host:7101/p1/devtools/page/B?bcpLease=l:t"
        );
    }

    #[test]
    fn host_port_strips_scheme_and_trailing_slash() {
        // Arrange / Act / Assert
        assert_eq!(host_port("http://127.0.0.1:9222/"), "127.0.0.1:9222");
        assert_eq!(host_port("https://chrome.local:9333"), "chrome.local:9333");
    }

    #[test]
    fn extract_token_prefers_query() {
        // Arrange
        let headers = HeaderMap::new();
        let query = LeaseQuery {
            bcp_lease: "lease-7:fence-9".to_string(),
        };

        // Act
        let token = extract_token(&headers, &query);

        // Assert
        assert_eq!(token.as_deref(), Some("lease-7:fence-9"));
    }

    #[test]
    fn extract_token_reads_bearer_header_when_no_query() {
        // Arrange
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer lease-7:fence-9".parse().unwrap(),
        );
        let query = LeaseQuery::default();

        // Act
        let token = extract_token(&headers, &query);

        // Assert
        assert_eq!(token.as_deref(), Some("lease-7:fence-9"));
    }

    #[test]
    fn extract_token_accepts_bare_header_without_scheme() {
        // Arrange
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "lease-7:fence-9".parse().unwrap());
        let query = LeaseQuery::default();

        // Act
        let token = extract_token(&headers, &query);

        // Assert
        assert_eq!(token.as_deref(), Some("lease-7:fence-9"));
    }

    #[test]
    fn extract_token_none_when_absent() {
        // Arrange
        let headers = HeaderMap::new();
        let query = LeaseQuery::default();

        // Act
        let token = extract_token(&headers, &query);

        // Assert
        assert_eq!(token, None);
    }
}
