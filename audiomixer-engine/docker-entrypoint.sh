#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Generates a config.json for an arbitrary track/bus/master count from environment variables, then
# execs the binary against it -- this is the "sizing" knob for a containerized deployment
# (Kubernetes Deployment env vars, docker-compose environment:, etc.): sizing is a startup-time
# parameter, not something baked into the image or a custom app-manager concept (MXL itself has no
# normalized app-management layer, this follows MXL's own reference deployment pattern of plain
# env-var-configured containers).
#
# No track input-patches are generated here (see patch.rs docs) -- a container-sized mixer's
# tracks start unpatched (silent) and get an input-patch assigned later over the amixer WebSocket
# protocol's `input-patch` PUT (ws.rs), since there's no way to hand-specify N individual track
# sources at container-start time.
#
# Bus/master decorrelation (see ~/.claude/plans/snug-painting-elephant.md): a bus is a pure summer
# now, owns no flow. MASTER_COUNT unset (the default) gives every generated bus a paired master via
# "auto_master" -- reproduces the old fused bus/master behavior with zero extra authoring, and
# BUS_<n>_TARGET still pins that master's own output-grid target the same way it always did. Set
# MASTER_COUNT to a real (possibly decorrelated-from-BUS_COUNT) value for a bigger system: that
# many *unpatched* masters are generated instead, no bus gets auto_master, and buses/masters get
# wired together explicitly over the WS protocol after startup.
#
# input/output grid sizing (plan §14, the *only* NMOS-facing surface -- neither tracks nor
# buses/masters have NMOS presence of their own): INPUT_GRID_COUNT/OUTPUT_GRID_COUNT generate that
# many empty, IS-05-activatable/patchable slots, decorrelated from TRACK_COUNT/BUS_COUNT/
# MASTER_COUNT, since "how much this device can receive/send over NMOS" is a deployment/capacity
# decision, not a mixing-topology one. OUTPUT_<n>_TARGET pins a specific output-grid entry's flow
# the same way BUS_<n>_TARGET used to.
#
# Each generated grid entry's own channel count is INPUT_GRID_CHANNELS/OUTPUT_GRID_CHANNELS
# (default: $CHANNELS, i.e. stereo) -- *not* forced to 1. This is the knob that decides which of
# two equally-valid topologies a deployment gets: many small entries (INPUT_GRID_COUNT=16,
# INPUT_GRID_CHANNELS=2 -- one Receiver per stereo pair) vs. few wide ones (INPUT_GRID_COUNT=1,
# INPUT_GRID_CHANNELS=32 -- one Receiver backed by one real 32-channel MXL flow, patch.rs's own
# per-channel `SourceRef::Input{entry_id, channel}` crosspoints doing the fan-out into
# tracks/buses/masters same as always). The wide form is the IS-08-analogous pattern discussed for
# this app: one NMOS-facing bundle, channel-granular routing handled entirely inside patch.rs
# rather than by minting a Receiver per stream. Neither form needed a code change to become
# possible -- `InputGridEntryConfig`/`OutputGridEntryConfig` already took an arbitrary `channels`
# count and `flow.rs`'s `FlowReader`/`FlowWriter` already wrap MXL's own native multi-channel flow
# support end to end; this generator simply wasn't exposing that per-entry knob until now.

set -eu

