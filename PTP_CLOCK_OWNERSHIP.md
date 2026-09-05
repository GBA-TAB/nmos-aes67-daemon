# RAVENNA driver: host-clock ownership contract

Scope: this applies to processes on a host that talk to the `MergingRavennaALSA`
kernel driver (`3rdparty/ravenna-alsa-lkm`) — today that's `aes67-daemon` and
`ptp-clock-manager`. It does **not** apply to apps that only use the MXL SDK
directly (mxl-test-app, and presumably future video apps): those don't touch
this driver at all, and already have their own working clock story
(`MxlInstance::get_current_index()` handles TAI-epoch sample indexing
internally — see `mxl-bridge/README.md`).

Written after a real multi-hour debugging session where two violations of the
rule below (documented at the bottom) produced a persistently "unlocked" PTP
status with no error message pointing at the actual cause.

## The rule

**Exactly one process may hold the RAVENNA driver's netlink connection at a
time.** `driver/module_netlink.c` tracks connected clients with a single
global `daemon_pid_` (not a list) — whichever process talks to the driver
*most recently* silently takes over that slot, and every response (PTP
status, RTP stream status, everything) is unicast only to whoever currently
holds it. This isn't limited to PTP status queries: it's the same channel
`aes67-daemon` uses for state-mutating commands like `SetSampleRate` and
`Add_RTPStream`, so a second client isn't just a read conflict, it can steal
responses out from under the daemon's actual stream-management job.

Concretely:
- **Never run `ptp-clock-manager` and `aes67-daemon` at the same time**
  against the same driver instance. `ptp-clock-manager` exists specifically
  for the case where `aes67-daemon` *isn't* running (e.g. mxl-bridge alone,
  which has no netlink connection of its own — it only touches the driver
  through the ALSA PCM layer).
- **Never run two `aes67-daemon` instances** against the same driver. This is
  easy to do by accident (an old instance left running in another terminal) —
  check `pgrep -af aes67-daemon` before trusting a "why won't this lock"
  investigation.

## `CLOCK_TAI` is the host-global shared resource, not the netlink connection

`ptp-clock-manager` disciplines the OS's `CLOCK_TAI` from the driver's PTP
offset *and* publishes the same status into a shared-memory segment
(`/dev/shm/ptp_clock_mgr`, `ptp-clock-manager/shm_clock.h`) that any process
can read with zero netlink access — `daemon/ptp_clock_shm.cpp`'s
`read_ptp_clock_shm_fresh()` already does exactly this, and
`nmos_manager.cpp::build_node_json()` already prefers it when fresh. This is
the actual "host-global" mechanism: whichever process legitimately owns the
netlink connection is responsible for keeping this shared state (and
`CLOCK_TAI`) up to date; every other consumer reads `CLOCK_TAI` and/or the SHM
segment, never opens a second netlink connection of its own.

**Open item, not yet implemented**: `aes67-daemon` doesn't currently
discipline `CLOCK_TAI` itself, even though it already receives the same PTP
offset/lock data over its own netlink session (for the NMOS `clocks[]`
field). Porting `ptp-clock-manager`'s `ClockDiscipline` servo into the daemon
would let a running daemon also own `CLOCK_TAI` directly, making
`ptp-clock-manager` a pure fallback for the no-daemon case by construction
rather than by convention (i.e. they'd never both be started expecting to
coexist). Worth doing before this setup runs unattended for real.

## PTP lock is gated by two independent counters, not one

`driver/PTP.c`'s `GetLockStatus()`:
```c
if (m_usPTPLockCounter != 0) return PTPLS_UNLOCKED;
if (m_usTICLockCounter != 0) return PTPLS_LOCKING;
return PTPLS_LOCKED;
```
- `m_usPTPLockCounter` (hysteresis = 4): decrements on each valid Sync/Follow_Up
  pair processed from the currently-elected master. Purely network-side.
- `m_usTICLockCounter`: decrements as the driver's internal audio-timing
  servo (`m_dTIC_CurrentPeriod`) converges. This only runs while the
  audio-frame TIC timer is active, i.e. while there's a real PCM/RTP stream
  open on the card — perfect PTP network sync alone will never move this
  counter.

The NMOS `clocks[].locked` boolean (`:3212/x-nmos/node/v1.3/self`) collapses
both "unlocked" and "locking" to `false` — it cannot tell you which counter is
still blocking. Use the daemon's own `:8080/api/ptp/status` instead; it
returns the real three-state string (`unlocked`/`locking`/`locked`) plus
per-leg detail.

## Master election is naive — watch for multiple masters

`driver/PTP.c`'s Announce handling is "first Announce wins, then stick with
it until it times out" — not real BMCA priority comparison. Two masters
active on the same domain won't be rejected or merged; the driver will
silently ignore whichever one it didn't elect, *unless* something disrupts
delivery from the elected one, in which case a 2-second no-Sync watchdog
(`"[%u] PTP Master sync timeout, resetting ..."` in the kernel log) resets
lock progress and re-elects — possibly onto the *other* master. Repeated
resets combined with the elected GMID changing between two different values
in the kernel log is the signature of this: multiple masters contending, not
a config problem. Confirm only one real master is transmitting before
debugging anything else.

## Practical runbook

- **Rebuild the kernel module per kernel version.** `modinfo`'s `vermagic`
  field must match `uname -r` exactly, or `insmod` will fail. Rebuild via
  `3rdparty/ravenna-alsa-lkm/driver`'s Makefile (`make clean && make modules`)
  against `/lib/modules/$(uname -r)/build`.
- **`build.sh` checks out our fork's `experimental-hw-timestamping` branch**
  (not upstream `bondagit/aes67-daemon`) after submodule init — see its
  comment for what that branch carries that upstream doesn't yet (per-leg PTP
  status, BCP-008 TX stream status, a raised ALSA channel ceiling).
- **A local NMOS registry (`easy-nmos`) on a `macvlan` network is unreachable
  from the host itself by default** — the Linux `macvlan` driver deliberately
  isolates a parent interface from its own macvlan children, so
  `aes67-daemon` running natively on the same host as the registry containers
  can't reach them without a host-side shim:
  ```
  sudo ip link add macvlan-shim link <parent-iface> type macvlan mode bridge
  sudo ip addr add <free-ip>/32 dev macvlan-shim
  sudo ip link set macvlan-shim up
  sudo ip route add <registry-ip>/32 dev macvlan-shim
  ```
  This interface and its routes do not survive a reboot; recreate them (and
  re-run `docker compose up -d` for the registry/node containers, which have
  no restart policy set) after one.
- **Diagnosing a stuck "unlocked" status**: check, in order — (1) is exactly
  one process connected to the driver's netlink (`pgrep -af
  "aes67-daemon|ptp-clock-manager"`, expect at most one); (2) is real PTP
  traffic arriving at all (`sudo tcpdump -i <iface> -n 'udp port 319 or udp
  port 320'`); (3) kernel log for repeated "PTP Master sync timeout,
  resetting" / changing GMIDs (multiple masters); (4) `:8080/api/ptp/status`
  for the real three-state value, not the NMOS boolean.
