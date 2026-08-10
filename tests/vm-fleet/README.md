# VirtualBox VM Fleet Test

This exercises Browser Control Plane against a fleet where **each machine is a
real VirtualBox VM running a real headless Chrome** — not a container. It proves
the control plane routes leases and proxies browser work across genuinely
separate machines over a network where the controller and client cannot reach
the browsers' CDP ports directly (only the local machine controller can).

Unlike the Docker topologies in `../e2e`, which model machines as containers on
isolated Docker networks, this drives actual VMs via `VBoxManage` and SSH. It is
a **manual** test (needs a VirtualBox host); it is not part of CI.

## Topology

```
                 VirtualBox NAT network  bcpnet 10.0.9.0/24
  bcp-ctrl (10.0.9.11)   bcp-controller :7000  +  bcp-e2e driver
  bcp-a1   (10.0.9.12)   chromium :9222  +  bcp-agent :7100   (machine a1)
  bcp-a2   (10.0.9.13)   chromium :9222  +  bcp-agent :7100   (machine a2)
  bcp-a3   (10.0.9.14)   chromium :9222  +  bcp-agent :7100   (machine a3)
```

The controller and client never touch a browser's CDP port; they only speak
gRPC to the machine controller, which speaks CDP to its local Chrome. The VM
host reaches each guest by key-based SSH through NAT port-forwards
(`host:2211 -> 10.0.9.11:22`, etc.).

## Build the golden image (once)

1. Download the Ubuntu 24.04 cloud image and convert it to a VDI (VirtualBox
   reads qcow2 natively, so no `qemu-img` is needed on Windows):
   ```
   curl -L -o base.img \
     https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img
   VBoxManage clonemedium disk base.img golden.vdi --format VDI
   VBoxManage modifymedium disk golden.vdi --resize 20480
   ```
2. Fill in `cloud-init/user-data` with your SSH public key, build a NoCloud seed
   `.viso` (see `cloud-init/seed.viso`), and boot a `bcp-golden` VM with
   `golden.vdi` + the seed attached as a DVD, NIC1 on a NAT network `bcpnet`.
   cloud-init installs `snap chromium` and authorizes the key.
3. `scp` the release binaries (`bcp-controller`, `bcp-agent` built with
   `--features real-pwright`, `bcp-e2e`) into `/home/cyuan` on the golden VM.
4. Power it off, `VBoxManage snapshot bcp-golden take base`, then linked-clone:
   ```
   VBoxManage clonevm bcp-golden --snapshot base --options link --name bcp-ctrl --register
   # repeat for bcp-a1 / bcp-a2 / bcp-a3
   ```
   Give each clone its own seed `.viso` with a unique static IP
   (`cloud-init/network-config`) and add a NAT SSH port-forward, e.g.
   `VBoxManage natnetwork modify --netname bcpnet --port-forward-4 "sshctrl:tcp:[]:2211:[10.0.9.11]:22"`.

Notes:
- Under a Hyper-V backend (WSL2 present) VirtualBox loses nested virt but
  headless Chrome runs fine; use a **NAT Network**, not host-only.
- A linked clone occasionally loses the cloud-init static-IP race on first boot;
  `VBoxManage controlvm <vm> reset` once fixes it.
- Overwrite a running binary only after `pkill`-ing it (ETXTBSY otherwise).

## Bring up the fleet

On each agent VM (`agent.toml` copied to `/home/cyuan/agent.toml`):

```bash
chromium --headless=new --no-sandbox --disable-gpu --remote-debugging-port=9222 \
  --user-data-dir=/tmp/cr about:blank &
BCP_AGENT_CONFIG=/home/cyuan/agent.toml bcp-agent --addr 0.0.0.0:7100 &
```

On the controller VM:

```bash
BCP_CONTROLLER_DB=/home/cyuan/controller.sqlite BCP_CONTROLLER_DISABLE_WEB=true \
  bcp-controller --addr 0.0.0.0:7000 &
```

## Run the tests

The `bcp-e2e` driver reads the fleet from `BCP_FLEET` (entries separated by `;`,
fields by `|`): `machine_id|agent_grpc_addr|platform|account_id|cdp_url`.

```bash
export BCP_CONTROLLER=http://127.0.0.1:7000
export BCP_FLEET="a1|http://10.0.9.12:7100|youtube|acct-a1|http://127.0.0.1:9222;\
a2|http://10.0.9.13:7100|x|acct-a2|http://127.0.0.1:9222;\
a3|http://10.0.9.14:7100|douyin|acct-a3|http://127.0.0.1:9222"

# Happy path: register + route + real CDP snapshot/eval on every VM.
BCP_E2E_MODE=vm-fleet bcp-e2e

# Scenario suite (restart the controller between scenarios for a clean state):
BCP_E2E_MODE=scenarios BCP_SCENARIO=exclusivity bcp-e2e
BCP_E2E_MODE=scenarios BCP_SCENARIO=fencing     bcp-e2e
BCP_DEAD_MACHINE=a1 BCP_E2E_MODE=scenarios BCP_SCENARIO=failover bcp-e2e   # kill a1's agent first
BCP_E2E_MODE=scenarios BCP_SCENARIO=persistence-acquire bcp-e2e            # capture PERSIST_* lines
# ...restart controller against the same SQLite...
BCP_LEASE_ID=... BCP_FENCE=... BCP_MACHINE=a1 BCP_MACHINE_COUNT=3 \
  BCP_E2E_MODE=scenarios BCP_SCENARIO=persistence-verify bcp-e2e
```

## What each scenario asserts

| scenario | asserts |
|----------|---------|
| `vm-fleet` | every machine routes + drives a real Chrome (a11y snapshot + `navigator.userAgent`), and a foreign agent rejects a lease it never installed |
| `exclusivity` | a second acquire of the same account is denied (`NOT_FOUND`); an independent account is acquirable concurrently; the profile is reclaimed after release |
| `fencing` | a wrong fencing token and an uninstalled lease are rejected; a **released+superseded** lease is revoked at the agent (regression test for the single-active-lease-per-profile fix) |
| `fencing-release` | releasing a lease revokes it at the agent even with no successor (the controller pushes `UninstallLease`) |
| `auto-offline` | the controller's background sweep marks stale machines offline (run the controller with low `BCP_MACHINE_OFFLINE_MS` / `BCP_SWEEP_SECONDS`) |
| `quarantine` | a quarantined profile is evicted and excluded from acquire until released |
| `audit` | browser operations emit `browser.*` audit events the agent reports to the controller (needs self-registering agents) |
| `failover` | a downed machine surfaces an error at use-time (no hang) while live machines keep serving |
| `persistence` | an active lease and the registered machines survive a controller restart (SQLite reload) |
