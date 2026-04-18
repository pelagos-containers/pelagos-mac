# macOS VM Networking Options — Design Analysis

*Researched 2026-03-11. Updated 2026-03-20 to reflect smoltcp NAT relay (PR #117,
v0.2.0). Covers pelagos-mac context: pure-Rust AVF stack, `aarch64-apple-darwin`,
developer tool targeting macOS 13.5+.*

---

## Background

pelagos-mac boots Linux VMs via Apple's `Virtualization.framework` (AVF). Every
networking option available to an AVF-based VM ultimately flows through one of the
mechanisms below. This document evaluates each on six axes plus additional
project-specific factors.

**Current implementation (v0.2.0):** `VZFileHandleNetworkDeviceAttachment` paired
with a pure-Rust smoltcp userspace NAT relay (`pelagos-vz/src/nat_relay.rs`). See
option 5 + option 6 in the tables, and the "Implemented Solution" section at the
bottom.

---

## The Options

### 1. `VZNATNetworkDeviceAttachment` ← replaced in v0.2.0

**What it is:** AVF's "no-entitlement NAT" attachment. One method call; AVF creates
a virtual NIC and the host provides NAT, DHCP, and DNS via the
`InternetSharing`/`NetworkSharing` daemon, which installs masquerade rules into the
kernel's PF packet filter.

**How it works under the hood:**
```
VM virtio-net NIC
  → AVF network device layer
    → InternetSharing / NetworkSharing daemon
      → PF kernel packet filter (NAT anchor)
        → host default route → internet
```
The DHCP server and DNS forwarder are also in the daemon layer. After N VM
lifecycles, `InternetSharing` loses its connection to the PF kernel device
(`connection error: Connection invalid`), the anchor is never installed on the next
VM start, and outbound TCP silently dies. ICMP may continue to work through a
stale or default PF rule.

On **macOS 26**, the daemon was renamed `com.apple.NetworkSharing` and is now
SIP-protected — the user-land kickstart workaround that worked on macOS 13–15 is
no longer available.

| Axis | Assessment |
|---|---|
| **Performance** | Moderate. Kernel NAT path when healthy; overhead from daemon mediation. |
| **Reliability** | **Poor.** Degrades after ~5 VM boots in a session; requires reboot on macOS 26 to recover. |
| **Security** | Good. NAT provides strong VM isolation by default; no additional attack surface. |
| **Long-term viability** | **Concerning.** InternetSharing has been progressively locked down (SIP on Tahoe). Apple's direction is `vmnet.framework`; NAT attachment may become more restricted. |
| **Entitlements** | None required. ✓ |
| **Open source** | No. Proprietary Apple implementation. |
| **Implementation complexity** | Trivial (one-liner in `pelagos-vz`). |
| **Host → VM connectivity** | No (NAT-only; no inbound unless port forwarding is added). |
| **Multi-VM** | Works; each VM gets its own 192.168.64.x address. |
| **Distribution impact** | None. No extra setup needed. |

---

### 2. `vmnet.framework` direct (`com.apple.vm.networking` entitlement)

**What it is:** Apple's dedicated VM networking framework, introduced in macOS 10.10.
Provides three modes: `VMNET_SHARED_MODE` (NAT), `VMNET_HOST_MODE` (host-only),
`VMNET_BRIDGED_MODE`. Bypasses `InternetSharing` entirely — the NAT is implemented
directly in the kernel via `vmnet`, not via PF anchors.

**How it works:**
```
VM virtio-net NIC (via VZFileHandleNetworkDeviceAttachment)
  → vmnet.framework (kernel driver)
    → [shared mode] kernel NAT → host default route → internet
    → [host mode]   192.168.x.0/24 isolated network, no internet
    → [bridged mode] passthrough to physical NIC, VM appears on LAN
```

The key difference from option 1: `vmnet` does not use PF anchors. It has its own
kernel-level NAT implementation that does not degrade across VM lifecycles.

**The entitlement problem:** `com.apple.vm.networking` is a **private entitlement**
restricted to licensed virtualization vendors (OrbStack, Docker Desktop, Parallels,
VMware). There is no self-service approval path. Getting it requires an Apple
Developer Relations contract — effectively, proving you are shipping a commercial
virtualization product.

