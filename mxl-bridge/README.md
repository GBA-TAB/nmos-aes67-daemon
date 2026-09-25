# mxl-bridge

Bridges audio between the RAVENNA/AES67 ALSA PCM device and [MXL](https://github.com/dmf-mxl/mxl)
(Media eXchange Layer — shared-memory + RDMA media flow exchange), orchestrated through NMOS
IS-04/IS-05 like the rest of this project. New standalone process, same placement pattern as
`../ptp-clock-manager/` — the C++ daemon and ptp-clock-manager are untouched by this.

Full design rationale lives in the conversation that produced this skeleton; the summary:

- **Timing is already solved by this project — and MXL doesn't even need our help with it.**
  `../ptp-clock-manager/drivers/clock_tai_driver.cpp` disciplines the OS's `CLOCK_TAI` directly from the
  RAVENNA driver's PTP offset, and MXL's `MxlInstance::get_current_index()` already reads the system
  clock internally to compute a TAI-epoch-based sample index (confirmed empirically — see below). So the
  capture loop just seeds a running index from `get_current_index()` once and advances it by the actual
  frame count read each period, exactly matching the pattern in MXL's own `flow-writer.rs` example — no
  manual `clock_gettime(CLOCK_TAI)` arithmetic needed in the hot path. (`src/clock.rs` still exists as a
  startup diagnostic — logs a raw `CLOCK_TAI` read at boot to fail loudly if `clock_tai_driver` isn't
  running — but isn't used for the actual sample-index math; an earlier version tried to compute the
  index manually and reimplement what MXL already does, which was subtly wrong and made every
  `open_samples` call fail with `InvalidArg`.)
- **ALSA-layer integration, not RTP-layer.** Opens the RAVENNA PCM device directly as an independent
  client (same proven pattern as `alsa_src_driver.cpp`, which already does this for both capture and
  playback, independently of the C++ daemon). Never touches RTP, SDP, or the daemon's internals.
- **Phase 1 scope: same-host RX and TX** (AES67/ALSA capture → MXL flow, and MXL flow → ALSA
  playback — both now implemented and verified). RDMA/Fabrics cross-host wiring is a later phase,
  once the NMOS layer sits on top of this.
- **NMOS follows AMWA BCP-007-03 v1.0 (NMOS With MXL)** — see the section below. A Receiver connects
  by `transport_params[0].mxl_flow_id` (what a spec Controller sends) or, for
  visualUniverse-nmosrouter as it is today, by `sender_id` alone (resolved locally or via the
  registry).

## NMOS With MXL (AMWA BCP-007-03 v1.0)

Aligned 2026-09-25 with the specification (AMWA-TV/bcp-007-03 @ 16d66a4), not with any one
implementation. The official schemas are vendored under `contract/` (plus IS-04 v1.3.3), and
`src/nmos/contract_tests.rs` drives this node's real HTTP router against them — the same files are
meant to be checked by every MXL node (the macOS driver included), so they all agree via the spec.

- **Domain identity**: `<mxl_domain>/domain_def.json` (`id`, `label`, `description`, `tags`); created
  with a random UUID if the domain has none, never rewritten if present (`src/mxl_domain.rs`).
- **Senders**: `transport` `urn:x-nmos:transport:mxl`, `manifest_href: null`, `/transportfile` → 404,
  `interface_bindings: []`. `master_enable: true` may name a `receiver_id` (per-subscriber lease) or
  not (controller lease).
- **Receivers**: `interface_bindings: []`, BCP-004-01 caps (`channel_count`, `sample_rate`,
  `sample_depth` of the daemon Source they feed). A request carrying a transport file is refused.
- **IS-05** (v1.1 and v1.2): `active`/`staged`/`constraints` carry exactly one set with
  `mxl_domain_id` and `mxl_flow_id`; `null` = not determined, `"auto"` resolves to this node's own
  values (never listed in constraints; never accepted for a Receiver's flow); anything that cannot
  apply here → 400, an activation that fails → 500 with the cause.

Verified live 2026-09-25 on the real RAVENNA card: router-style connect (sender enabled without a
`receiver_id`, receiver by `sender_id`), spec-style connect (receiver by `mxl_flow_id`, domain
`"auto"`), and a 16-channel flow refused by an 8-channel receiver with its reason; the fed daemon
Sources report `transmitting`. Full round trip confirmed audible the same night: macOS CoreAudio
(AES67_macos_Driver) → AES67 → RAVENNA → ALSA → mxl-bridge → MXL flow → mxl-bridge → ALSA → RAVENNA →
AES67 (239.55.5.5) → back on the Mac.

## Status

**Same-host RX and TX both work end to end**, verified with a full round trip through real ALSA hardware
(well, `snd-aloop`) and a real MXL flow in between — no shortcuts:

`speaker-test` (440Hz sine, `hw:Loopback,0,1`) → `mxl-bridge` RX (`hw:Loopback,1,1` capture → MXL flow) →
`mxl-bridge` TX (same flow → `hw:Loopback,0,2` playback) → `arecord` (`hw:Loopback,1,2`) → WAV file.

Checked two ways:
- **Structural**: MXL's own `mxl-info` tool (built separately from the MXL repo for verification, not a
  `mxl-bridge` dependency) confirmed the RX-written flow had the correct format (`Audio`, 48000/1, 2
  channels), a stable ~9ms latency matching the configured period exactly, and a head index advancing at
  ~48000/s in real time.
- **Content**: the captured round-trip WAV was analyzed (zero-crossing rate, min/max, RMS) — amplitude
  stayed within ±0.8 of full scale (no clipping), RMS (0.556× peak) matched sine-wave theory (0.566×
  expected), and the zero-crossing frequency estimate (427Hz) was close to the actual 440Hz tone. This is
  the first real confirmation that sample *content* survives the round trip correctly, not just flow
  structure/timing.

One real bug found and fixed along the way: the TX read loop's error path `continue`d before reaching the
index-advance line, so any transient read failure (an `OutOfRangeTooLate` happened once at startup, before
the writer had produced anything yet) turned into an infinite retry loop stuck on the same stale index.
Fixed by resyncing to the flow's actual current head (`head_index()`) on any read error instead of blindly
retrying — the more generally-correct behavior anyway (self-heals if the reader ever falls behind or the
writer restarts), not just a one-off patch.

**Caveat on both tests above**: this dev environment has no PTP grandmaster, so `clock_tai_driver`'s servo
had nothing to discipline `CLOCK_TAI` against — it was running free (unsynchronized, though still validly
TAI-formatted). The tests only demonstrate *internal* timing consistency (one host, one shared local clock
feeding the whole pipeline), not real synchronization to an external reference. That distinction will
matter once cross-host (Fabrics/RDMA) work needs two hosts' clocks to actually agree — worth re-verifying
against a real PTP grandmaster before trusting this for that.

**The NMOS Node/IS-04/IS-05 layer is implemented and verified against a real activation**, not just
discovery. With the same RX/TX round-trip test running, a real `PATCH .../single/receivers/{id}/staged`
(via plain `curl`, no controller needed) with `{sender_id, master_enable: true, activation: {mode:
"activate_immediate"}}` correctly resolved the sender's `flow_id` (self-connection shortcut — same-node
sender/receiver pairs skip the registry round trip, see `nmos/server.rs::receiver_patch`), started the TX
thread pointed at it, and produced a captured WAV with an **exact 440.0Hz** zero-crossing match and an
RMS/peak ratio of 0.707 — the precise theoretical value for an undistorted sine wave (1/√2). Cleaner than
the earlier config-driven test (427Hz estimate), likely just measurement variance rather than anything
architecturally different.

