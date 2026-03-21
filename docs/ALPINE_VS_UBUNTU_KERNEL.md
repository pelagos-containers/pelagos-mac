# Alpine vs Ubuntu Kernel Under AVF — Boot Differences and RCU Stalls

This document explains why the Ubuntu build VM suffered ~8-minute boots when using the
Alpine lts kernel, why switching to Ubuntu's own 6.8 HWE kernel fixed it, and why the
networking and console setup differs between the two boot paths.

---

## 1. The Kernels Are Fundamentally Different Builds

**Alpine lts kernel (6.12.67-0-lts)**
- Built by Alpine Linux with a minimalist, generic config
- `PREEMPT_DYNAMIC` — preemption model is runtime-switchable
- No paravirt stubs tuned for the hypervisor environment AVF presents
- `CONFIG_KVM_GUEST` absent or mismatched — no paravirt ticketlock yielding
- Designed for Alpine's own musl-based userspace, not Ubuntu 22.04

**Ubuntu 6.8.0-106-generic (HWE)**
- Built by Canonical with `CONFIG_KVM_GUEST=y` — the critical difference
- Ubuntu-tuned RCU configuration (`RCU_FANOUT`, `RCU_BOOST`, grace-period delays)
- `PREEMPT_VOLUNTARY` (server default) — less aggressive preemption, better
  for latency-sensitive kernel paths under a hypervisor
- The kernel Ubuntu 22.04 was designed and tested to run on

---

## 2. Why the RCU Stalls Happened

AVF (Apple Virtualization Framework) runs vCPUs as Dispatch queue items on the host.
It **silently preempts vCPUs** — it can yank a vCPU off a host core at any time without
notifying the guest kernel. This is fundamentally different from KVM/Hypervisor.framework
with paravirt, where the hypervisor informs the guest before preempting.

When a vCPU is preempted mid-execution:

```
Guest CPU is inside an RCU read-side critical section
         │
         │  ← AVF yanks the vCPU off the host core, gives it to
         │    another macOS process. No notification to guest.
         │
[vCPU halted for some macOS scheduler quantum — could be 10–100ms]
         │
         │  ← vCPU resumes
```

RCU's grace period mechanism assumes that if a CPU has been running, it has passed
through a quiescent state (a point where it holds no RCU read-side locks). When the
Alpine 6.12 kernel's RCU watchdog timer fires and sees CPUs that appear stuck, it
declares a stall. The result was visible in the console at every boot:

```
[   60s] rcu_preempt: detected stalls on CPUs/tasks:
          softirq=350/352  ← softirq handler count barely moved
          fqs=1            ← only 1 "force-quiescent-state" sweep ran
```

This stall blocked the softirq threads that process network packets and timers,
cascading into:
- `systemd-timesyncd` first NTP attempt failing (network not up when it tried)
- `NETDEV WATCHDOG: CPU 1: transmit queue 0 timed out 489310 ms` (virtio_net TX starved)
- All of userspace delayed ~60s waiting for the stall to resolve

**Why Ubuntu's kernel does not stall:**

`CONFIG_KVM_GUEST=y` installs paravirt stubs including `pv_ops.irq.save_fl`,
`pv_ops.cpu.cpuid`, and paravirt ticketlock stubs that yield to the hypervisor
rather than spinning. Even though AVF is not KVM, these stubs make the kernel far more
tolerant of vCPU preemption — when a spin-wait detects no progress, it calls into the
hypervisor layer to yield rather than burning cycles and starving RCU.

Ubuntu's RCU tuning also has larger `RCU_FANOUT` trees and longer initial grace-period
delays, so the RCU watchdog tolerates longer gaps before declaring a stall.

---

## 3. The Boot Sequences Are Completely Different

### Alpine kernel boot path (legacy)

```
AVF loads Alpine vmlinuz (pre-decompressed) + initramfs-custom.gz
         │
         ▼
Alpine kernel boots; mounts initramfs as rootfs
         │
         ▼
pelagos init script runs in initramfs:
  ├── ip link set eth0 up
  ├── ip addr add 192.168.105.2/24 dev eth0   ← static IP set HERE, before switch_root
  ├── ip route add default via 192.168.105.1
  └── copies Alpine rootfs to /dev/vda only if label ≠ "pelagos-root"
         │
         ▼  switch_root → Ubuntu userspace on /dev/vda
         │
         ▼
systemd starts with:
  - systemd-networkd MASKED  (would disrupt the IP set by initramfs)
  - serial-getty@hvc0 MASKED (caused 8-min retry loop — see §4 below)
  - eth0 already has 192.168.105.2; smoltcp relay can reach it
```

The Alpine initramfs configured the IP *before* `switch_root`. Ubuntu's systemd then
ran on top, inheriting that network state, without ever needing to reconfigure eth0.