| Axis | Assessment |
|---|---|
| **Performance** | **Excellent.** Kernel-level vmnet driver; no daemon mediation. Low latency, high throughput. |
| **Reliability** | **Excellent.** No PF anchor degradation. vmnet interface is stable across VM lifecycles. |
| **Security** | Excellent. Kernel isolation; modes are cleanly separated. Bridged mode exposes VM to LAN — use shared or host mode for developer tooling. |
| **Long-term viability** | **Excellent.** This is Apple's strategic VM networking API. OrbStack and Docker Desktop depend on it. Apple has strong incentive to maintain it. |
| **Entitlements** | **Private entitlement — requires Apple contract.** Blocker for most independent developers. |
| **Open source** | No. Proprietary Apple framework. |
| **Implementation complexity** | Moderate. Must use `VZFileHandleNetworkDeviceAttachment` to bridge vmnet ↔ AVF; requires a C shim or Rust `vmnet-sys` binding. |
| **Host → VM connectivity** | Yes (with vmnet shared/host mode — the host interface is reachable). |
| **Multi-VM** | Yes; vmnet handles multiple interfaces. |
| **Distribution impact** | Requires Apple entitlement approval before shipping. |

---

### 3. `socket_vmnet` (privileged helper wrapping `vmnet.framework`) ← evaluated, replaced

**What it is:** An Apache 2.0 privileged helper daemon, maintained by the Lima team
(`github.com/lima-vm/socket_vmnet`), that runs as root via launchd and exposes
`vmnet.framework` networking to unprivileged processes via a Unix socket with fd
passing. Used by Lima (v0.12+) and QEMU on macOS.

**How it works:**
```
VM virtio-net NIC (via VZFileHandleNetworkDeviceAttachment)
  → socket_vmnet_client (unprivileged, fd passed via Unix socket)
    → /var/run/socket_vmnet (Unix socket)
      → socket_vmnet daemon (runs as root, holds vmnet handle)
        → vmnet.framework (kernel driver, shared/host/bridged)
```

The insight: running as root is sufficient to call `vmnet.framework` — the
`com.apple.vm.networking` entitlement is only required when calling vmnet from
a sandboxed or signed-but-unprivileged process. A root daemon sidesteps the
entitlement entirely.

**Installation:** A launchd plist at `/Library/LaunchDaemons/` installs the daemon.
The binary lives at `/opt/socket_vmnet/bin/socket_vmnet` (root-only writable path
for security). Homebrew can install it: `brew install socket_vmnet`.

**Modes supported:** `shared` (internet-accessible NAT, default, gateway
`192.168.105.1`), `host` (isolated, VM-to-host only), `bridged`.

| Axis | Assessment |
|---|---|
| **Performance** | **Good.** vmnet kernel path for actual packet forwarding; minor IPC overhead for fd passing at setup time (~negligible after connect). |
| **Reliability** | **Poor under container workloads.** vmnet.framework maintains a fixed-size kernel NAT table. Under load — OCI image pulls (many parallel TCP streams), high container churn — the table fills and new connections are silently dropped. No error is surfaced; containers appear to hang with no diagnostic signal. This is the primary reason socket_vmnet was rejected for pelagos-mac. |
| **Security** | Good with caveats. The helper runs as root — it is a privileged daemon on the host. Socket permissions (mode 0600, owner root) restrict who can connect. Needs careful installation to `/opt/socket_vmnet` (not `/usr/local`, which is user-writable). |
| **Long-term viability** | **Good.** Actively maintained (Lima org, CNCF Incubating). Tracks vmnet.framework changes. Risk: if Apple restricts vmnet from root processes, this breaks; currently Apple has no such restriction. |
| **Entitlements** | **None required on caller binary.** The helper daemon handles the privileged call. ✓ |
| **Open source** | **Yes. Apache 2.0.** ✓ |
| **Implementation complexity** | Moderate. Need to: (a) bundle/install socket_vmnet as a launchd daemon, (b) use `VZFileHandleNetworkDeviceAttachment` to bridge vmnet fds into AVF, (c) implement fd passing from socket_vmnet_client. Adds a system-level installation step. |
| **Host → VM connectivity** | Yes (vmnet shared/host mode). |
| **Multi-VM** | Yes; multiple VMs share one daemon; unique MAC addresses required. |
| **Distribution impact** | **Adds an installer step.** A signed `.pkg` installer can install the launchd daemon and set ownership to root. Needs a privileged install (standard for `.pkg`). |

---

### 4. `VZBridgedNetworkDeviceAttachment`

**What it is:** AVF attachment that bridges the VM's virtual NIC directly to a
physical host network interface. The VM appears as a peer on the LAN with its own
DHCP-assigned or static IP.