What's implemented: IS-04 Node API (Node/Device/Source/Flow/Sender/Receiver, GET-only, one fixed
Sender+Receiver pair per process), IS-05 Connection API (staged/active GET, PATCH for both — sender PATCH
only toggles `master_enable`/`receiver_id` bookkeeping; receiver PATCH is the real one, driving TX thread
lifecycle), and registry registration + heartbeat (skipped cleanly if `nmos_registry_address` is unset —
Node API still serves). Cross-node connections (a receiver activated with a sender_id belonging to some
*other* mxl-bridge instance) go through `nmos/registration.rs::resolve_sender_flow_id`, an IS-04 Query API
GET — not yet tested against a second real instance or the actual orchestrator, only the self-connection
path above.

Known simplifications, deliberately deferred rather than unnoticed: no scheduled activation (only
`activate_immediate` is meaningfully handled, matching what the orchestrator's `ConnectionService` actually
sends); `staged` and `active` are the same state (no separate staged-not-yet-active concept); no IS-04
Query API websocket subscriptions.

One route-syntax bug found while testing this: axum 0.7's path-parameter syntax is `:id`, not `{id}` — the
latter is silently treated as a literal path segment (matches nothing real, falls through to axum's default
404) rather than erroring at startup, so this went unnoticed until the first real PATCH with an actual UUID
failed. `{id}` is axum 0.8's syntax; easy to mix up if skimming newer docs against an older pinned version.

