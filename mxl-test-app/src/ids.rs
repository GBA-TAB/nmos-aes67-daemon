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

// This app's own ids, for buses it creates flows for itself (an explicit `flow_id` bus target).
// These don't need to match anything external — nothing looks them up by name — so reusing the
// same namespace with an app-local prefix is just for internal consistency, not interop.
pub fn app_device_id() -> uuid::Uuid {
    stable_id("mxl-test-app-device")
}
pub fn bus_source_id(bus_id: u32) -> uuid::Uuid {
    stable_id(&format!("mxl-test-app-bus-source:{bus_id}"))
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