TRACK_COUNT="${TRACK_COUNT:-8}"
BUS_COUNT="${BUS_COUNT:-2}"
# Unset (default): every generated bus gets "auto_master":{} -- a 1:1 paired master per bus. Set to
# an explicit count for a bigger, decorrelated system -- see the header comment above.
MASTER_COUNT="${MASTER_COUNT:-}"
INPUT_GRID_COUNT="${INPUT_GRID_COUNT:-0}"
OUTPUT_GRID_COUNT="${OUTPUT_GRID_COUNT:-0}"
# Per-entry channel count for every generated grid entry -- see the header comment above. Defaults
# to $CHANNELS below once that's set (bash/dash can't forward-reference it here, so the fallback is
# applied when each entry is actually built).
INPUT_GRID_CHANNELS="${INPUT_GRID_CHANNELS:-}"
OUTPUT_GRID_CHANNELS="${OUTPUT_GRID_CHANNELS:-}"
# "simple" (default, today's gain->fader->mute/solo chain) or "full_channel" (adds every
# processing stage in dsp.rs -- filter, EQ, both dynamics stages, phase, delay -- to every
# generated track/master, as structural placeholders; see config.rs's ChannelTemplate docs). One
# value applied uniformly to every generated track/master -- per-resource template mixes aren't
# expressible from env vars alone, hand-author a config.json for that. Buses have no template of
# their own (pure summers).
CHANNEL_TEMPLATE="${CHANNEL_TEMPLATE:-simple}"
MXL_DOMAIN="${MXL_DOMAIN:?MXL_DOMAIN must be set (the shared MXL domain mount, e.g. /home/mxl/domain)}"
SAMPLE_RATE="${SAMPLE_RATE:-48000}"
PERIOD_FRAMES="${PERIOD_FRAMES:-480}"
CHANNELS="${CHANNELS:-2}"
WS_PORT="${WS_PORT:-9090}"
MIXER_ID="${MIXER_ID:-0}"
METER_HZ="${METER_HZ:-25}"
# Kubernetes' downward API exposes the pod's own name as $(POD_NAME) when wired into the
# Deployment's env (fieldRef: metadata.name) -- falls back to the hostname (a container's own
# hostname is its short container id by default) for docker-compose/plain `docker run` use, so
# output-grid flow ids are still deterministic-per-container without extra config there either.
INSTANCE_NAME="${INSTANCE_NAME:-$(hostname)}"
NMOS_LABEL="${NMOS_LABEL:-audiomixer-engine ${INSTANCE_NAME}}"
NMOS_REGISTRY_ADDRESS="${NMOS_REGISTRY_ADDRESS:-}"
NMOS_REGISTRY_PORT="${NMOS_REGISTRY_PORT:-80}"
INTERFACE_NAME="${INTERFACE_NAME:-eth0}"
# The pod's own IP, for building href/manifest_href URLs a registry/controller can actually reach
# -- Kubernetes' downward API exposes this as $(POD_IP) (fieldRef: status.podIP); falls back to
# resolving this container's own hostname for docker-compose/plain `docker run` use.
IP_ADDR="${IP_ADDR:-$(hostname -i 2>/dev/null | awk '{print $1}')}"
# Live runtime state (gain/fader/mute/sends/DSP params/patches -- persistence.rs) is saved here and
# resumed from here on the next start, if a file already exists. Unset (the default) disables
# persistence entirely. Point this at a path on a mounted PersistentVolumeClaim (see
# kube-example.yaml) for a pod restart/reschedule to resume live state, not just static config.
STATE_PATH="${STATE_PATH:-}"

CONFIG_PATH="${CONFIG_PATH:-/tmp/audiomixer-engine.conf}"

# Splits a "key:value" target spec (e.g. "packed_tx_name:testmix2" or "flow_id:<uuid>") into a
# `"target":{"key":"value"}` JSON fragment, or the empty string if $1 is unset/empty -- shared by
# the bus (via auto_master), master, and output-grid generation loops below.
target_json_fragment() {
    target_value="$1"
    if [ -n "$target_value" ]; then
        key="${target_value%%:*}"
        value="${target_value#*:}"
        printf '"target":{"%s":"%s"}' "$key" "$value"
    fi
}

tracks_json=""
i=0
while [ "$i" -lt "$TRACK_COUNT" ]; do
    entry=$(printf '{"id":%d,"label":"Track %d","sends":[],"template":"%s"}' "$i" "$((i + 1))" "$CHANNEL_TEMPLATE")
    if [ -z "$tracks_json" ]; then tracks_json="$entry"; else tracks_json="$tracks_json,$entry"; fi
    i=$((i + 1))
done

buses_json=""
masters_json=""
i=0
while [ "$i" -lt "$BUS_COUNT" ]; do
    if [ -z "$MASTER_COUNT" ]; then
        # Small-mixer default: this bus gets a paired master, carrying the template/target that
        # used to live on the bus itself.
        target_var="BUS_${i}_TARGET"
        eval "target_value=\${$target_var:-}"
        target_json=$(target_json_fragment "$target_value")
        if [ -n "$target_json" ]; then
            auto_master_json=$(printf '"auto_master":{%s,"template":"%s"}' "$target_json" "$CHANNEL_TEMPLATE")
        else
            auto_master_json=$(printf '"auto_master":{"template":"%s"}' "$CHANNEL_TEMPLATE")
        fi
        entry=$(printf '{"id":%d,"label":"Bus %d",%s}' "$i" "$((i + 1))" "$auto_master_json")
    else
        # Decorrelated mode: a plain pure-summer bus, no paired master -- wire it to a master
        # explicitly over the WS protocol after startup.
        entry=$(printf '{"id":%d,"label":"Bus %d"}' "$i" "$((i + 1))")
    fi
    if [ -z "$buses_json" ]; then buses_json="$entry"; else buses_json="$buses_json,$entry"; fi
    i=$((i + 1))