**Resolved**: `mxl::load_api()` (dynamic `dlopen` via `libloading`) works fine given an absolute path —
`find_mxl_so()` in `main.rs` locates the built `libmxl.so` at runtime by searching
`<exe_dir>/build/mxl-sys-*/out/lib/libmxl.so` relative to the running binary's own path (robust regardless
of CWD or debug/release profile, no `LD_LIBRARY_PATH` needed). Passing the bare string `"libmxl.so"`
instead (relying on ambient library search paths, as MXL's own examples do) was not tested.

## Capacity, latency, robustness and tests (2026-09-26)

Verified live on the lab host (RAVENNA driver, `aes67-daemon`, bridge as the orchestrator's
`mxl-bridge-1`): **tx and rx bit-exact, NMOS control matrix 7/7, 128 channels each way provisioned,
survives daemon and bridge restarts.**

### Capacity: a fixed stream layout, set at startup

`capacity` in the config makes the bridge own the daemon's streams: at startup (and every
`recheck_secs`) it plans the tx Sources and rx Sinks the daemon must have and creates, fixes or removes
streams to match (`provision.rs`, `plan()` is pure and unit-tested). Without `capacity` the bridge only
mirrors whatever streams the daemon has. The lab layout, 128 channels each way (16 tx x 8 ch, 16 rx x 8 ch):

```json
"capacity": {
  "alsa_channels": 128,
  "mode": "apply",
  "recheck_secs": 30,
  "tx": { "streams": 16, "channels": 8, "multicast_base": "239.55.5.5" },
  "rx": { "streams": 16, "channels": 8, "parking_group": "239.255.255.1", "delay": 576 }
}
```

- `alsa_channels` is **checked, not set**: the daemon must already run at that width (its own
  `alsa_channels`, now up to 128, see `../daemon/README.md`) or wider. A narrower daemon is logged
  as an error and nothing is applied; at most 64 streams in total (the daemon's stream-id limit).
- `channels` is either one number for every stream (with `streams`) or a list, one entry per stream.
- tx stream *n* sends to `multicast_base + n` (239.55.5.5, .6, ... .20).
- rx streams need an SDP to exist in the daemon: unconnected ones are **parked** on `parking_group`
  (a group nothing sends to). An IS-05 disconnect on the daemon's Receiver parks the Sink again
  (daemon `nmos_sink_parking_address`) instead of deleting it.
- rx `delay` is the daemon's playout delay per Sink in samples (its jitter buffer, added to the rx
  latency as is); default 576 (12 ms). Applied to live Sinks too, without dropping their connection.
- `mode: "log"` only prints the plan - use it first on a daemon with streams you care about.

### Latency settings

| field | default | meaning |
|---|---|---|
| `period_frames` | - | capture (2110 -> MXL) ALSA period and MXL commit size. **48** (1 ms) in the lab |
| `tx_period_frames` | 48 | playback (MXL -> 2110) ALSA period |
| `tx_buffer_periods` | 4 | playback buffer depth; kept full, so added to tx latency as is (4 ms) |
| `tx_mxl_delay_ms` | 3 | starting read delay behind now (TAI) for each MXL Source; self-tuning, see below |
| `rt_priority` | 70 | SCHED_FIFO priority of the capture/playback threads; 0 = normal scheduling |

- **Self-tuning tx read delay.** Each Source is read `delay` behind now; a read that lands before the
  writer has committed adds 1 ms (up to 50 ms). A 1 ms writer settles at the base, a 10 ms-block writer
  at about 10 ms more. A writer that falls far behind is read at its head minus the delay instead.