| Axis | Assessment |
|---|---|
| **Performance** | **Excellent.** Kernel bridge, minimal overhead over bare metal. |
| **Reliability** | Good. No daemon-mediated NAT; depends on physical network health. |
| **Security** | **Poor for developer tooling.** VM is fully visible on the LAN. No isolation between VM traffic and host network. Containers could initiate connections to other LAN machines. |
| **Long-term viability** | Tied to `com.apple.vm.networking` entitlement path (same as option 2). |
| **Entitlements** | **`com.apple.vm.networking` required.** Same private entitlement blocker. |
| **Open source** | No. |
| **Implementation complexity** | Low once entitlement is obtained. |
| **Host → VM connectivity** | Yes — VM is a full LAN peer. |
| **Multi-VM** | Yes; each VM needs a unique MAC. |
| **Use case fit** | Poor. Bridging is designed for server VMs that need a real LAN presence, not developer container workloads where isolation is desirable. |

---

### 5. `VZFileHandleNetworkDeviceAttachment` (raw Ethernet frames) ← used in current implementation

**What it is:** AVF attachment that exposes the VM's virtual NIC as a pair of file
handles for raw Ethernet frame I/O. You provide a connected datagram socket (`SOCK_DGRAM`)
and AVF sends/receives raw layer-2 frames through it. You are responsible for
everything above the wire: routing, NAT, DHCP, DNS.

This is not a standalone networking option — it is the **plumbing layer** used to
connect AVF to any real networking backend:
- Pair with `vmnet.framework` (via socket_vmnet or direct) → options 2/3
- Pair with a TUN device + host routing → option 6
- Pair with a user-space TCP/IP stack → option 7 (SLIRP variant)

MTU: default 1500 bytes; configurable up to 65535 on macOS 13+ via
`setMaximumTransmissionUnit`.
Socket buffer recommendation: `SO_RCVBUF` ≥ 4× `SO_SNDBUF`.

| Axis | Assessment |
|---|---|
| **Performance** | Depends entirely on what's on the other end of the socket. |
| **Reliability** | The attachment itself is reliable; reliability depends on the backend. |
| **Security** | Neutral — you implement the security model. |
| **Entitlements** | **None.** The attachment type itself requires no entitlement. ✓ |
| **Open source** | N/A (AVF mechanism, not a standalone solution). |
| **Implementation complexity** | **High.** You must implement or integrate a full networking backend. |
| **Role in pelagos** | This is the integration point used in v0.2.0 — paired with the smoltcp NAT relay. |

---

### 6. SLIRP / `libslirp` (user-mode networking)

**What it is:** A user-space TCP/IP stack originally written for SLIP emulation,
now maintained as `libslirp` (LGPL 2.1) by the QEMU community. QEMU uses it as its
default `user` networking mode. No kernel involvement — every packet is processed by
the library inside the calling process.

**How it works:**
```
VM virtio-net NIC
  → raw Ethernet frames
    → libslirp (user-space TCP/IP: ARP, IP, TCP, UDP, ICMP implemented in library)
      → host TCP/UDP socket calls → internet
```
The library intercepts each frame, unwraps the TCP/UDP payload, makes real system
calls from the host side, then synthesizes response frames. It is complete NAT in
software.

**Performance:** QEMU's own documentation characterizes user-mode networking as
having "a lot of overhead so the performance is poor." Benchmarks consistently show
30–50% throughput reduction vs kernel NAT for bulk transfers. Latency adds 0.5–2ms
per round-trip from the user-space processing loop. For container image pulls
(bulk HTTPS transfers), this is measurable.

