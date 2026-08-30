//! Mirrors mxl-bridge's own `src/mxl_flow.rs` id-derivation scheme exactly (same namespace, same
//! format strings) so this app can address mxl-bridge's flows by the same names an operator uses
//! in mxl-bridge's own config/IS-08 requests, without needing to paste raw UUIDs between the two
//! processes. Not a shared library on purpose — mxl-bridge is a binary crate, and this handful of
//! pure functions is cheaper to keep in sync by hand (and by the tests below) than to restructure
//! mxl-bridge into a lib+bin split just for this. **If mxl-bridge's `mxl_flow.rs` ever changes
//! these formats, update here too.**

const ID_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x6d, 0x78, 0x6c, 0x2d, 0x62, 0x72, 0x69, 0x64, 0x67, 0x65, 0x2d, 0x6e, 0x73, 0x2d, 0x00, 0x00,
]);

fn stable_id(name: &str) -> uuid::Uuid {
    uuid::Uuid::new_v5(&ID_NAMESPACE, name.as_bytes())
}

/// The MXL flow_id of a daemon Sink's default (unpacked) mirrored flow on mxl-bridge — what a
/// `sink_daemon_id` track source in this app's config resolves to.
pub fn sink_flow_id(daemon_id: u8) -> uuid::Uuid {
    stable_id(&format!("mxl-bridge-sink-flow:{daemon_id}"))
}

/// The MXL flow_id mxl-bridge creates (writer) for a packed-RX flow of the given IS-08 name —
/// what a `packed_rx_name` track source in this app's config resolves to.
pub fn packed_rx_flow_id(name: &str) -> uuid::Uuid {
    stable_id(&format!("mxl-bridge-packed-rx-flow:{name}"))
}

/// The MXL flow_id mxl-bridge *reads* (as a packed-TX flow's reader) for the given IS-08 name —
/// what a `packed_tx_name` bus target in this app's config resolves to, i.e. writing to this
/// flow_id is how this app feeds mxl-bridge's packed-TX crosspoint.
pub fn packed_tx_flow_id(name: &str) -> uuid::Uuid {
    stable_id(&format!("mxl-bridge-packed-tx-flow:{name}"))
}

// This app's own ids. These don't need to match anything external — nothing looks them up by
// name — so reusing the same namespace with an app-local prefix is just for internal consistency,
// not interop. Node/Device are per-instance (like the bus/track ids below), keyed by
// `instance_name`, so two replicas registering with the same registry don't collide.
pub fn node_id(instance_name: &str) -> uuid::Uuid {
    stable_id(&format!("mxl-test-app-instance-node:{instance_name}"))
}
pub fn device_id(instance_name: &str) -> uuid::Uuid {
    stable_id(&format!("mxl-test-app-instance-device:{instance_name}"))
}
/// A bus's mirrored NMOS Sender id (the Flow's own id is `instance_bus_flow_id`, above — a
/// Sender is a distinct resource from its Flow).
pub fn instance_bus_sender_id(instance_name: &str, bus_id: u32) -> uuid::Uuid {
    stable_id(&format!("mxl-test-app-instance-bus-sender:{instance_name}:{bus_id}"))
}
/// A track's mirrored NMOS Receiver id.
pub fn instance_track_receiver_id(instance_name: &str, track_id: u32) -> uuid::Uuid {
    stable_id(&format!("mxl-test-app-instance-track-receiver:{instance_name}:{track_id}"))
}

/// A bus's flow_id when no explicit target is configured (`BusTarget` absent) — derived from an
/// `instance_name` (e.g. the pod name, so each replica in a container/Kubernetes deployment gets
/// distinct, but still deterministic/reproducible-across-restarts, bus flow ids) plus the bus's
/// own id, rather than requiring every containerized instance's config to spell out an explicit
/// UUID per bus (see docker-entrypoint.sh, which generates config for an arbitrary track/bus count
/// without computing any ids itself).
pub fn instance_bus_flow_id(instance_name: &str, bus_id: u32) -> uuid::Uuid {
    stable_id(&format!("mxl-test-app-instance-bus-flow:{instance_name}:{bus_id}"))
}
pub fn instance_bus_source_id(instance_name: &str, bus_id: u32) -> uuid::Uuid {
    stable_id(&format!("mxl-test-app-instance-bus-source:{instance_name}:{bus_id}"))
}
/// An output-grid entry's flow_id when no explicit target is configured — same idea as
/// `instance_bus_flow_id`, keyed by the entry's own string id (the output grid's own namespace,
/// `patch.rs`) instead of a bus's numeric one.
pub fn instance_output_flow_id(instance_name: &str, output_id: &str) -> uuid::Uuid {
    stable_id(&format!("mxl-test-app-instance-output-flow:{instance_name}:{output_id}"))
}
pub fn instance_output_source_id(instance_name: &str, output_id: &str) -> uuid::Uuid {
    stable_id(&format!("mxl-test-app-instance-output-source:{instance_name}:{output_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned against mxl-bridge's own `mxl_flow.rs` output — both values below are real, observed
    /// output from a running mxl-bridge (the first is the `flow_id` a live mxl-bridge actually
    /// reported for daemon Sink 1 via `GET /x-nmos/node/v1.3/senders/` during Milestone 4a's
    /// Loopback verification; the second is the real `.mxl-flow` directory name mxl-bridge created
    /// on disk for `packed-rx:testmix` during Milestone 4b's), independently recomputed via
    /// Python's `uuid.uuid5` (same RFC 4122 algorithm) against the same namespace and name
    /// strings — not guessed. If mxl-bridge's format strings or namespace ever change, this test
    /// (and the real interoperability) breaks loudly instead of silently.
    #[test]
    fn ids_match_mxl_bridges_own_values() {
        assert_eq!(sink_flow_id(1).to_string(), "769531a4-4386-505e-a5a3-b81ae72ded6c");
        assert_eq!(packed_rx_flow_id("testmix").to_string(), "293b7244-a27b-58a9-8536-f584455cc0ca");
    }
}