- **Not every playback period x buffer combination is bit-exact through the RAVENNA driver**: 96 x 3
  played stale audio (repeated old periods). 48 x 4 is verified; re-run the tx audiotest after changing
  either.
- **Measured** (audiotest, sample-exact alignment): tx MXL-commit -> wire about **12 ms** with a 1 ms
  writer, about 13 ms from the audiomixer (2 ms periods); rx wire -> readable in MXL about **1 ms** on
  top of the daemon's playout `delay`.
- **RT threads** need CAP_SYS_NICE: the image sets it on the binary as a *permitted* file capability
  (`setcap cap_sys_nice+p`, after the `chown` - chown clears capabilities) and `rt.rs` raises it with
  `capset` before `sched_setscheduler`; the orchestrator's pod template adds `SYS_NICE`. Without it the
  bridge logs a warning and runs with normal scheduling.

### Robustness

- **Daemon restart**: a daemon stream missing from a poll is only un-mirrored after 10 s
  (`daemon_client.rs`, `REMOVAL_GRACE`), so a daemon restart doesn't tear down NMOS resources and MXL
  flows. The capture/playback threads reopen their ALSA device on failure (`run_reopening`, 1 -> 10 s
  backoff) - the device goes away while the daemon restarts.
- **Failed sources** back off instead of spinning (about 50 ms of consecutive failures).
- **Bridge restart**: IS-05 activations persist in `state_path` and are re-applied at startup.

### Test tools

The bridge binary carries its own test helpers (`src/audiotest.rs`):
`mxl-bridge audiotest gen` (write a deterministic per-channel pattern into a new MXL flow),
`audiotest dump` (read an MXL flow range to a file), `audiotest capture` (AF_PACKET capture of an
RTP multicast on the media NIC, with IGMP join, big receive buffer, drop count and CLOCK_TAI arrival
times). Two scripts drive them from the host (they `kubectl exec` into the bridge pod):

```bash
# tx: pattern -> MXL -> bridge -> wire, checked bit for bit on every channel of every stream
python3 contract/audiotest.py tx --streams 4-15 --seconds 4
GEN_BLOCK=48 python3 contract/audiotest.py tx --streams 4     # as a 1 ms writer (default 480 = 10 ms)

# rx: an external 2110 sender (noise/music, not a pure tone) on stream N, wire vs MXL flow
python3 contract/audiotest.py rx --stream 0 --seconds 4

# NMOS control matrix through reMOS /api/routes (the operator's path)
python3 contract/nmos_control.py --tx 4 --rx 4 --rx-sender <NMOS sender id> [--skip-restart]
```

`nmos_control.py` checks: tx-connect (Test Tones -> 1 kHz on the wire), tx-reroute (tone gone),
tx-survives-restart (bridge restart via the orchestrator), tx-disconnect (silence, stream keeps
running), rx-connect (Sink gets the sender's SDP and receives), rx-audio (bit-exact wire vs MXL; a
silent sender passes as "connection verified"), rx-disconnect (Sink parked, not receiving). It restores
what it changed. Env: `REMOS`, `ORCH`, `DAEMON_NODE`, plus audiotest's `BRIDGE_POD`, `BRIDGE_NODE`,
`DAEMON`, `IFACE`, `BRIDGE_BIN`.

Samples are aligned by content (the pattern for tx, the dumped flow for rx) at one constant RTP/MXL
offset, since the lab grandmaster's epoch is not TAI (RTP time and the MXL index differ by a constant).
Latency is then timed on the host's own clock: packet arrival (CLOCK_TAI, from the capture) against the
MXL index of the same sample (tx), or against when that index became readable in the flow (rx).

### Open (not yet verified)

- rx content over an NMOS connection: the control matrix's only transmitting 2110 audio sender in the
  lab was silent, so rx bit-exactness is verified only with a direct (non-NMOS) RAVENNA stream from a
  Mac.
- IS-08 channel mapping on the bridge's packed 8-channel flows.
- Sustained load: all 32 streams active at once for hours, with CPU/xrun statistics.
- Cross-host MXL (Fabrics/RDMA).

## Building

```bash
cargo build
```

