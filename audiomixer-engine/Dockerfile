# SPDX-License-Identifier: Apache-2.0
#
# Builds audiomixer-engine against a from-source MXL SDK build, the same way mxl-sys's own build.rs
# does it outside a container (clang + cmake/ninja + vcpkg + libfabric) -- mirrors the builder
# stage of mxl/examples/Dockerfile, trimmed to what mxl-sys's build actually needs (no GStreamer
# plugin build, no top-level SDK `ninja install`: mxl-sys vendors and builds its own copy of the
# SDK into its own OUT_DIR, it doesn't link against a system-installed one here).
#
# IMPORTANT — build context: audiomixer-engine's Cargo.toml depends on the sibling `mxl` repo via a
# relative path (`../../mxl/rust/mxl`), so this can't be built with `audiomixer-engine/` as the Docker
# context (Docker can't see outside its context root). Build from the common parent of both repos
# instead, e.g. from `~/DEV/nmos/`:
#
#   docker build -f aes67-linux-daemon/audiomixer-engine/Dockerfile -t audiomixer-engine .
#
# (adjust the two path arguments if your checkout layout differs, but keep the *relative* distance
# between `mxl/` and `aes67-linux-daemon/audiomixer-engine/` at exactly two levels, matching the crate's
# own `../../mxl/rust/mxl` path -- that relationship, not any specific absolute path, is what has
# to be preserved inside the build context).

FROM debian:trixie-slim AS base
RUN apt-get update && apt-get install -y --no-install-recommends \
    dumb-init \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

#-----------------------------------------------------------------
FROM base AS builder
ENV HOME="/root"
ARG CLANG_VERSION=19

RUN apt-get update && apt-get install -y --no-install-recommends \
    "clang-${CLANG_VERSION}" \
    "clang-tools-${CLANG_VERSION}" \
    "lld-${CLANG_VERSION}" \
    build-essential bison flex \
    cmake ninja-build \
    curl zip unzip tar git \
    pkg-config \
    autoconf automake libtool \
    librdmacm-dev \
    libsysprof-capture-4-dev \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    rustup \
    && rm -rf /var/lib/apt/lists/*

# Make the requested clang version the one `cc`/`c++`/`clang` resolve to — mxl-sys's build.rs
# invokes cmake with the "Linux-Clang-Release"/"-Debug" preset, which expects `clang`/`clang++` on
# PATH to be this version, not whatever Debian's default alternative happens to be.
#
# The build also needs an unversioned `ld.lld` on PATH: mxl's own CMake preset passes
# `-fuse-ld=lld` to the compiler (found the hard way — omitting the `lld-${CLANG_VERSION}` package
# above entirely made the very first CMake compiler-sanity-check fail with "invalid linker name in
# argument '-fuse-ld=lld'").
RUN update-alternatives --install /usr/bin/clang clang "/usr/bin/clang-${CLANG_VERSION}" 100 \
    && update-alternatives --install /usr/bin/clang++ clang++ "/usr/bin/clang++-${CLANG_VERSION}" 100 \
    && update-alternatives --install /usr/bin/cc cc "/usr/bin/clang-${CLANG_VERSION}" 100 \
    && update-alternatives --install /usr/bin/c++ c++ "/usr/bin/clang++-${CLANG_VERSION}" 100 \
    && update-alternatives --install /usr/bin/ld.lld ld.lld "/usr/bin/ld.lld-${CLANG_VERSION}" 100

RUN git clone https://github.com/microsoft/vcpkg.git "${HOME}/vcpkg" \
    && cd "${HOME}/vcpkg" && ./bootstrap-vcpkg.sh -disableMetrics
ENV VCPKG_ROOT="${HOME}/vcpkg"

# Preserve the exact two-levels-up relationship audiomixer-engine/Cargo.toml's `../../mxl/rust/mxl`
# path dependency expects — see the module-level comment above.
COPY mxl/ /build/mxl/
COPY aes67-linux-daemon/audiomixer-engine/ /build/aes67-linux-daemon/audiomixer-engine/
RUN /build/mxl/.devcontainer/scripts/common/libfabric/install.sh
RUN /build/mxl/.devcontainer/scripts/common/rust/install-rust.sh
ENV CARGO_HOME="${HOME}/.cargo/"
ENV RUSTUP_HOME="${HOME}/.rustup/"
ENV PATH="${CARGO_HOME}bin:${PATH}"

WORKDIR /build/aes67-linux-daemon/audiomixer-engine
RUN cargo build --release

#-----------------------------------------------------------------
# Runtime image: just the binary plus the libmxl.so mxl-sys's build vendored for it, laid out the
# same way main.rs::find_mxl_so() expects to find it at runtime relative to its own path (a
# `build/mxl-sys-*/out/lib/libmxl.so` subdirectory next to the executable — the `mxl-sys-0` name
# below doesn't need to match cargo's real fingerprinted directory name, find_mxl_so() only checks
# the `mxl-sys-` prefix).
FROM base AS runtime
WORKDIR /app
COPY --from=builder /build/aes67-linux-daemon/audiomixer-engine/target/release/audiomixer-engine /app/audiomixer-engine
COPY --from=builder \
    /build/aes67-linux-daemon/audiomixer-engine/target/release/build/mxl-sys-*/out/lib/libmxl.so \
    /app/build/mxl-sys-0/out/lib/libmxl.so
COPY aes67-linux-daemon/audiomixer-engine/docker-entrypoint.sh /app/docker-entrypoint.sh
RUN chmod +x /app/docker-entrypoint.sh

RUN useradd -m mxl && chown -R mxl:mxl /app
# Real-time audio threads (src/rt.rs) - after the chown, which would erase file capabilities: SYS_NICE permitted (not effective - the binary must still
# start where SYS_NICE is not allowed); rt.rs raises it just before switching to SCHED_FIFO.
RUN apt-get update && apt-get install -y --no-install-recommends libcap2-bin \
    && setcap cap_sys_nice+p /app/audiomixer-engine && rm -rf /var/lib/apt/lists/*
USER mxl

ENTRYPOINT ["/usr/bin/dumb-init", "--", "/app/docker-entrypoint.sh"]
