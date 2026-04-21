# IPv6 Networking in pelagos-mac

*Implemented in v0.6.15 (PRs #250-era). Covers SLAAC, NDP proxy, routing, and the
host-side machinery that gives the VM a real Global Unicast Address (GUA).*

---

## Overview

The VM gets a real GUA — the same /64 prefix as the macOS host — via SLAAC.  No NAT
occurs for IPv6; packets flow with the VM's address as source and destination, fully
visible to the upstream router.  This requires active cooperation from the host:

1. **RA synthesis** — the relay synthesises a Router Advertisement so the VM learns
   the /64 prefix and default router without needing one from the actual router.
2. **NDP proxy (Phase 1)** — when the VM's GUA is assigned, the pfctl helper seeds
   the upstream router's NDP cache and adds a kernel host route so inbound replies
   reach the VM.
3. **NDP proxy (Phase 2, pending #250)** — a BPF listener on en0 that responds to
   ongoing NS probes from the router, keeping the cache valid after expiry.

---

## Packet Path

### Outbound (VM → internet)

```
VM eth0  →  relay (avf socket)  →  utun10  →  macOS routing table
                                                    │  default via en0
                                                    ▼
                                               en0 → upstream router → internet
```

- The relay receives raw Ethernet frames from the AVF socket, strips the 14-byte
  Ethernet header, prepends a 4-byte utun AF header (`AF_INET6 = 30`), and writes
  to the utun fd.
- The macOS kernel sees an incoming IPv6 packet on utun10 and routes it via the
  default route (en0).  `net.inet6.ip6.forwarding = 1` must be set (it is by
  default on most systems).
- Source address is the VM's GUA — no NAT.

### Inbound (internet → VM)

```
internet → upstream router → en0 (host)
                                  │  kernel sees dst=vm_gua, looks up route
                                  │  host route: vm_gua → utun10  (added by pfctl)
                                  ▼
                             utun10  →  relay (reads from utun fd)
                                  │  prepend Ethernet header (dst=vm_mac, src=GATEWAY_MAC)
                                  ▼
                             relay (avf socket) → VM eth0
```

- The pfctl helper adds a `/128` host route for the VM's GUA pointing at utun10
  when `AssignUtunAlias` is called.
- Inbound packets addressed to vm_gua arrive at en0, and the kernel's routing table
  directs them to utun10.
- The relay reads from utun_fd, wraps in an Ethernet frame with `dst = vm_mac` and
  `src = GATEWAY_MAC (02:00:00:00:00:01)`, and delivers to the VM.

---

## SLAAC — how the VM gets its GUA

The relay detects the host's GUA /64 prefix on the egress interface at startup using
`ifconfig` output.  When the VM sends a Router Solicitation (ICMPv6 type 133), the
relay synthesises a Router Advertisement containing:

- **Prefix Information option**: the real host /64 prefix, `A=1` (SLAAC), `L=1`
  (on-link), lifetime 30 days.
- **Router Lifetime**: 1800 s (so the VM installs a default route via `fe80::1`).
- **Source**: `fe80::1` (the relay's virtual link-local address on the virtual Ethernet).

The VM does SLAAC: it forms its EUI-64 IID from its MAC, appends it to the prefix,
runs DAD, and assigns the GUA to `eth0`.

The relay detects SLAAC completion by waiting for the first non-DAD IPv6 packet
sourced from the GUA (DAD NS uses `::` as source).  At that point it calls
`pfctl_assign_utun_alias` to set up the host route and NDP seed.

---

## NDP proxy — why it is needed

The upstream router knows the host's en0 MAC (`aa:bb:cc:...`).  It does NOT know
the VM's GUA.  When the router wants to deliver a packet to `vm_gua`, it first sends
a Neighbour Solicitation (NS) to the solicited-node multicast address for `vm_gua`.
Nobody on the physical LAN answers — the VM is behind the utun tunnel, not on en0.
The router marks `vm_gua` as `(incomplete)` and drops the packet.

The host must act as an NDP proxy: answer NS for `vm_gua` with an NA that names
`en0_mac` as the link-layer address.  The router then caches `vm_gua → en0_mac` and
can deliver replies.

### Phase 1 — seeding at alias time (implemented)

When `pfctl_assign_utun_alias` is called with the VM's GUA:

1. **Gratuitous NA** — an unsolicited Neighbour Advertisement (ICMPv6 type 136) is
   broadcast to `ff02::1` (all-nodes multicast) on en0, advertising
   `vm_gua → en0_mac`.  Most routers accept gratuitous NAs and update their cache.

2. **Unicast probe NS** — a Neighbour Solicitation (type 135) is sent as a unicast
   Ethernet frame directly to the router's MAC address (looked up from the host NDP
   table), sourced from vm_gua with SLLA=en0_mac.  This causes the router to send an
   NA back to the host, simultaneously seeding its own cache with `vm_gua → en0_mac`.
   **Unicast is critical**: sending to the solicited-node multicast address would also
   hit our own en0 NIC, causing the host's NDP stack to create a conflicting `/128
   UHLWI` entry for vm_gua on en0 that overrides the utun10 host route.

Both frames are injected via BPF (`/dev/bpfN` with `BIOCSETIF`).

### Phase 2 — ongoing proxy (pending, issue #250)

Router NDP caches expire (typically every 20–30 s after the router re-probes).  After
the first expiry the router sends a new NS; if nobody answers, `vm_gua` goes
incomplete again and inbound traffic stops.

Phase 2 adds a BPF listener on en0 that watches for ICMPv6 NS (type 135) targeting
vm_gua and immediately responds with an NA.  This keeps the router's cache valid
indefinitely.  Until Phase 2 lands, reachability degrades after ~30 s of idle.

---

## Virtual gateway addresses

The relay presents itself to the VM as a virtual gateway with the following fixed
addresses, all answered by NDP synthesised in the relay:

| Address | Scope | Purpose |
|---|---|---|
| `02:00:00:00:00:01` | Ethernet MAC | GATEWAY_MAC — used for all gateway Ethernet frames |
| `fe80::1` | Link-local | Default router advertised in RA; VM routes GUA traffic here |
| `fd00::1/128` | ULA | Infrastructure anchor on utun10; VM static config may use this as default gateway |

`fd00::1/128` exists solely to give utun10 an IPv6 address so the macOS kernel
accepts `route add -inet6 -host <gua> -interface utunN` (rejected with ENETUNREACH
otherwise).  The relay intercepts NDP NS for `fd00::1` and responds with GATEWAY_MAC
— the kernel routes `fd00::1` via `lo0`, so without relay interception the VM's NS
goes unanswered.

---

## Host-side setup sequence

```
relay start
  └── detect_host_gua_prefix(en0)          → gua_prefix stored in RelayState
  └── pfctl_setup_utun(utun10, en0, subnet)
        └── ifconfig utun10 inet 192.168.N.1 192.168.N.2 up
        └── ifconfig utun10 inet6 fd00::1 prefixlen 128 alias   ← IPv6 anchor
        └── pfctl NAT44 rule: nat on en0 from 192.168.N.0/24

VM sends Router Solicitation
  └── relay intercepts RS, sends synthesised RA (prefix=gua_prefix)
  └── VM does SLAAC → assigns vm_gua to eth0, DAD, then usable

VM sends first real GUA-sourced packet
  └── maybe_assign_gua_alias fires
  └── pfctl_assign_utun_alias(utun10, vm_gua)
        └── route add -inet6 -host vm_gua -interface utun10
        └── BPF: send gratuitous NA (vm_gua → en0_mac) to ff02::1
        └── BPF: send unicast probe NS (src=vm_gua, dst=router) to router_mac

Inbound IPv6 reply arrives at en0
  └── kernel route: vm_gua → utun10  →  relay reads, wraps in Ethernet → VM
```

---

## Known limitations

| Limitation | Issue | Notes |
|---|---|---|
| NDP cache expiry breaks inbound after ~30 s idle | #250 Phase 2 | BPF listener needed |
| Prefix mobility (host roams to new network) | #248 | Re-issue RA when GUA prefix changes |
| IPv6 assignment inside container network namespaces | #244 | Post-SLAAC work |