The `mxl` dependency (`../../mxl/rust/mxl`, a **path dependency** — this assumes `~/DEV/nmos/mxl` is
checked out as a sibling of `aes67-linux-daemon`, brittle for anywhere else this gets cloned; swap for a
git dependency on `dmf-mxl/mxl` if that matters later) pulls in `mxl-sys`, whose build script compiles
the whole MXL C++ SDK via CMake + the `Linux-Clang-Debug`/`-Release` presets. That requires, on top of a
normal Rust toolchain:

- **Ninja**: `sudo apt-get install ninja-build`
- **clang-19 as the default `clang`/`clang++`/`lld`** (the CMake presets reference unversioned `clang`):
  ```bash
  sudo apt-get install -y --no-install-recommends \
    clang-19 clang-tools-19 clang-tidy-19 clang-format-19 llvm-19 clangd-19 lld-19
  sudo ~/DEV/nmos/mxl/.devcontainer/scripts/debian/register-clang-version.sh 19 100
  ```
- **librdmacm-dev, libsysprof-capture-4-dev**, plus autoconf/automake/libtool/nasm/bison/flex (build deps
  for the vcpkg ports below):
  ```bash
  sudo apt-get install -y --no-install-recommends \
    librdmacm-dev libsysprof-capture-4-dev \
    autoconf automake libtool nasm bison flex pkg-config build-essential
  ```