**Note on Rust (updated v0.2.0):** pelagos-mac implemented the smoltcp-based userspace
NAT relay (PR #117). `smoltcp` is a Rust no-std TCP/IP stack; we added the host-side
socket proxying that makes it work as NAT. UDP is handled entirely outside smoltcp
(raw frame interception + reply synthesis); TCP uses smoltcp with a default IPv4 route.
This is architecturally similar to SLIRP but pure Rust, no LGPL, and no C dependency.
Performance is sufficient for OCI image pulls and devcontainer builds — 27/27 e2e tests
pass.

| Axis | Assessment |
|---|---|
| **Performance** | **Poor.** 30–50% throughput overhead vs kernel NAT. Higher latency. |
| **Reliability** | **Excellent.** Pure user-space, no kernel state. Cannot degrade. |
| **Security** | **Excellent isolation.** No host network involvement; zero OS-level attack surface from the VM side. |
| **Long-term viability** | **Excellent.** No OS dependency whatsoever. Works identically on every macOS version. |
| **Entitlements** | **None.** ✓ |
| **Open source** | LGPL 2.1 (libslirp). LGPL imposes linking obligations in proprietary distributions. |
| **Implementation complexity** | Moderate-high. C FFI binding + license management. Or build on smoltcp + custom host-side socket proxying (~significant engineering). |
| **Host → VM connectivity** | No by default (same limitation as NAT). Port forwarding must be implemented explicitly. |
| **Multi-VM** | Yes; each SLIRP instance is independent. |
| **Use case fit** | Acceptable for low-frequency operations; problematic for container image pulls at scale. QEMU uses it only because it requires no setup. |

---

### 7. Kernel Extensions (kexts) — Parallels / VMware legacy model

**What it is:** Both Parallels and VMware historically installed kernel extensions
to create their own virtual network interfaces (e.g. `vmnet1`, `vmnet8` on Linux;
`feth0`, `bridge100` variants on macOS). This gave them deep OS integration and
very high performance.

**Status:** Apple has been deprecating kexts since macOS 10.15 Catalina. macOS 12
Monterey made loading third-party kexts require explicit user approval. Both Parallels
(v18+) and VMware (Fusion 13+) have transitioned their networking layer to
System Extensions (`NetworkExtension` framework + `NEDriverKit`). New kext-based
networking is **not viable for new development**.

| Axis | Assessment |
|---|---|
| **Performance** | Was excellent; irrelevant for new code. |
| **Long-term viability** | **Dead.** Kext loading is increasingly restricted and will be removed. |
| **Entitlements** | Requires kext signing certificate from Apple (separate from Developer ID). |
| **Open source** | No. |
| **Verdict** | Not a viable option. Document for completeness only. |

---

### 8. Apple `Containerization` framework (macOS 26+)

**What it is:** Apple's open-source (Apache 2.0) Swift framework announced at WWDC
2025. Runs each container in its own micro-VM via AVF. Provides a dedicated IP per
container — no per-port forwarding required. Uses Virtualization.framework under the
hood.

**Networking model:** VM-per-container with dedicated IPs. The framework manages
networking internally; it exposes Netlink socket APIs for in-container network
configuration. Specific entitlements for networking are not publicly documented, but
AVF's `com.apple.security.virtualization` is required.

| Axis | Assessment |
|---|---|
| **Performance** | Sub-second container starts claimed. Networking benchmarks not yet available. |
| **Reliability** | Unknown; framework is 0.x with no production track record. |
| **Long-term viability** | High if Apple continues investment; risky as 0.x dependency. |
| **Entitlements** | `com.apple.security.virtualization` at minimum; full entitlement surface not documented. |
| **Open source** | **Yes. Apache 2.0.** ✓ |
| **Implementation complexity** | Very high: Swift-only API, no C interface; Rust ↔ Swift FFI is painful. |
| **macOS requirement** | **macOS 26 only.** Blocks ~95% of existing macOS installs. |
| **Verdict** | 2027+ watch item. Track macOS 26 adoption and 1.0 release. |

---

## Comparative Summary

### Performance & Reliability

| Option | Throughput | Latency | Degradation risk | Recovery |
|---|---|---|---|---|
| 1. VZNATNetworkDeviceAttachment | Moderate | Moderate | **High** (after ~5 VMs) | Reboot on macOS 26 |
| 2. vmnet.framework direct | **Excellent** | **Excellent** | None | N/A |
| 3. socket_vmnet | Good | Good | None | N/A |
| 4. VZBridgedNetworkDeviceAttachment | Excellent | Excellent | None | N/A |
| 5. VZFileHandleNetworkDeviceAttachment | Depends on backend | Depends | Depends | Depends |
| 6. SLIRP / libslirp | **Poor** (−30–50%) | High | None | N/A |
| 7. Kexts | Was excellent | Was excellent | N/A | Dead path |
| 8. Apple Containerization | Unknown | Unknown | Unknown | Unknown |

### Security & Entitlements

| Option | Entitlements | Privilege required | VM isolation | Notes |
|---|---|---|---|---|
| 1. VZNATNetworkDeviceAttachment | None | None | NAT | Simple, fragile |
| 2. vmnet.framework direct | **Private Apple contract** | Entitlement | NAT / host-only | Best isolation, gated |
| 3. socket_vmnet | **None on caller** | Root daemon via launchd | NAT / host-only | Root helper is audited OSS |
| 4. VZBridgedNetworkDeviceAttachment | Private Apple contract | Entitlement | **None** (LAN-visible) | Wrong mode for dev tooling |
| 5. VZFileHandleNetworkDeviceAttachment | None | Depends on backend | Depends | Plumbing layer |
| 6. SLIRP | None | None | **Strongest** (pure user-space) | LGPL obligation |
| 7. Kexts | Apple kext cert | Kernel | N/A | Dead |
| 8. Apple Containerization | Undocumented | Unknown | AVF per-VM | macOS 26 only |

### Developer / Distribution Friction

| Option | Installer impact | Setup step needed | License |
|---|---|---|---|
| 1. VZNATNetworkDeviceAttachment | None | None | Proprietary |
| 2. vmnet.framework direct | Requires Apple entitlement approval | None (post-approval) | Proprietary |
| 3. socket_vmnet | **.pkg must install launchd daemon** | One-time (launchd) | **Apache 2.0** |
| 4. VZBridgedNetworkDeviceAttachment | Requires Apple entitlement approval | None | Proprietary |
| 5. VZFileHandleNetworkDeviceAttachment | Depends on backend | Depends | N/A |
| 6. SLIRP | None | None | **LGPL 2.1** (linking obligation) |
| 7. Kexts | Dead | Dead | N/A |
| 8. Apple Containerization | macOS 26 only | None | **Apache 2.0** |

### Long-term OS Support Trajectory

| Option | macOS 13–15 | macOS 26 (Tahoe) | macOS 27+ outlook |
|---|---|---|---|
| 1. VZNATNetworkDeviceAttachment | Works (pfctl workaround) | **Degraded — reboot only** | At risk |
| 2. vmnet.framework direct | Works | Works | **Strategic API — safe** |
| 3. socket_vmnet | Works | Works | Safe (follows vmnet) |
| 4. VZBridgedNetworkDeviceAttachment | Works | Works | Safe |
| 5. VZFileHandleNetworkDeviceAttachment | Works | Works | Safe (stable primitive) |
| 6. SLIRP | Works | Works | Works (no OS dependency) |
| 7. Kexts | Deprecated | Broken | Gone |
| 8. Apple Containerization | N/A | Beta | Strategic if Apple invests |

---

## Recommendation for pelagos-mac

### Current implementation (v0.2.0): smoltcp userspace NAT relay

The implemented solution is `VZFileHandleNetworkDeviceAttachment` (option 5) paired
with a pure-Rust smoltcp NAT relay (PR #117). This replaced both `VZNATNetworkDeviceAttachment`
(option 1) and `socket_vmnet` (option 3), which were evaluated and discarded.

**Why smoltcp won:**

1. **Zero external dependencies.** No Homebrew packages, no launchd daemons, no
   privileged helpers, no installer steps. Works out of the box.
2. **No kernel NAT state to degrade.** smoltcp is pure userspace — there is no PF
   anchor, no vmnet handle, and no daemon that can lose its connection.
3. **Pure Rust.** Consistent with the no-Go, no-subsystem-dependency architecture.
4. **No entitlements.** `VZFileHandleNetworkDeviceAttachment` requires no special
   entitlement on the caller binary.
5. **Proven at scale.** 27/27 devcontainer e2e tests pass, including Dockerfile
   builds with `apt-get install` (large multi-file downloads over TCP).

**Architecture:**
```
VM virtio-net NIC
  → VZFileHandleNetworkDeviceAttachment (SOCK_DGRAM socketpair)
    → pelagos-vz nat_relay.rs (smoltcp TCP stack + raw UDP interception)
      → host TCP/UDP sockets → internet
```

**Known tradeoffs vs kernel NAT:**
- Throughput for bulk transfers (OCI image pulls) is sufficient but not as fast as
  vmnet kernel path. This has not been a practical problem.
- Each TCP connection requires a host socket and a relay thread. Connection scaling
  is bounded by host FD limits, not smoltcp itself.

### Future watch items

**vmnet.framework direct (option 2):** If pelagos reaches commercial distribution
and Apple entitlement approval is feasible, option 2 offers higher bulk throughput.
File with Apple Developer Relations when there is a clear distribution story.

**Apple Containerization (option 8):** Track for 2027 when macOS 26 has meaningful
market share and the framework has a security track record.

### Ruled out

- **Option 1 (VZNATNetworkDeviceAttachment):** PF anchor degrades; unrecoverable
  without reboot on macOS 26. Replaced.
- **Option 3 (socket_vmnet):** Requires a privileged launchd daemon as a Homebrew
  install step. Replaced by the zero-dependency smoltcp relay.
- **Option 4 (Bridged):** Wrong security model for developer container tooling.
- **Option 7 (Kexts):** Dead.

---

## IPv6 Status and Phase 4 Design Decision

*Added 2026-04-18. Tracks issue #229.*

### What works today (Phases 1–3)

The relay is dual-stack. The VM has a static ULA address (`fd00::2/64`) and a
dynamic EUI-64 link-local. The relay holds `fd00::1/64` and a dynamic gateway LL.

| Traffic | How it works | Status |
|---|---|---|
| `ping6 fd00::1` or `ping6 <gateway-LL>` | Relay synthesises reply directly | ✅ Works |
| `ping6 <external>` from VM | Not answered — would fake reachability | ✅ Correctly rejected |
| UDP from VM to external IPv6 host | Proxied via `UdpSocket::bind("[::]:0")` on host | ✅ Works if host has IPv6 |
| UDP from VM to `fd00::1` | Echoed locally by relay | ✅ Works |
| TCP from VM to external IPv6 host | smoltcp completes handshake, data dead-ends | ❌ Not proxied |
| `ping6 <external>` from container | Packet routed to relay, relay drops it | ❌ No path out |

The VZ MLD snooping constraint applies to all of the above: the relay cannot receive
IPv6 frames via multicast unless VZ's virtual switch has already registered the relay
as a member of the relevant solicited-node group. The workaround — advertising a
multicast MAC as the TLLA in NDP Neighbor Advertisements — is in place for all relay
addresses. Any new relay address requires the same treatment.

### Phase 4: outbound IPv6 to the internet

Three approaches, each with different tradeoffs:

#### Option A — pf NAT66 (kernel)

Add a `pf` anchor rule that rewrites the source of packets from `fd00::2` to the
host's real global IPv6 address before they leave `en0`.

```
# /etc/pf.anchors/pelagos-nat66
nat on en0 inet6 from fd00::/64 to any -> (en0)
```

**Pros:** Works for all protocols (TCP, UDP, ICMPv6) without any per-protocol relay
code. Performance is kernel-level.

**Cons:** Requires `sudo pfctl` to load the anchor — a privileged operation.
Installs a persistent system-level rule that outlives the VM. Requires the host
to have a real global IPv6 address; won't work on IPv4-only networks. Adds a
`pf` dependency that contradicts the zero-external-dependency design principle.

#### Option B — tun interface + userspace SNAT

Create a `utun` interface on the host, assign an IPv6 address to it, configure
the relay to write outbound VM packets into the tun fd with a rewritten source
address, and let the host kernel route them out.

**Pros:** Fully userspace, no `pf` involvement. Works for all protocols.

**Cons:** `utun` creation on macOS requires the `com.apple.net.utun-control` IOCTL,
which works from unprivileged processes but is not straightforward. Managing the
tun fd inside the relay's poll loop adds significant complexity. The source address
must be an address the host kernel accepts as a legitimate local source, which
requires also assigning it to the tun interface.

#### Option C — per-protocol raw-socket proxy (incremental)

Extend the existing per-protocol proxy pattern:
- ICMPv6 echo to external: open a raw IPv6 ICMP socket, send the probe, wait for
  real reply, forward back to VM. macOS raw ICMPv6 sockets require no privilege
  for echo (`SOCK_DGRAM` + `IPPROTO_ICMPV6`).
- TCP outbound: full bidirectional proxy (connect to real destination from host,
  splice with smoltcp TCP socket). Harder than UDP because TCP is stateful and
  bidirectional.

**Pros:** No privilege, no new dependencies, no system-level rules. Incremental —
ICMPv6 echo can be done today; TCP is a separate effort.

**Cons:** Each protocol requires explicit implementation. Does not handle protocols
the relay doesn't know about (future protocols, raw IP, etc.).

### Current position

Option C (per-protocol) for ICMPv6 echo is low-hanging fruit and can be done without
any design risk. Option A (pf NAT66) solves everything at once but requires privilege
and adds a system dependency. Option B (tun) is the best long-term answer if general
protocol coverage is needed without privilege, but the implementation cost is high.

No decision has been made. Phase 4 is deferred pending alignment on which approach
fits the zero-dependency architecture goal.
