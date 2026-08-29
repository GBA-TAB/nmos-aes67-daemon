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
- **IS-05 activation needs zero orchestrator changes.** The orchestrator's `ConnectionService`
  (`~/DEV/visualUniverse-nmosrouter*`) relays whatever a sender's `manifest_href` returns into the
  receiver's `/staged` PATCH but never validates it — so a receiver here can just ignore that relayed
  content and self-resolve the paired sender via `sender_id` against the IS-04 registry, mirroring the
  daemon's own `fetch_remote_sender_sdp()` pattern. A private-use `transporttype`
  (`urn:x-mxl:transport:flow`) is used since there's no AMWA-registered URN for this. **Implemented and
  verified — see Status.**

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
Query API websocket subscriptions; `transportfile`/`constraints` are minimal placeholders (the design
deliberately doesn't need real SDP-shaped transport_file content — see the IS-05 design note above); no
config yet for a real RAVENNA device (only tested against ALSA Loopback).

One route-syntax bug found while testing this: axum 0.7's path-parameter syntax is `:id`, not `{id}` — the
latter is silently treated as a literal path segment (matches nothing real, falls through to axum's default
404) rather than erroring at startup, so this went unnoticed until the first real PATCH with an actual UUID
failed. `{id}` is axum 0.8's syntax; easy to mix up if skimming newer docs against an older pinned version.

**Resolved**: `mxl::load_api()` (dynamic `dlopen` via `libloading`) works fine given an absolute path —
`find_mxl_so()` in `main.rs` locates the built `libmxl.so` at runtime by searching
`<exe_dir>/build/mxl-sys-*/out/lib/libmxl.so` relative to the running binary's own path (robust regardless
of CWD or debug/release profile, no `LD_LIBRARY_PATH` needed). Passing the bare string `"libmxl.so"`
instead (relying on ambient library search paths, as MXL's own examples do) was not tested.

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
