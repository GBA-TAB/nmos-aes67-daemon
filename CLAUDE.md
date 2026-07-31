# CLAUDE.md

This file does not document the whole codebase (no such file existed before this one — it was written by
Claude Code from a separate session working in the companion orchestrator project, based on a targeted
code survey, not full exploration of this repo). It exists to hand off one specific, in-progress task:
**bringing this daemon's BCP-008-01/02 and ST 2022-7 support in line with what a companion NMOS
orchestrator now expects.** Treat everything below as a working note for that task, not general
architecture documentation — verify against the actual code before relying on any of it, especially if
time has passed.

## Status update (2026-08-01)

All three gaps below have been implemented (uncommitted, in this working tree — see `git diff`) and the
daemon builds clean (`cmake --build .`). **Not yet tested against real hardware or the orchestrator** —
only compile-verified, same caveat as everything else in this file until proven otherwise.

1. **`Node.interfaces[]`** — `build_node_json()` (`daemon/nmos_manager.cpp:547`) now loops over
   `is_dual_leg() ? 2 : 1` legs, emitting one entry per leg using `config_->get_interface_name(idx)` (the
   already-correct per-leg name `interface_bindings` uses) and that leg's own MAC via `get_interface_mac()`
   for idx 1 (idx 0 still uses the cached `get_mac_addr_str()`).
2. **`linkStatus` partial-degradation** — `nmos_is12.cpp` now has a `compute_link_status(leg0_up, dual_leg,
   leg1_up)` helper checking both legs independently via `get_interface_link_up(config_->get_interface_name(idx))`,
   returning `PartiallyHealthy` when exactly one leg is up. Wired into both `ncp_receiver_monitor_props` and
   `ncp_sender_monitor_props`.
