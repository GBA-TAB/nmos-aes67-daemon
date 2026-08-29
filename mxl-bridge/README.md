# mxl-bridge

Bridges audio between the RAVENNA/AES67 ALSA PCM device and [MXL](https://github.com/dmf-mxl/mxl)
(Media eXchange Layer — shared-memory + RDMA media flow exchange), orchestrated through NMOS
IS-04/IS-05 like the rest of this project. New standalone process, same placement pattern as
`../ptp-clock-manager/` — the C++ daemon and ptp-clock-manager are untouched by this.

Full design rationale lives in the conversation that produced this skeleton; the summary:

- **Timing is already solved by this project.** `../ptp-clock-manager/drivers/clock_tai_driver.cpp`
  disciplines the OS's `CLOCK_TAI` directly from the RAVENNA driver's PTP offset. Linux's `CLOCK_TAI`
  epoch matches MXL's SMPTE ST 2059-1 epoch exactly, so timestamping a captured ALSA period is just
  `clock_gettime(CLOCK_TAI)` → `sampleIndex = taiNs / (1_000_000_000 / sampleRate)`. No PTP client code
  needed here.
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

Skeleton only — `Cargo.toml` wires up the `mxl` dependency and the whole build toolchain is validated
end to end (see below), but `src/main.rs` doesn't do anything yet. Real implementation (config, ALSA
capture loop, MXL flow writer, NMOS Node/IS-04/IS-05 layer) is in progress.

**Open/unverified**: whether `mxl::load_api("libmxl.so")` (dynamic `dlopen` via `libloading`) actually
finds the built `libmxl.so` at runtime without extra environment setup. The skeleton binary has *no*
`DT_NEEDED` entry for `libmxl` at all (confirmed via `readelf -d`) since nothing calls into it yet — so
this hasn't been exercised. If it doesn't resolve on its own, the fallback is `LD_LIBRARY_PATH` pointed
at wherever `mxl-sys`'s build script placed `libmxl.so` under
`target/debug/build/mxl-sys-<hash>/out/build/lib/` (that hash is a Cargo build-script fingerprint, not
stable across dependency changes — resolve the actual path per-build rather than hardcoding it).

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