### Ubuntu kernel boot path (current)

```
AVF loads ubuntu-vmlinuz (decompressed raw arm64 Image) + ubuntu-initrd.img (zstd)
         │
         ▼
Ubuntu 6.8 kernel boots; mounts initrd
         │
         ▼
Ubuntu initrd: minimal — finds root by LABEL=ubuntu-build, mounts ext4, pivot_root
  (no network configuration; no ip= on cmdline, no NFS root)
         │
         ▼  pivot_root → Ubuntu userspace (build.img)
         │
         ▼
systemd starts cleanly:
  ├── systemd-networkd: reads /etc/systemd/network/10-eth.network
  │     └── ip link set eth0 up + ip addr add 192.168.105.2/24 dev eth0
  ├── sshd: starts on port 22
  ├── systemd-timesyncd: NTP sync (succeeds immediately — no RCU stall)
  └── serial-getty@hvc0: auto-login root shell (hvc0 available from kernel boot)
```

---

## 4. Why the Networking Masking Was Necessary Under Alpine — and Harmful Under Ubuntu

Under the Alpine path, the initramfs configured the IP using raw `iproute2` commands.
When Ubuntu's `systemd-networkd` subsequently started (~60s into boot, after the RCU
stall resolved), it saw an address on eth0 and **re-applied it**:

1. Removed the existing address
2. Triggered ARP DAD (Duplicate Address Detection) — sent gratuitous ARPs, waited ~1s
3. Re-added the address

During that ~1s DAD window, the smoltcp NAT relay was ARP-ing for `192.168.105.2`
and receiving no reply. smoltcp's ARP cache expired the entry, the TCP SYN was dropped,
and the SSH relay reported "Connection timed out during banner exchange". This happened
on every SSH retry until networkd finished re-applying the config. Masking networkd
eliminated this disruption.

Under the Ubuntu kernel path, **no initramfs IP configuration occurs at all**. eth0
comes up clean, and networkd is the first and only agent to configure it — no DAD
conflict, no disruption window. Masking networkd under this path means eth0 never gets
an address and the VM is unreachable indefinitely.

---

## 5. The `/dev/hvc0` Issue

The Alpine lts kernel compiled `CONFIG_VIRTIO_CONSOLE` as a module (`virtio_console.ko`).
Under the Alpine boot, the module loaded and the device appeared, but Ubuntu's systemd
had a race: `serial-getty@hvc0.service` started before the device node stabilized,
causing systemd to retry the service in exponential backoff for up to ~8 minutes.
That retry loop was itself a source of delay — it held the `getty.target` ordering
chain, blocking login prompts across the board.

The Ubuntu 6.8 kernel has `CONFIG_VIRTIO_CONSOLE=y` (built-in). `/dev/hvc0` exists
before the initrd even runs. `serial-getty@hvc0.service` starts immediately on first
attempt, and with the root auto-login drop-in, gives an interactive shell within
seconds of multi-user.target.

---

## 6. Summary

| Property | Alpine lts 6.12 | Ubuntu 6.8 HWE |
|---|---|---|
| `CONFIG_KVM_GUEST` | absent | `=y` |
| RCU stall under AVF vCPU preemption | ~60s stall every boot | none |
| Boot time to SSH-ready | ~8 minutes | ~15 seconds |
| `CONFIG_VIRTIO_CONSOLE` | module (race with udev) | built-in |
| eth0 configuration | initramfs (before switch_root) | networkd (after pivot_root) |
| `systemd-networkd` | harmful — must be masked | essential — must be enabled |
| `serial-getty@hvc0` | unstable — must be masked | clean, auto-login root |
| NTP on first attempt | fails (RCU stall delays network) | succeeds |
| Kernel / userspace match | mismatch | matched |

---

## 7. Relationship to the smoltcp NAT Relay

All VM networking — for both the default Alpine container VM and the Ubuntu build VM —
goes through the smoltcp-based NAT relay in `pelagos-vz/src/nat_relay.rs`. There is
no socket_vmnet (vmnet.framework) dependency.

The relay runs as a poll thread inside the pelagos daemon on macOS. SSH traffic reaches
the VM via:

```
ssh → ProxyCommand (pelagos ssh-relay-proxy 22)
       → 127.0.0.1:RELAY_PROXY_PORT (local TCP)
       → smoltcp NAT interface (virtio-net socketpair)
       → VM eth0:22
```

The relay hardcodes the guest IP as `192.168.105.2/24` and answers ARP for the gateway
`192.168.105.1`. Any boot sequence that results in a different IP on eth0, or that
disrupts ARP replies during reconfiguration, will cause SSH to time out. This is why
the kernel and boot path must be matched to the expected IP configuration strategy.
