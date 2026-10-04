//! The media function contract (mxl-k8s-operator's `docs/02-media-function-contract.md`, rules
//! `C-*`), through the shared `mxl-function` crate of our MXL fork: the env namespace `BR_`, the
//! preflight, and the health / status / descriptor the contract's routes serve.
//!
//! The bridge's settings stay the JSON document the orchestrator writes (`BR_CONFIG`, else the
//! first argument): a recorded deviation from C-CFG-1 until it is agreed with the operator's
//! authors.

use std::collections::HashMap;

use mxl_function::{EnvConfig, Health};
use serde_json::{Value, json};

use crate::nmos::NmosState;
use crate::nmos::state::{SinkEntry, SourceEntry};

pub const PREFIX: &str = "BR_";
/// The MXL SDK version of our fork this is built against.
pub const LIBMXL: &str = "1.2.0";

pub fn keys() -> Vec<mxl_function::config::Key> {
    mxl_function::config::standard_keys("mxl-bridge.conf")
}

/// What `--emit-type` says beyond the settings.
pub fn type_spec(http_port: u16) -> mxl_function::manifest::TypeSpec {
    mxl_function::manifest::TypeSpec {
        display_name: "ST 2110-30 / AES67 bridge".into(),
        notes: "aes67-linux-daemon Sinks and Sources mirrored as MXL flows over its RAVENNA ALSA device; IS-08 packed flows. The settings are the JSON document at BR_CONFIG.".into(),
        image: "mxl-bridge".into(),
        http_port,
        domain_env: format!("{PREFIX}DOMAIN"),
        config_dir_env: format!("{PREFIX}CONFIG_DIR"),
        needs_pinning: true,
        hardware: serde_json::json!({}),
    }
}

pub fn env() -> EnvConfig {
    EnvConfig::from_env(PREFIX, keys())
}

pub fn allow(env: &EnvConfig) -> mxl_function::preflight::Allow {
    let set = |k: &str| env.get(k).is_some_and(|v| v != "0" && v != "false");
    mxl_function::preflight::Allow { non_tmpfs: set("ALLOW_NON_TMPFS"), unset_tai: set("ALLOW_UNSET_TAI") }
}

/// The sinks and sources, read without blocking the async runtime: the contract's closures are
/// synchronous, the maps are behind tokio mutexes held briefly by the sync / IS-05 paths.
fn with<T, U>(m: &tokio::sync::Mutex<HashMap<u8, T>>, f: impl FnOnce(&HashMap<u8, T>) -> U) -> Option<U> {
    for _ in 0..20 {
        if let Ok(g) = m.try_lock() {
            return Some(f(&g));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    None
}

/// The verdict (C-HLTH-5, C-HLTH-8): a Sink (2110 in, MXL out) whose flow faulted is this
/// bridge's own failure; a Source (MXL in, 2110 out) whose upstream flow faulted is a reason; a
/// clock problem from the preflight degrades (C-ID-13).
pub fn health(s: &NmosState, clock_problem: Option<&str>) -> Health {
    let mut h = Health::ok();
    if let Some(c) = clock_problem {
        h = h.fail(c.to_string());
    }
    let sink_faults = with(&s.sinks, |m| sorted(m.values().filter_map(|e: &SinkEntry| e.fault.clone().map(|f| (e.daemon_id, e.label.clone(), f)))));
    let source_faults = with(&s.sources, |m| sorted(m.values().filter_map(|e: &SourceEntry| e.fault.clone().map(|f| (e.daemon_id, e.label.clone(), f)))));
    match sink_faults {
        Some(v) => {
            for (id, label, f) in v {
                h = h.fail(format!("sink {id} ({label}): {f}"));
            }
        }
        None => h = h.note("the sink state is busy; sink faults not read for this verdict".to_string()),
    }
    for (id, label, f) in source_faults.unwrap_or_default() {
        h = h.note(format!("source {id} ({label}): {f}"));
    }
    h
}

fn sorted(it: impl Iterator<Item = (u8, String, String)>) -> Vec<(u8, String, String)> {
    let mut v: Vec<_> = it.collect();
    v.sort();
    v
}

/// The state document (C-API-5): the mirrored Sinks and Sources, active, fault, their flows.
pub fn status(s: &NmosState) -> Value {
    let sinks = with(&s.sinks, |m| {
        let mut v: Vec<Value> = m
            .values()
            .map(|e| json!({ "id": e.daemon_id, "label": e.label, "channels": e.channels, "active": e.active, "flow_id": e.flow_id.to_string(), "flow_open": e.flow.is_some(), "leases": e.leases, "fault": e.fault }))
            .collect();
        v.sort_by_key(|x| x["id"].as_u64());
        v
    });
    let sources = with(&s.sources, |m| {
        let mut v: Vec<Value> = m
            .values()
            .map(|e| json!({ "id": e.daemon_id, "label": e.label, "channels": e.channels, "active": e.active, "flow_id": e.flow_id, "sender_id": e.sender_id, "fault": e.fault }))
            .collect();
        v.sort_by_key(|x| x["id"].as_u64());
        v
    });
    json!({ "alsa_channels": s.alsa_channels.load(std::sync::atomic::Ordering::Relaxed), "sinks": sinks, "sources": sources })
}

/// The descriptor's own part (C-DESC-2): MXL senders (the Sinks' flows) and receivers (the
/// Sources), bound through its native IS-05; it needs the RAVENNA ALSA device and RT priority.
pub fn descriptor(s: &NmosState) -> Value {
    let senders = with(&s.sinks, |m| m.values().map(|e| json!({ "id": e.sender_id.to_string(), "label": e.label, "format": "urn:x-nmos:format:audio", "channels": e.channels })).collect::<Vec<_>>()).unwrap_or_default();
    let receivers = with(&s.sources, |m| m.values().map(|e| json!({ "id": e.receiver_id.to_string(), "label": e.label, "format": "urn:x-nmos:format:audio", "channels": e.channels })).collect::<Vec<_>>()).unwrap_or_default();
    json!({
        "capabilities": ["st2110-30", "aes67", "is08-channel-mapping"],
        "senders": senders,
        "receivers": receivers,
        "requirements": { "devices": ["/dev/snd"], "capabilities": ["SYS_NICE"], "hostPaths": [], "sysctls": [] }
    })
}