done

if [ -n "$MASTER_COUNT" ]; then
    # No target here -- a master owns no flow at all (see the plan's §14); external visibility, if
    # wanted, comes from patching master-out:<id> into an output-grid entry (which does carry a
    # target, below) after startup.
    i=0
    while [ "$i" -lt "$MASTER_COUNT" ]; do
        entry=$(printf '{"id":%d,"label":"Master %d","template":"%s"}' "$i" "$((i + 1))" "$CHANNEL_TEMPLATE")
        if [ -z "$masters_json" ]; then masters_json="$entry"; else masters_json="$masters_json,$entry"; fi
        i=$((i + 1))
    done
fi

# Falls back to the global stereo default when unset, same convention as every other per-entry
# "channels" field in config.rs.
input_grid_channels="${INPUT_GRID_CHANNELS:-$CHANNELS}"
output_grid_channels="${OUTPUT_GRID_CHANNELS:-$CHANNELS}"

input_grid_json=""
i=0
while [ "$i" -lt "$INPUT_GRID_COUNT" ]; do
    # No "source" -- starts empty, waiting for IS-05 receiver activation (nmos/server.rs).
    entry=$(printf '{"id":"nmos-in-%d","label":"NMOS Input %d","channels":%d}' "$i" "$((i + 1))" "$input_grid_channels")
    if [ -z "$input_grid_json" ]; then input_grid_json="$entry"; else input_grid_json="$input_grid_json,$entry"; fi
    i=$((i + 1))
done

output_grid_json=""
i=0
while [ "$i" -lt "$OUTPUT_GRID_COUNT" ]; do
    target_var="OUTPUT_${i}_TARGET"
    eval "target_value=\${$target_var:-}"
    target_json=$(target_json_fragment "$target_value")
    if [ -n "$target_json" ]; then
        entry=$(printf '{"id":"nmos-out-%d","label":"NMOS Output %d","channels":%d,%s}' "$i" "$((i + 1))" "$output_grid_channels" "$target_json")
    else
        entry=$(printf '{"id":"nmos-out-%d","label":"NMOS Output %d","channels":%d}' "$i" "$((i + 1))" "$output_grid_channels")
    fi
    if [ -z "$output_grid_json" ]; then output_grid_json="$entry"; else output_grid_json="$output_grid_json,$entry"; fi
    i=$((i + 1))
done

cat > "$CONFIG_PATH" <<EOF
{
  "mxl_domain": "${MXL_DOMAIN}",
  "sample_rate": ${SAMPLE_RATE},
  "period_frames": ${PERIOD_FRAMES},
  "channels": ${CHANNELS},
  "ws_port": ${WS_PORT},
  "mixer_id": ${MIXER_ID},
  "meter_hz": ${METER_HZ},
  "instance_name": "${INSTANCE_NAME}",
  "state_path": $([ -n "$STATE_PATH" ] && printf '"%s"' "$STATE_PATH" || printf 'null'),
  "nmos_label": "${NMOS_LABEL}",
  "nmos_registry_address": $([ -n "$NMOS_REGISTRY_ADDRESS" ] && printf '"%s"' "$NMOS_REGISTRY_ADDRESS" || printf 'null'),
  "nmos_registry_port": ${NMOS_REGISTRY_PORT},
  "interface_name": "${INTERFACE_NAME}",
  "ip_addr": "${IP_ADDR}",
  "input_grid": [${input_grid_json}],
  "output_grid": [${output_grid_json}],
  "tracks": [${tracks_json}],
  "buses": [${buses_json}],
  "masters": [${masters_json}]
}
EOF

echo "generated ${CONFIG_PATH} (${TRACK_COUNT} tracks, ${BUS_COUNT} buses, masters '${MASTER_COUNT:-auto-paired}', ${INPUT_GRID_COUNT} input-grid x ${input_grid_channels}ch, ${OUTPUT_GRID_COUNT} output-grid x ${output_grid_channels}ch, template '${CHANNEL_TEMPLATE}', instance '${INSTANCE_NAME}')" >&2
exec /app/audiomixer-engine "$CONFIG_PATH"
