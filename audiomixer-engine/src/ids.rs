//! Names and ids follow `mxl-<host>-<app>-<resource>` (GBA-TAB/mxl docs/Naming.md), so the same
//! app on several hosts sharing one registry never collides, and ids are stable across restarts.
//! This app's own resources: `gridin<NN>-<NN>` (input-grid receivers) and `gridout<NN>-<NN>`
//! (output-grid source/flow/sender), keyed by grid position - the channel range the labels show.
//! The bridge's flows this app reads are named with the bridge's app name on the same host
//! (`bridge_app`, default `bridge-1`): `rx<nn>`, `packedrx-<name>`, `packedtx-<name>`.

use mxl::naming::{Kind, Naming};
use std::sync::OnceLock;

static NAMING: OnceLock<Naming> = OnceLock::new();
static BRIDGE: OnceLock<Naming> = OnceLock::new();

/// Sets this process's naming (app = MXL_APP_NAME, else `instance_name`) and the bridge's app
/// name. Called once at startup, before any id is derived.
pub fn init(instance_name: &str, bridge_app: &str) {
    let n = NAMING.get_or_init(|| Naming::from_env(instance_name));
    let _ = BRIDGE.set(n.for_app(bridge_app));
}

pub fn naming() -> &'static Naming {
    NAMING.get_or_init(|| Naming::from_env("audiomixer"))
}

fn bridge() -> &'static Naming {
    BRIDGE.get_or_init(|| naming().for_app("bridge-1"))
}

fn range(prefix: &str, start: u32, channels: u32) -> String {
    if channels <= 1 {
        format!("{prefix}{:02}", start + 1)
    } else {
        format!("{prefix}{:02}-{:02}", start + 1, start + channels)
    }
}

/// An input-grid entry's resource name: its channel range (`gridin01-08`).
pub fn input_resource(grid_channel_start: u32, channels: u32) -> String {
    range("gridin", grid_channel_start, channels)
}

/// An output-grid entry's resource name: its channel range (`gridout01-08`).
pub fn output_resource(grid_channel_start: u32, channels: u32) -> String {
    range("gridout", grid_channel_start, channels)
}

/// The MXL flow_id of a daemon Sink's mirrored flow on mxl-bridge (a `sink_daemon_id` source).
pub fn sink_flow_id(daemon_id: u8) -> uuid::Uuid {
    bridge().id(&format!("rx{daemon_id:02}"), Kind::Flow)
}

/// mxl-bridge's packed-RX flow of the given IS-08 name (a `packed_rx_name` source).
pub fn packed_rx_flow_id(name: &str) -> uuid::Uuid {
    bridge().id(&format!("packedrx-{name}"), Kind::Flow)
}

/// mxl-bridge's packed-TX flow of the given IS-08 name (a `packed_tx_name` bus target): writing it
/// is how this app feeds the bridge's packed-TX crosspoint.
pub fn packed_tx_flow_id(name: &str) -> uuid::Uuid {
    bridge().id(&format!("packedtx-{name}"), Kind::Flow)
}

pub fn node_id() -> uuid::Uuid {
    naming().app_id(Kind::Node)
}

pub fn device_id() -> uuid::Uuid {
    naming().app_id(Kind::Device)
}

pub fn input_receiver_id(resource: &str) -> uuid::Uuid {
    naming().id(resource, Kind::Receiver)
}

pub fn output_flow_id(resource: &str) -> uuid::Uuid {
    naming().id(resource, Kind::Flow)
}

pub fn output_source_id(resource: &str) -> uuid::Uuid {
    naming().id(resource, Kind::Source)
}

pub fn output_sender_id(resource: &str) -> uuid::Uuid {
    naming().id(resource, Kind::Sender)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_resources_are_channel_ranges() {
        assert_eq!(input_resource(0, 8), "gridin01-08");
        assert_eq!(output_resource(8, 8), "gridout09-16");
        assert_eq!(input_resource(4, 1), "gridin05");
    }

    /// The bridge's ids, as mxl-bridge derives them (same crate, same names).
    #[test]
    fn bridge_ids_follow_the_bridges_names() {
        let b = Naming::new("caspar", "bridge-1");
        assert_eq!(b.id("rx03", Kind::Flow), mxl::naming::id_of("mxl-caspar-bridge-1-rx03", Kind::Flow));
    }
}
