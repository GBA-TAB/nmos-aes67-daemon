#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Generates a config.json for an arbitrary track/bus count from environment variables, then execs
# the binary against it -- this is the "sizing" knob for a containerized deployment (Kubernetes
# Deployment env vars, docker-compose environment:, etc.): sizing is a startup-time parameter, not
# something baked into the image or a custom app-manager concept (see the Phase 2 plan's
# Verification section note on why -- MXL itself has no normalized app-management layer, this
# follows MXL's own reference deployment pattern of plain env-var-configured containers).
#
# No track sources are generated here (see config.rs's TrackConfig::source docs) -- a
# container-sized mixer's tracks start silent and get assigned a source later over the amixer
# WebSocket protocol's `source` PUT (ws.rs), since there's no way to hand-specify N individual
# track sources at container-start time. Buses always get a real flow at startup (Bus::new
# requires one); BUS_<n>_TARGET can pin a specific one (e.g. "packed_tx_name:testmix2" to feed a
# specific mxl-bridge instance) — otherwise each bus gets an id derived from INSTANCE_NAME, which
# should be set to the pod name in Kubernetes so replicas don't collide (see
# ids::instance_bus_flow_id).

set -eu

TRACK_COUNT="${TRACK_COUNT:-8}"
BUS_COUNT="${BUS_COUNT:-2}"
MXL_DOMAIN="${MXL_DOMAIN:?MXL_DOMAIN must be set (the shared MXL domain mount, e.g. /home/mxl/domain)}"
SAMPLE_RATE="${SAMPLE_RATE:-48000}"
PERIOD_FRAMES="${PERIOD_FRAMES:-480}"
CHANNELS="${CHANNELS:-2}"
WS_PORT="${WS_PORT:-9090}"
MIXER_ID="${MIXER_ID:-0}"
METER_HZ="${METER_HZ:-25}"
# Kubernetes' downward API exposes the pod's own name as $(POD_NAME) when wired into the
# Deployment's env (fieldRef: metadata.name) -- falls back to the hostname (a container's own
# hostname is its short container id by default) for docker-compose/plain `docker run` use, so bus
# flow ids are still deterministic-per-container without extra config there either.
INSTANCE_NAME="${INSTANCE_NAME:-$(hostname)}"
NMOS_LABEL="${NMOS_LABEL:-mxl-test-app ${INSTANCE_NAME}}"
NMOS_REGISTRY_ADDRESS="${NMOS_REGISTRY_ADDRESS:-}"
NMOS_REGISTRY_PORT="${NMOS_REGISTRY_PORT:-80}"
INTERFACE_NAME="${INTERFACE_NAME:-eth0}"
# The pod's own IP, for building href/manifest_href URLs a registry/controller can actually reach
# -- Kubernetes' downward API exposes this as $(POD_IP) (fieldRef: status.podIP); falls back to
# resolving this container's own hostname for docker-compose/plain `docker run` use.
IP_ADDR="${IP_ADDR:-$(hostname -i 2>/dev/null | awk '{print $1}')}"

CONFIG_PATH="${CONFIG_PATH:-/tmp/mxl-test-app.conf}"

tracks_json=""
i=0
while [ "$i" -lt "$TRACK_COUNT" ]; do
    entry=$(printf '{"id":%d,"label":"Track %d","bus_assign":[]}' "$i" "$((i + 1))")
    if [ -z "$tracks_json" ]; then tracks_json="$entry"; else tracks_json="$tracks_json,$entry"; fi
    i=$((i + 1))
done

buses_json=""
i=0
while [ "$i" -lt "$BUS_COUNT" ]; do
    target_var="BUS_${i}_TARGET"
    eval "target_value=\${$target_var:-}"
    if [ -n "$target_value" ]; then
        # $target_value is e.g. "packed_tx_name:testmix2" or "flow_id:<uuid>" -- split on the
        # first ':' into a target object matching config.rs's BusTarget externally-tagged shape.
        key="${target_value%%:*}"
        value="${target_value#*:}"
        target_json=$(printf '"target":{"%s":"%s"}' "$key" "$value")
    else
        target_json=""
    fi
    if [ -n "$target_json" ]; then
        entry=$(printf '{"id":%d,"label":"Bus %d",%s}' "$i" "$((i + 1))" "$target_json")
    else
        entry=$(printf '{"id":%d,"label":"Bus %d"}' "$i" "$((i + 1))")
    fi
    if [ -z "$buses_json" ]; then buses_json="$entry"; else buses_json="$buses_json,$entry"; fi
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
  "nmos_label": "${NMOS_LABEL}",
  "nmos_registry_address": $([ -n "$NMOS_REGISTRY_ADDRESS" ] && printf '"%s"' "$NMOS_REGISTRY_ADDRESS" || printf 'null'),
  "nmos_registry_port": ${NMOS_REGISTRY_PORT},
  "interface_name": "${INTERFACE_NAME}",
  "ip_addr": "${IP_ADDR}",
  "tracks": [${tracks_json}],
  "buses": [${buses_json}]
}
EOF

echo "generated ${CONFIG_PATH} (${TRACK_COUNT} tracks, ${BUS_COUNT} buses, instance '${INSTANCE_NAME}')" >&2
exec /app/mxl-test-app "$CONFIG_PATH"
