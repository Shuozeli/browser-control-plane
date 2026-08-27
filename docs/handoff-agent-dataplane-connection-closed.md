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
>
> **UPDATE 2026-08-27 — there are TWO bugs; the stale-cache fix is deployed but a
> second one remains. See the "Update (2026-08-27)" section at the bottom.** The
> follow-up notes below (2026-08-23) predate the fix and are partly stale.

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

## Follow-up findings (2026-08-23, from the media-downloader side)

Re-tested to wire the catalog pipeline's discovery onto BCP. State:

1. **Data plane is only PARTIALLY recovered.** `eval youtube/alienware-browser-user1`
   now returns `www.google.com` (alienware agent was restarted ✅). But
   `youtube/shuoze` and `youtube/yuacx2` (the **ubuntu-gui** agent) still return
   `Connection closed` — that agent still holds the stale cached CDP handle and
   needs a `rollout restart` (or the reconnect fix). `armwinroot` profiles still
   point at dead `:9227`.

2. **BLOCKER for external raw-CDP clients: the agent raw-CDP proxy port is not
   exposed on the tailnet.** The agent serves the raw CDP proxy on `grpc_port + 1`
   (`:7101`, `spawn_cdp_proxy` in `bcp-agent/src/main.rs`), but only `:7100`
   (gRPC) and `:7080` (dashboard) are reachable — `:7101` times out on both the
   pod IP (100.103.239.55) and MagicDNS. Verified: `acquire` + `install` succeed
   and the lease is accepted, but `GET http://<agent>:7101/<profile>/json/version?bcpLease=…`
   is unreachable. The discovery scraper drives **raw CDP** (needs `/json/new` +
   ws + `Network.getResponseBody` for Douyin), which the semantic gateway
   (`Evaluate`/`RunScript`) can't fully cover — so it needs `:7101`.
   **Ask: expose each agent's `:7101` as a tailnet-reachable port** (k8s Service /
   hostPort / `BCP_AGENT_PROXY_ADDR` bound to the pod's tailnet interface), the
   same way `:7100` is. Once `:7101` is reachable, the pipeline's `acquire()` can
   lease via bcp-proto and drive the leased raw-CDP proxy directly.

## Actions taken from the media-downloader side (2026-08-23)

- **Exposed `:7101` on the alienware agent** so external raw-CDP clients can reach
  the proxy: `kubectl -n dragb patch svc browser-control-plane-agent-alienware`
  adding a `cdp-proxy` port `7101` (the `tailscale.com/expose` proxy bridges it).
  **This is a live patch, NOT in a source manifest — add port 7101 to the agent
  Service manifest(s) so it survives redeploys, and do the same for armwinroot /
  ubuntu-gui.** Verified: raw CDP now drives through
  `http://<agent>:7101/<profile>/json/list?bcpLease=…` (client must rewrite the
  advertised `0.0.0.0` ws host to the dialed host).
- Confirmed the **raw-CDP proxy path hits the same agent bug**: a lease's first
  scrape works (drove a real page, extracted 25–30 video tiles), but subsequent
  ws sessions get `Connection reset without closing handshake`. So the
  `RealPwrightGateway` stale/again-dead cached-handle issue affects the raw proxy
  too — the permanent reconnect/health-check fix is still needed. `eval` currently
  works on `alienware-browser-user1 / -yuanchenxi2025 / -yuanchenxi2026`; `user4`
  + `alienware` return "no available profile matched"; ubuntu-gui still stale.

## Why this matters

The media-downloader "catalog pipeline" (discovery service) is designed to lease
browsers via BCP + drive them with `pwright-bridge`. It is blocked on this. Once
the data plane drives browsers again, the discovery service can go through BCP;
until then it can only use a direct-CDP dev fallback.
See `~/projects/personal/media-downloader/docs/design/11-decoupled-discovery-download.md`.

## Update (2026-08-27): fixes deployed + second root cause reconciled

There are **two independent bugs**. The earlier notes conflated them.

### Bug 1 — stale cached CDP connection — FIXED and DEPLOYED
`RealPwrightGateway` reused a dead cached websocket forever (11-day wedge). Fixed
by evict-and-retry-once reconnect (`with_reconnect` / `is_connection_closed` in
`crates/bcp-core/src/pwright.rs`, commit `35e82d8`). Also shipped: Phase 8 raw-proxy
clean teardown + `/json/new` (`480f0c0`), a Dockerfile fix so the image actually
builds the agent with `--features real-pwright` (`7561bd7`). Built image
`5235f1ba` (tag `480f0c0…`), rolled onto **all three agents + the controller**
(pinned to the SHA tag). `eval location.host` returns `www.google.com`;
`ListMachineLeases` lease-sync now works (controller was stale and returned
`Unimplemented` until rolled).

### Bug 2 — pod -> tailnet egress cannot sustain HEAVY CDP — STILL OPEN
Reproduced firmly: a heavy sustained CDP stream (real page / event flood) from a
**k3s agent pod** to a browser host **resets/black-holes at ~10-16 KB**
(`Connection reset without closing handshake` / recv timeout), while the *same*
drive from a **normal tailnet host works with no reset** (a local `bcp-agent`
streamed a real youtube page for tens of seconds; a pod died at ~16 KB). Light
ops (`eval` on a light page) pass; heavy scraping does not. An MSS clamp on the
node's `tailscale0` did **not** move the cutoff, so the mechanism is some
tailscale-egress / long-lived-WS interaction from the pods, not simple segment
MTU. This — not Bug 1 — is what breaks real scraping and the raw-proxy sessions.

NB: my 2026-08-23 "MTU/pod-egress is a red herring" claim was wrong; it was based
on a *light* drive test that never pushed enough data to trip Bug 2.

**Fix for Bug 2: run each agent ON its browser host (`localhost -> Chrome`)** so
the heavy data plane never traverses the pod->tailnet path. Not yet done (spans
2 Windows hosts + 1 Linux). ubuntu-gui was brought up on the fixed image but,
being a pod, still hits Bug 2 on heavy CDP.

### Still open
- Bug 2 fix (on-host agents) — not started.
- `:7101` raw-proxy port is only a live `kubectl patch` on alienware — not in a
  source manifest, not on armwinroot/ubuntu-gui.
- `armwinroot` profile still points at dead `:9227`.
- Agent Deployments are pinned to the SHA tag (`480f0c0…`), not `:latest`.