3. **BCP-008 counter methods** — confirmed via the driver headers
   (`3rdparty/ravenna-alsa-lkm/driver/RTP_stream_info.h:109-128`, `TRTP_stream_status`) that the driver only
   exposes boolean RTP error flags, no per-packet loss/lateness *counts* — so real per-leg counters would need
   new driver instrumentation, out of scope here. Implemented `GetLostPacketCounters`/`GetLatePacketCounters`
   (`NcReceiverMonitor`, methodId 4,1/4,2) and `GetTransmissionErrorCounters` (`NcSenderMonitor`, methodId 4,1)
   in the `handle_is12_message` dispatcher returning an empty `NcCounter` collection, per BCP-008-01's own
   conformance language ("devices that do not have the capability... MUST implement the method but return an
   empty collection"). Method IDs verified against the authoritative model
   (`AMWA-TV/nmos-control-feature-sets`, `monitoring/README.md`, `main` branch — bcp-008-01 v1.0.0 itself has
   no frozen copy of the model and always points at that living doc). Also added to
   `ncp_class_descriptor_json()`'s method list for `NcReceiverMonitor`/`NcSenderMonitor` so
   `GetControlClass` advertises them.

Note: the live model (`nmos-control-feature-sets/monitoring`, `main` branch) has since grown
`linkStatusTransitionCounter`/`connectionStatusTransitionCounter`/etc. and an `autoResetCountersAndMessages`
property that this daemon's existing `4,1`-`4,8` property layout doesn't have — deliberately **not** touched
here since it wasn't one of the three flagged gaps and the orchestrator's own `ReadCountersAsync` doesn't
depend on it. Flagging in case a future compliance pass wants it.

## The companion project

`~/DEV/visualUniverse-nmosrouter` — a Blazor/.NET NMOS routing orchestrator ("reMOS Router"). This daemon
is its device-side (NMOS Node) counterpart: the orchestrator's `DeviceMonitorService`/`ChannelMappingService`
consume this daemon's IS-12 (BCP-008) and IS-08 endpoints directly. That project's own `CLAUDE.md` has a
full "BCP-008-01/02 status monitoring" section and an "IS-08" section worth reading if picking this up from
scratch — it documents the client-side assumptions this daemon needs to satisfy, including AMWA spec
details (property/method names, exact enum integer values, per the published `bcp-008-01`/`bcp-008-02`
specs and the NMOS Control Feature Sets "Monitoring" datatype registry, checked directly against the spec
text during that session, not guessed).

The orchestrator was recently extended (2026-07-31) with: a red/blue ST 2022-7 redundancy-leg UI indicator
per receiver/sender (`RedundancyLegs.razor`), TX-side (`Sender`) leg tracking added for parity with the
existing RX side, and BCP-008 packet/error counter support (`DeviceMonitorService.ReadCountersAsync`,
invoking `NcReceiverMonitor.GetLostPacketCounters()`/`GetLatePacketCounters()` and
`NcSenderMonitor.GetTransmissionErrorCounters()` as IS-12 *methods*). This daemon does not yet support the
counter methods, and has two other real gaps against what the orchestrator/spec expects — see below.

## Survey findings (as of the daemon code checked out when this was written)

A read-only code survey (not yet verified by compiling/running) found three concrete gaps, in priority
order:

### 1. `Node.interfaces[]` is spec-non-compliant in dual-leg (ST 2022-7) mode

`daemon/nmos_manager.cpp:547-551`, in `build_node_json()`:
```cpp
<< ",\n  \"interfaces\": [{"
<< "\n    \"name\": \"" << config_->get_interface_name() << "\","
<< "\n    \"port_id\": \"" << colon_to_dash_mac(config_->get_mac_addr_str()) << "\","
<< "\n    \"chassis_id\": \"" << colon_to_dash_mac(config_->get_mac_addr_str()) << "\""
<< "\n  }]"
```
Always emits exactly **one** array entry, even when the receiver/sender is dual-leg. Worse: in dual-leg
mode, `config_->get_interface_name()` (the no-arg overload, `config.hpp:67`) returns the *raw, unsplit*
`interface_name_` string — literally `"eth0,eth1"` (comma-separated) as one garbled `name`, not two real
interface names. `config.cpp:111-113` splits this into the correct per-leg vector (`interfaces_`) but never
mutates `interface_name_` itself, so the two code paths diverge.

By contrast, `interface_bindings` on Sender (`nmos_manager.cpp:684-685`) and Receiver (`:707-708`) **is**
correct — it uses `get_interface_name(uint8_t idx)` (`config.hpp:68-71`), which indexes into the properly
split `interfaces_` vector, giving real per-leg names (e.g. `"eth0"`, `"eth1"`).

**Why this matters**: IS-04's own schema for `Node.interfaces[].name` says it's *"used by sub-resources of
this node such as senders and receivers to refer to interfaces to which they are bound"* — i.e.
`interface_bindings[i]` values are supposed to match an entry in `Node.interfaces[].name`. Right now they
never can, since the Node only ever advertises one garbled combined name. Any spec-conformant NMOS client
(including AMWA's own test suite) would likely flag this. The orchestrator's own UI isn't broken by it
(`RedundancyLegs.razor` reads `interface_bindings` directly, doesn't cross-reference `Node.interfaces` at
all — confirmed by checking the raw IS-04 schemas before implementing), but it's still a real conformance
bug worth fixing independent of the orchestrator.

**Fix shape**: `build_node_json()` should emit one `interfaces[]` entry per leg, using the already-correct
`interfaces_` vector (same source `interface_bindings` already uses) instead of the raw
`get_interface_name()` no-arg call. Check whether `config_` exposes a per-index MAC getter for `port_id`/
`chassis_id` too — the survey didn't confirm whether both legs would legitimately share one MAC (e.g. a
single NIC's two ports) or need distinct ones; verify against how `rtp_port_sec`/`rtp_mcast_base_sec` are
configured alongside `interface_name_` in `config.cpp`.

### 2. `linkStatus` can't represent a partially-degraded redundant pair

`daemon/nmos_is12.cpp` uses one shared health enum for every BCP-008 status property (lines 37-40:
`kHealthInactive=0, kHealthHealthy=1, kHealthPartiallyHealthy=2, kHealthUnhealthy=3`), reused across
`NcOverallStatus`/`NcLinkStatus`/`NcConnectionStatus`/`NcSynchronizationStatus`/`NcStreamStatus`/
`NcTransmissionStatus`/`NcEssenceStatus` alike (confirmed no separate `NcLinkStatus` type exists anywhere
in the tree). This is actually **fine** numerically — the orchestrator independently confirmed via the
published spec that `NcLinkStatus`'s real values (`AllUp=1/SomeDown=2/AllDown=3`, no `0`) coincide exactly
with the generic `Healthy(1)/PartiallyHealthy(2)/Unhealthy(3)` used elsewhere, so reusing one enum isn't
itself a bug.

The actual gap: `link_status = link_up ? kHealthHealthy : kHealthUnhealthy` (`nmos_is12.cpp:133-134` for
receivers, `:217-218` for senders) is driven by a single boolean and **never emits `2`
(PartiallyHealthy/SomeDown)**. For a dual-leg (2022-7) receiver/sender with one leg up and one down — the
exact scenario `SomeDown` exists to describe — this daemon currently can't report it at all; it'll show
either fully healthy or fully unhealthy depending on which single `link_up` check feeds it. Confirm first
whether `link_up` is currently derived from the correct per-leg interface state or (like the `Node.interfaces`
bug above) from the buggy combined `interface_name_` string — that's the same root cause pattern and may
need fixing together.

**Fix shape**: for dual-leg receivers/senders, evaluate both legs' link state independently and report
`Healthy` (both up) / `PartiallyHealthy` (exactly one up) / `Unhealthy` (both down) — reusing the existing
`kHealth*` constants, no new enum needed.

### 3. BCP-008 counter methods don't exist at all

Confirmed via `grep -rn "GetLostPacketCounters\|GetLatePacketCounters\|GetTransmissionErrorCounters\|PacketCounter\|CounterEvent"` — zero matches anywhere in `nmos_is12.cpp`/`nmos_manager.{cpp,hpp}`. The
IS-12 method dispatcher (`handle_is12_message`, `nmos_is12.cpp:357-480`) only implements `NcObject.Get`
(1,1) / `Set` (1,2, always `405`) / `NcBlock.GetMemberDescriptors` (2,1) / `NcClassManager.GetControlClass`
(3,1) — anything else returns `501 MethodNotImplemented`.

This isn't strictly *breaking* anything today — the orchestrator's `ReadCountersAsync` already treats a
missing/unimplemented method the same as "device doesn't support this," per spec (`"devices that do not
have the capability... MUST implement the method but return an empty collection"` — not implementing the
method at all degrades to the same client-visible outcome as long as the class descriptor doesn't advertise
it, though the *cleanest* spec-compliant answer is to actually implement it and return an empty collection,
per the MUST language).

**This is the most valuable addition if pursued**: `daemon/session_manager.cpp` already tracks real
per-leg stream handles for dual-leg receivers/senders (`info.handle[0]`/`handle[1]`, set up in `add_sink`/
`add_source` around lines 1040-1079 / 663-711) — i.e. the daemon already has the two legs as distinct,
addressable things at the driver level, which is exactly the substrate real per-leg loss/lateness counters
would need. **Open question, not yet investigated**: does the underlying kernel driver
(`driver_->add_rtp_stream()`/the driver interface these handles come from) already expose per-stream
packet-loss/lateness statistics that could be read out per handle, or would this need new driver
instrumentation? That needs to be checked in the driver interface (likely under `3rdparty/ravenna-alsa-lkm`
or wherever `driver_` is defined) before sizing this properly — it could be a small IS-12 surface change
over existing data, or a much larger addition if the driver doesn't track this yet.

**Fix shape if driver data exists**: implement `GetLostPacketCounters`/`GetLatePacketCounters` on
`NcReceiverMonitor` and `GetTransmissionErrorCounters` on `NcSenderMonitor` in the `nmos_is12.cpp`
dispatcher, returning one `NcCounter`-shaped entry per leg (`{name, value, description}` — no numeric
codes, just a JSON array per the spec). Name each counter with the real interface name (e.g.
`"eth0 lost packets"`) once finding #1 above is fixed and real names are available — that's what makes the
counters human-correlatable to a specific leg on the orchestrator side, even though the client doesn't
auto-correlate them (the spec defines no structured way to do so, confirmed directly from the spec text).
Also needs the new methods added to whatever builds the `GetControlClass` response's method list, so the
orchestrator's `classDescriptor.MethodsOrEmpty` lookup actually finds them.

## Practical notes for whoever picks this up

- Read-only survey only — nothing in this repo has been modified or built yet as of this note.
- The orchestrator side (`~/DEV/visualUniverse-nmosrouter`) is fully built/tested/deployed already; this
  daemon is the remaining side of the pair.
- No automated IS-12 tests exist under `daemon/tests/` — changes here would need manual verification
  against a real client (the orchestrator, or a generic IS-12 tool) same as everything else in this file
  that's marked unverified.
- `daemon/README.md:231-232` already self-documents that BCP-008-02 sender monitoring "requires the small
  kernel driver patch... without a matching kernel module, sender monitoring reports degraded/unknown
  status" — i.e. some of this was already known to be hardware-dependent before this survey.
