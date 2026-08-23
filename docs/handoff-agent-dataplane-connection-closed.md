# Handoff: BCP browser data-plane fails "Connection closed"

**Date:** 2026-08-22
**Severity:** blocks all browser operations through BCP (control plane + browsers
are otherwise healthy).
**Owner:** unassigned — handoff for a fixing agent.

> **ROOT CAUSE FOUND (2026-08-23) — it is a bcp agent bug, not networking.**
> `RealPwrightGateway` caches one CDP connection per profile (`self.active`) and
> `ensure_browser` returns the cached handle **without health-checking it**. When a
> transient `Connection reset by peer` killed the cached websocket (first seen
> 2026-08-08), the agent kept handing back the **dead** handle, so every subsequent
> browser op failed `Connection closed` indefinitely — the agent never reconnected
> (it went silent for 11 days). **Restarting the agent pod fixes it immediately**
> (verified: after `rollout restart`, `eval location.host` returns `www.google.com`).
>
> **The permanent fix is a code change**: detect a dead cached CDP connection and
> reconnect — either health-check in `ensure_browser`, or evict-and-retry-once when a
> browser op returns a closed/reset error. See `crates/bcp-core/src/pwright.rs`
> (`RealPwrightGateway::ensure_browser` / `page_for_profile`).
>
> DISPROVEN hypotheses (do not re-chase): (#1) Origin/header rejection — the browser
> ws accepts any Origin/Host. (earlier MTU/pod-egress theory) — a faithful drive
> sequence (createTarget/attach/evaluate) works fine from a pod; the misleading
> "black-hole" was a `Target.setDiscoverTargets` flood that hangs from ANY client,
> including off-cluster. Network/pod egress is NOT the cause.
>
> Separately still true: `armwinroot` profile points at dead `:9227` (fix the port);
> `acquire` vs `eval` resolve accounts differently (secondary quirk).

## One-line

The BCP control plane routes fine and the underlying Chrome browsers are alive and
drivable via **direct** CDP, but **every** browser operation *through a BCP agent*
fails with `FailedPrecondition: browser operation failed for profile '<id>':
Connection closed`. So the break is in the **agent data plane** (agent → Chrome
CDP driving / pwright bridge / raw-CDP proxy), not in the controller or the
browsers.

## Deployment (all on the tailnet, k3s)

| Component | Node | Endpoint |
|---|---|---|
| Global controller (gRPC) | `browser-control-plane-k3spod-yuacx` (100.91.186.14) | `http://…:7000` |
| Controller dashboard | same | `http://…:7080` (`/healthz` = ok, `/api/snapshot`) |
| Agent — alienware | `browser-control-plane-agent-alienware-k3spod-yuacx` | `http://…:7100` |
| Agent — armwinroot | `browser-control-plane-agent-armwinroot-k3spod-yuacx` | `http://…:7100` |
| Agent — ubuntu-gui | `browser-control-plane-agent-ubuntu-gui-k3spod-yuacx` | `http://…:7100` |

Snapshot counts: 3 machines online, 11 profiles (all `available`), 13 accounts
(8 youtube, 4 wsj, 1 x — **no douyin**), 0 active leases.

Machines/passwords: SSH user/pass is `cyuan`/`cyuan` on the host boxes
(alienware-win-yuacx 100.112.45.12, armwinroot-yuacx 100.77.171.126). k3s pods:
get logs via the k3s host (`k3shost-yuacx` 100.110.27.91 / `k3svm-yuacx`).

## Evidence

### 1. Control plane is healthy
```
bcp-client --controller http://…:7000 machines     # -> 3
curl http://100.91.186.14:7080/api/snapshot         # -> full fleet, all profiles "available"
```

### 2. The underlying Chrome browsers are ALIVE and drivable *directly*
Direct CDP websocket works (Browser.getVersion returns) on the live profiles, e.g.
`alienware-win-yuacx:9401` (Chrome/151.0.7922.140) and
`ubuntu-gui-browser-yuacx:9223` (Chrome/151.0.7922.108). Connected with
`suppress_origin=True` (no Origin header).

### 3. Profile → CDP endpoint liveness (direct `GET /json/version`)
```
alienware-win-yuacx:9222   200      alienware-win-yuacx:9401  200 (ws OK)
alienware-win-yuacx:9404   200      alienware-win-yuacx:9402  200
armwinroot-yuacx:9227      502  <-- both armwinroot profiles point here; upstream Chrome down
ubuntu-gui-browser-yuacx:9223  200 (ws OK)
ubuntu-gui-browser-arm2:9223   502  <-- note: arm2 host is the infra-default browser, but NOT what BCP points to
```

### 4. Through BCP it FAILS
```
bcp-client --controller http://…:7000 eval \
  --platform youtube --account-id shuoze --expression "location.host"
# -> FailedPrecondition: browser operation failed for profile 'shuoze': Connection closed
```
Same "Connection closed" for every youtube profile tried (shuoze, yuacx2,
browser-data, alienware-browser-user1, alienware-browser-user4,
alienware-yuanchenxi2025) — including ones whose CDP is 200 AND whose ws works
directly.

Secondary quirk: `acquire --platform youtube --account-id alienware-browser-user1`
returns `NotFound: no available profile matched request`, even though `eval` with
the same account-id *did* route to that profile (reached "Connection closed"). So
`acquire` vs `eval` resolve accounts differently — check account_id vs handle vs
profile_id matching.

## Leading hypotheses (ranked)

1. **Chrome `--remote-allow-origins` rejects the agent's ws upgrade.** Direct ws
   worked only with *no* Origin header (`suppress_origin`). The agent's CDP client
   / pwright bridge likely sends an `Origin`/`Host` that Chrome rejects, so Chrome
   accepts `/json/version` (HTTP) but closes the websocket → "Connection closed".
   Fix candidates: launch Chrome with `--remote-allow-origins=*`, or make the
   agent/pwright omit Origin (mirror `suppress_origin`) and rewrite Host to match
   Chrome's expectation. See `crates/bcp-agent/src/proxy.rs` (host rewrite) and
   pwright `crates/pwright-bridge/src/browser.rs` (`ws://…host rewrite when
   connecting through a proxy`).
2. **Stale profile → port registry.** `armwinroot` profiles point to `:9227`
   which is 502 (that Chrome is gone); the live armwinroot browsers are elsewhere
   (the BitBrowser instances on 9401–9411 nginx). Reconcile the profile registry
   with the actually-running Chrome ports per machine.
3. **Agent → Chrome network path.** Agent pods reach Chrome via the tailnet
   hostname; confirm the pod can open the *websocket* (not just HTTP) to the host
   Chrome (MagicDNS + port reachable from inside the pod's netns).

## What to check first

1. `kubectl logs` the three `browser-control-plane-agent-*` pods while running one
   `bcp-client eval …` — capture the exact error at the moment of "Connection
   closed" (which ws URL, which header, Chrome's close reason).
2. Inspect how the agent opens the CDP websocket (Origin header? Host rewrite?)
   vs. the working direct connect (`suppress_origin=True`, no Origin).
3. Check the Chrome launch flags on each host for `--remote-allow-origins`.
4. Fix the `armwinroot` profile port (9227 → the live port) and re-test.

## Repro (fast)

```bash
CTRL=http://browser-control-plane-k3spod-yuacx.tail8f3b66.ts.net:7000
bcp-client --controller $CTRL machines                      # 3  (control plane ok)
bcp-client --controller $CTRL eval --platform youtube \
  --account-id shuoze --expression "location.host"          # Connection closed  (BUG)
# prove the browser itself is fine (direct CDP ws):
python - <<'PY'
import json,urllib.request; from websocket import create_connection
b="http://ubuntu-gui-browser-yuacx.tail8f3b66.ts.net:9223"
u=json.load(urllib.request.urlopen(b+"/json/version"))["webSocketDebuggerUrl"]
ws=create_connection(u,timeout=8,suppress_origin=True)
ws.send(json.dumps({"id":1,"method":"Browser.getVersion"})); print(ws.recv()[:80])
PY
```

## Why this matters

The media-downloader "catalog pipeline" (discovery service) is designed to lease
browsers via BCP + drive them with `pwright-bridge`. It is blocked on this. Once
the data plane drives browsers again, the discovery service can go through BCP;
until then it can only use a direct-CDP dev fallback.
See `~/projects/personal/media-downloader/docs/design/11-decoupled-discovery-download.md`.