- **libfabric v2.5.1**, built from source and installed to `/usr` (MXL's own install script):
  ```bash
  cd /tmp && git clone -b v2.5.1 https://github.com/ofiwg/libfabric.git && cd libfabric && \
    ./autogen.sh && \
    ./configure --prefix=/usr --disable-kdreg2 --disable-memhooks-monitor --disable-uffd-monitor && \
    make -j"$(nproc)" && sudo make install
  ```
- **vcpkg** (no sudo — installs to `~/vcpkg`, matching the path hardcoded in `mxl/CMakePresets.json`'s
  toolchain file reference):
  ```bash
  git clone https://github.com/microsoft/vcpkg ~/vcpkg && ~/vcpkg/bootstrap-vcpkg.sh --disableMetrics
  ```
- **GStreamer dev headers** — not obviously related to this crate, but MXL's top-level `CMakeLists.txt`
  gates its `utils/` subdirectory (which needs GStreamer) behind a separate `BUILD_UTILS` option that
  `mxl-sys`'s build script does *not* turn off (it only disables `BUILD_DOCS`/`BUILD_TESTS`/`BUILD_TOOLS`),
  so this ends up required to configure cleanly:
  ```bash
  sudo apt-get install -y --no-install-recommends libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev
  ```

First build triggers vcpkg building ~8 dependencies from source (catch2, spdlog, fmt, pcapplusplus, etc.)
— expect 15-40+ minutes. Subsequent builds are fast; vcpkg and CMake both cache their outputs.

## Known issues

- **RESOLVED (2026-09-12, found via real-hardware testing - the first time this session actually
  ran the real C++ daemon against the real RAVENNA card): the MXL write index accumulated forever
  with no re-verification against the real clock, drifting without bound.** Requested as a direct
  follow-up to `decklink-mxl-gateway`'s own real-hardware optimization pass that session - started
  the real `aes67-daemon` (`daemon/aes67-daemon -c daemon.conf`) against this host's real
  `MergingRavennaALSA` card for the first time, pointed `mxl-bridge` at it, and activated its one
  real, already-configured Sink ("DKL OPT2 Audio", 16 real channels from the DeckLink OPT2 card's
  own onboard NMOS agent). `mxl-info` showed real, genuine audio capture (head index correctly
  advancing) but `Latency` growing without bound - 729ms, then climbing steadily to 3534ms within
  15 real seconds, no sign of stabilizing.
  - **Root cause**: `MxlAudioFlow::write_next` (`mxl_flow.rs`) seeds its write index once from
    `MxlInstance::get_current_index` on the first call, then only ever increments it by `count`
    every period - never re-verified against the real clock again. The local ALSA hardware clock
    isn't guaranteed to run at exactly the rate `get_current_index`'s own reference clock does, and
    nothing here ever checked that assumption - the same "sampled once, never re-verified" shape as
    the clock-offset staleness bug already found and fixed in `gst-mxl-rs` earlier that session
    (`c3974d40`), independently rediscovered here because it was never ported to this app's own
    hand-rolled index tracking.
  - **Fix**: every `write_next` call now also computes a fresh `get_current_index` and snaps the
    accumulated index to it once they diverge past `drift_tolerance_samples` (5ms at 48kHz, scaled
    by configured sample rate) - see `mxl_flow.rs`'s doc comments on that function and on
    `write_next` itself for the full reasoning, including why this isn't the same fix shape as
    `gst-mxl-rs`'s own (that one derives an index fresh from each GStreamer buffer's real PTS every
    single time with no accumulation at all - not directly portable here, since this has no
    GStreamer buffer/PTS to anchor to, just a raw ALSA period).
  - **Verified live**, same real hardware, same real Sink, same 15-second (then a further 30-second)
    observation window: latency now stays bounded, oscillating within roughly ±8ms with no growth
    trend, instead of climbing past 3.5s. The correction fires on a real but modest fraction of
    periods (~9.75% - 1099 of 11273 real `write_next` calls over ~2m23s), not every call.
  - **Not fixed, flagged as a related open question**: `MxlAudioFlowSource::read_next` (the Rx/
    playback direction) has the identical accumulate-and-never-re-verify code shape, but wasn't
    given the same treatment - the correct reference for a reader is different (trail
    `head_index()` by a stable playout margin, not track it tightly), and no Source/Receiver was
    activated this session to measure whether it actually drifts in practice the way the write
    side provably does. See `read_next`'s own doc comment.
  - **Not investigated, flagged as a likely deeper cause**: the real per-period drift rate implied
    by how often corrections fire (roughly 5ms every ~100ms, order of several percent) is large for
    a real hardware clock mismatch - typical free-running crystal drift is far smaller. This session
    separately found the real daemon logging persistent `driver_manager:: cmd GetPTPStatus failed
    with error unexpected driver command response code` every ~2s against the currently-loaded
    `MergingRavennaALSA` kernel module (`bondagit-2.1`, loaded 2026-09-09) - plausibly means PTP
    genuinely isn't locking on this card right now, which would explain a real, not merely
    measurement-artifact, clock-rate mismatch. This fix bounds the damage regardless of cause, but
    doesn't address whatever's actually keeping the card's clock from being disciplined - worth a
    dedicated look at the daemon/driver communication before relying on this for real production
    timing accuracy.
  - **Update (2026-09-12, same day): found and fixed.** See `../daemon/README.md`'s own "Known
    issues" - the daemon's kernel netlink command/response channel had no per-request sequence
    number, so an unsolicited periodic PTP status push arriving on the same channel as command
    replies permanently desynced whatever real command was sent around the same time. After that
    fix, this app's own drift-correction code (above) fired *zero* times across a fresh
    observation window that, before the daemon fix, triggered it roughly every 100ms - strong
    evidence the apparent clock-rate mismatch was this same communication desync, not a genuine
    hardware clock problem. The drift-correction code here stays regardless (defense-in-depth for
    whenever PTP genuinely isn't locked, e.g. a real signal-loss scenario), but the *cause* of the
    drift actually observed live that day is now understood and fixed at the daemon level.

- **RESOLVED (2026-09-12, found via an audit prompted by the same session's `decklink-mxl-gateway`/
  `mxl-signal-gen` watchdog-hardening work): no fault visibility on a stalled/failing MXL read or
  write, in this app or `../mxl-test-app/`.** Confirmed both apps are structurally immune to the
  two other bug classes found elsewhere that session (a zero-sleep busy-spin loop pegging a core -
  both are paced by real blocking I/O, ALSA hardware writes here, an explicitly-guarded tick
  scheduler in `mxl-test-app`; and `register_all` silently omitting a channel - both iterate live,
  dynamic collections, not a fixed set of `Option<T>` config fields). But a read/write failure in
  either app just logs `tracing::warn!`/`error!` and continues (`alsa_playback.rs`'s
  `resync_to_head` on read failure, `alsa_capture.rs`'s write-failure log, `mxl-test-app`'s
  equivalent in `engine.rs`) - nothing is surfaced to a real NMOS controller. A permanently-dead
  flow (upstream writer gone, MXL domain issue) looks identical, from the registry's point of view,
  to a genuinely healthy one - exactly the class of blind spot the `watchdog` work elsewhere this
  session exists to close, just not yet applied here.
  - **Plan**: add `fault: Option<String>` to `SinkEntry`/`SourceEntry` (`mxl-bridge`, in the
    existing per-map lock - no new lock needed) and `Mutex<Option<String>>` to `InputGridEntry`/
    `OutputGridEntry` (`mxl-test-app`, matching those types' existing per-entry-Mutex convention).
    Set it at the read/write call site on failure, clear it on the next success - safe to do
    unconditionally here (unlike `decklink-mxl-gateway`'s DeckLink SDK case) since neither app ever
    tears down or rebuilds a reader/writer on a transient failure, only resyncs its tracked index.
    Fold `fault.is_none()` into whatever already computes the exposed `active`/`subscription.active`
    (`SinkEntrySnapshot`/`SourceEntrySnapshot`'s `From` impls in `mxl-bridge`; the inline
    `reader.is_some()` check in `mxl-test-app`'s `registration.rs`) rather than overwriting the
    PATCH-driven intent bit directly - so a fault clearing never wrongly resurrects an entry the
    user deactivated while it was faulted. Push the change to the registry promptly on a fault
    *transition* (not every period) via a small debounced notify channel calling each app's
    existing, already-idempotent `register_all`/`register_sink`+`register_source` - both apps
    already have everything needed for a full-registration push, just not a trigger tied to fault
    state today (a related, smaller pre-existing gap found along the way: neither app currently
    re-pushes to the registry on an IS-05 activation change either, only on daemon-topology changes
    or a full periodic/404-triggered resync - the same notify mechanism incidentally covers that
    too).
  - **Implemented as planned, in both apps.** `mxl-bridge`: `SinkEntry`/`SourceEntry.fault`, set/
    cleared in `alsa_capture.rs`/`alsa_playback.rs`, `NmosState::mark_sink_fault`/`clear_sink_fault`
    (+ `Source` counterparts) sending on `NmosState`'s new `fault_notify_tx` only on an actual
    transition; `SinkEntrySnapshot`/`SourceEntrySnapshot::from` now compute `active` as `e.active &&
    e.fault.is_none()` instead of just `e.active`. `mxl-test-app`: `InputGridEntry`/
    `OutputGridEntry.fault: Mutex<Option<String>>`, set/cleared in `engine.rs`'s input-read and
    output-write steps via `MixerState::mark_fault`/`clear_fault` (same notify-on-transition
    pattern, on `MixerState`'s own `fault_notify_tx` this time - the audio engine and NMOS state are
    separate types here, unlike `mxl-bridge`); `resources.rs`'s `sender_json` no longer hardcodes
    `subscription.active: true`, and `registration.rs`'s receiver `active` computation now ANDs in
    `fault.is_none()` too. Both apps' new `registration::run_fault_push` consumes the notify channel,
    draining any further pending notifications before each pass (a burst of near-simultaneous
    faults - e.g. a shared MXL domain hiccup - collapses into one re-registration, not one per
    fault) and re-running the same already-idempotent `register_all` the startup/404-recovery path
    already uses.
  - **Verified**: `cargo test` in both crates - 17/17 (`mxl-bridge`), 75/75 (`mxl-test-app`), all
    passing, including every pre-existing test that constructs an `InputGridEntry`/`OutputGridEntry`/
    `SinkEntry`/`SourceEntry`/`MixerState` directly. Live-smoke-tested `mxl-test-app` against this
    session's own real MXL domain and registry: real output-grid flow genuinely writing (`mxl-info`
    confirms a live, advancing head index), `subscription.active: true` over the real Node API - the
    happy path is provably unbroken, matching the prior hardcoded-`true` behavior exactly when
    nothing is faulted. **Not verified**: the fault path itself under a real ALSA/MXL failure -
    unlike `decklink-mxl-gateway`'s naturally-reproducible DeckLink stall, there was no safe way to
    manufacture a genuine ALSA read/write error or MXL domain fault in this environment without
    risking the shared domain other live processes this session depend on. The mechanism is a
    direct structural copy of the identical, already-live-verified pattern from
    `decklink-mxl-gateway`/`mxl-signal-gen` earlier this session (mark-on-error/clear-on-success +
    notify-only-on-transition), not new design - reasonable confidence, but a real hardware/fault
    test is worth doing before leaning on this in production.
