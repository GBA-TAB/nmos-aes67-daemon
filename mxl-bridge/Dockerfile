# SPDX-License-Identifier: Apache-2.0
#
# mxl-bridge as an mxl-orchestrator app kind (`mxl-bridge`): runs on the kind worker next to the other
# MXL apps, in their shared MXL domain (`domain-pvc` at /home/mxl/domain), and bridges it to ST 2110-30
# through the RAVENNA card's ALSA device of the host (/dev/snd, privileged pod) and the host's
# aes67-daemon (HTTP API).
#
# Built with `--features mxl-not-built`: only MXL's headers are needed (bindgen), no CMake/vcpkg SDK
# build. libmxl.so itself is NOT baked in - the orchestrator mounts the host's build at /opt/mxl-lib
# (same as decklink-mxl-gateway), and the config's `mxl_so_path` points at it, so every MXL app on the
# node uses one and the same MXL library.
#
# Build context: a staging dir holding `mxl-bridge/` and `mxl/` (lib/include + rust, no target/) -
# see `docker-build.sh`, which assembles it; `mxl-bridge`'s path dependency is `../../mxl/rust/mxl`,
# so the layout inside the image mirrors ~/DEV/nmos/{aes67-linux-daemon/mxl-bridge,mxl}.

FROM debian:trixie-slim AS base
RUN apt-get update && apt-get install -y --no-install-recommends \
    dumb-init \
    ca-certificates \
    libasound2t64 \
    && rm -rf /var/lib/apt/lists/*

#-----------------------------------------------------------------
FROM base AS builder
ENV HOME="/root"
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    libasound2-dev \
    libclang-dev \
    clang \
    rustup \
    && rm -rf /var/lib/apt/lists/*
ENV CARGO_HOME="${HOME}/.cargo/"
ENV RUSTUP_HOME="${HOME}/.rustup/"
ENV PATH="${CARGO_HOME}bin:${PATH}"
RUN rustup default stable

COPY mxl/ /build/mxl/
COPY mxl-bridge/ /build/aes67-linux-daemon/mxl-bridge/
WORKDIR /build/aes67-linux-daemon/mxl-bridge
RUN cargo build --release --features mxl-not-built

#-----------------------------------------------------------------
FROM base AS runtime
WORKDIR /app
COPY --from=builder /build/aes67-linux-daemon/mxl-bridge/target/release/mxl-bridge /app/mxl-bridge

# Same uid as the other MXL images (first `useradd` = 1000), so flows in the shared domain stay
# readable/writable across apps; `audio` (gid 29, as on the host) for the RAVENNA /dev/snd nodes.
RUN useradd -m -G audio mxl && chown -R mxl:mxl /app
# Real-time audio threads (src/rt.rs) - after the chown, which would erase file capabilities: SYS_NICE permitted (not effective - the binary must still
# start where SYS_NICE is not allowed); rt.rs raises it just before switching to SCHED_FIFO.
RUN apt-get update && apt-get install -y --no-install-recommends libcap2-bin \
    && setcap cap_sys_nice+p /app/mxl-bridge && rm -rf /var/lib/apt/lists/*
USER mxl

ENTRYPOINT ["/usr/bin/dumb-init", "--", "/app/mxl-bridge"]
