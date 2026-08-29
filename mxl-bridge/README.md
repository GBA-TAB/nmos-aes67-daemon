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
- **Phase 1 scope: RX direction only** (AES67/ALSA capture → MXL flow, exposed as an NMOS Sender).
  TX (MXL flow → ALSA playback, NMOS Receiver) is the structural mirror image and is the natural next
  slice. RDMA/Fabrics cross-host wiring is a later phase, once same-host RX+TX both work.
- **IS-05 activation needs zero orchestrator changes.** The orchestrator's `ConnectionService`
  (`~/DEV/visualUniverse-nmosrouter*`) relays whatever a sender's `manifest_href` returns into the
  receiver's `/staged` PATCH but never validates it — so a receiver here can just ignore that relayed
  content and self-resolve the paired sender via `sender_id` against the IS-04 registry, mirroring the
  daemon's own `fetch_remote_sender_sdp()` pattern. A private-use `transporttype`
  (e.g. `urn:x-mxl:transport:flow`) is used since there's no AMWA-registered URN for this.

## Status

**RX direction (AES67/ALSA capture → MXL flow) works end to end**, verified with a real audio signal:
ALSA Loopback (`snd-aloop`) fed a 440Hz test tone on the playback side, `mxl-bridge` captured from the
paired capture subdevice, and MXL's own `mxl-info` tool (built separately from the MXL repo for
verification, not a `mxl-bridge` dependency) confirmed the resulting flow had the correct format
(`Audio`, 48000/1, 2 channels), a stable ~9ms latency matching the configured period exactly, and a head
index advancing at ~48000/s in real time while the process ran. Not yet verified: actual sample *content*
correctness (that it's really a clean sine wave and not, say, silence or a scaled/clipped version) — the
available MXL CLI tools (`mxl-data-probe`) only read ANC/Data flows, not audio; would need a small custom
`SamplesReader`-based check or a `mxl-gst` sink piped to an analyzer to confirm that specifically.

Still to build: config for a real RAVENNA device (only tested against ALSA Loopback so far), the TX
direction (MXL flow → ALSA playback), and the whole NMOS Node/IS-04/IS-05 layer (`nmos_node_port`,
`nmos_label`, `nmos_registry_address`, etc. are already in `Config` but unused — the process currently
just runs the capture loop directly with no NMOS surface at all).

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
