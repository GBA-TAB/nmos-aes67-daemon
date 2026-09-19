//! A machine-discoverable description of this mixer instance — SESSION-2026-09-18, built for a
//! "command proxy" use case: a separate control surface that hosts UI composed of commands/
//! information from *several* different apps at once, not just this one. Such a proxy can't
//! afford to have this mixer's own param names/types/ranges/units hand-coded into it the way
//! `audiomixer` (this app's own reference dashboard) does — it needs to discover that shape over
//! the wire instead.
//!
//! Two pieces, both under one `amixer/{mixerId}/init` message:
//! - `schema`: static, kind-level metadata (`layouts`, and which params exist per resource kind
//!   with type/unit/range/readonly/applicability) — doesn't change while this process runs.
//! - `topology`: the actual current state — every track/bus/master/grid-entry's own identity
//!   (id/label/channels/layout) *and* live values (gain/fader/mute/sends/chain/...), reusing the
//!   exact same per-field JSON shapes `ws.rs`'s own periodic broadcast and PUT-echo already use,
//!   so a proxy that discovers a resource via `init` recognizes the very same shape when that
//!   resource's own fields show up again later on the ordinary `{"path","value"}` broadcast
//!   stream — one wire shape, not two to keep in sync.
//!
//! Sent two ways (`ws.rs`):
//! - Once, directly to a newly-connected socket, before it even subscribes to the shared
//!   broadcast stream — so a proxy gets the *complete* current picture immediately on connect,
//!   not "whatever happens to arrive over the next several periodic-tick and PUT-echo messages".
//! - Re-broadcast to every connected client whenever the mixer's own topology actually changes
//!   (a track/bus/master CREATE or DELETE, `handle_create`/`handle_delete`) — a proxy that already
//!   built its interface from an earlier `init` gets told to redo that, rather than silently
//!   drifting out of sync with a mixer it thinks looks different than it now does.
//!
//! Deliberately does **not** cover: per-stage (`dsp.rs`) field-level schema (a `chain` entry's own
//! `params` shape varies by `StageKind`, already fully self-describing via `chain_json`'s own
//! `{index,kind,params}` shape — a proxy that already knows `StageKind`'s handful of variants and
//! their own field names doesn't need a separate schema for this); the mixer-level runtime-
//! editable `downmix` coefficient tables (`ws.rs`'s own `"downmix"` PUT kind) — a real gap, but a
//! narrower, lower-priority one than the topology/param schema this closes, left for later.

use crate::layout::ChannelLayout;
use crate::mixer::role_angle;
use crate::ws::WsState;

/// One named layout's own real shape — `roles` in real channel order, each with its short name,
/// its real ADM `speakerLabel` form, and its own real BS.2051 azimuth/elevation when it has one
/// (`M`/`Lfe` don't — see `role_angle`'s own doc comment). Lets a proxy draw a real speaker
/// ring for a track/bus tagged with one of these, the same real angle table `mixer.rs`'s own VBAP
/// panning math already uses, without needing its own hardcoded copy of BS.2051.
fn layouts_json() -> serde_json::Value {
    let named = [
        ChannelLayout::Mono,
        ChannelLayout::Stereo,
        ChannelLayout::Quad,
        ChannelLayout::Surround5_1,
        ChannelLayout::Surround7_1,
        ChannelLayout::Surround5_1_4,
    ];
    serde_json::json!(named
        .iter()
        .map(|&layout| {
            let roles: Vec<serde_json::Value> = layout
                .roles()
                .iter()
                .map(|&role| {
                    let angle = role_angle(role);
                    serde_json::json!({
                        "role": role.short_name(),
                        "adm_speaker_label": role.adm_speaker_label(),
                        "azimuth_deg": angle.map(|(az, _)| az),
                        "elevation_deg": angle.map(|(_, el)| el),
                    })
                })
                .collect();
            (layout_wire_name(layout), serde_json::json!({ "channels": layout.channel_count(), "roles": roles }))
        })
        .collect::<serde_json::Map<_, _>>())
}

/// The exact `#[serde(rename_all = "snake_case")]` wire form `layout.rs`'s own `ChannelLayout`
/// derive already produces on every other path (`channel-list`'s own `"layout"` field, etc.) --
/// re-derived here via a real round-trip rather than hand-typed, so this can never drift from
/// what the rest of the protocol actually sends.
fn layout_wire_name(layout: ChannelLayout) -> String {
    serde_json::to_value(layout).unwrap().as_str().unwrap().to_string()
}

/// One param's own static metadata -- deliberately a plain, flat, hand-built JSON object (not a
/// typed struct/derive) matching this codebase's own existing convention for wire shapes
/// (`ws.rs`'s own `*_json` functions all build `serde_json::json!` literals directly) rather than
/// introducing a second convention just for this module. `unit`/`min`/`max`/`default` omitted
/// (not `null`) when not applicable, so a proxy's own "does this param have a range" check is a
/// plain key-presence test.
fn param(kind: &str, opts: serde_json::Value) -> serde_json::Value {
    let mut obj = serde_json::json!({ "type": kind, "readonly": false });
    if let (Some(o), Some(base)) = (opts.as_object(), obj.as_object_mut()) {
        for (k, v) in o {
            base.insert(k.clone(), v.clone());
        }
    }
    obj
}

fn readonly(mut p: serde_json::Value) -> serde_json::Value {
    p["readonly"] = serde_json::json!(true);
    p
}

/// Every param this app's WS protocol actually supports, per resource kind -- cross-checked
/// directly against `ws.rs`'s own `apply_track_param`/`apply_bus_param`/`apply_master_param`/
/// `current_track_value`/`current_bus_value`/`current_master_value` match arms and
/// `run_meter_broadcaster`'s own periodic-tick publish list, not guessed from memory. `stage/{N}`
/// (a track's/master's own per-slot chain editing, `dsp.rs::StageKind`-specific fields) is
/// deliberately not enumerated per param name here -- see this module's own top doc comment for
/// why `chain`'s own `{index,kind,params}` shape already self-describes it.
fn param_schemas_json() -> serde_json::Value {
    let send_shape = serde_json::json!({
        "bus_id": "int", "on": "bool", "level_db": "float (dB, -60..12)", "pickoff": "\"pre_fader\"|\"post_fader\"",
        "rotation_deg": "float (deg, meaning depends on pan_mode/pan_object)", "elevation_deg": "float (deg)",
        "pan_mode": "\"auto\"|\"adm\"|\"route\" -- see mixer::SendPanMode", "pan_object": "readonly: the live PanObject::classify result for \"auto\" mode",
        "route": "null | bool[track_channels][bus_channels] -- only applied when pan_mode==\"route\"",
    });
    let adm_object_shape = serde_json::json!({
        "name": "string", "gain_db": "float (dB)",
        "position": { "azimuth": "float (deg)", "elevation": "float (deg)", "distance": "float (1.0 == reference distance)" },
        "width": "float (0..1)", "height": "float (0..1)", "depth": "float (0..1)",
    });
    let patch_entry_shape = "null | {\"source\": \"input:<id>\"|\"track-out:<id>\"|\"bus-out:<id>\"|\"master-out:<id>\", \"channel\": int}";

    let channel = serde_json::json!({
        "gain": param("float", serde_json::json!({"unit": "dB", "min": -20, "max": 20, "default": 0})),
        "fader": param("float", serde_json::json!({"unit": "dB", "min": -128, "max": 12, "default": 0})),
        "lfe-trim": param("float", serde_json::json!({"unit": "dB", "min": -20, "max": 12, "default": 0, "applicability": "layout has an Lfe role (see schema.layouts)"})),
        "mute": param("bool", serde_json::json!({"default": false})),
        "solo": param("bool", serde_json::json!({"default": false})),
        "sends": param("array", serde_json::json!({"item": send_shape, "default": []})),
        "chain": readonly(param("array", serde_json::json!({"item": "see chain's own {index,kind,params} shape, self-describing"}))),
        "adm-objects": param("array", serde_json::json!({"item": adm_object_shape, "length": "always exactly this track's own channel count"})),
        "input-patch": param("array", serde_json::json!({"item": patch_entry_shape, "length": "this track's own channel count"})),
        "peakmeter": readonly(param("array", serde_json::json!({"item": "float (dBFS, or null for silence/no signal)"}))),
        "input-meter": readonly(param("array", serde_json::json!({"item": "float (dBFS, or null)", "note": "measured before gain, at the input-patch pickoff point"}))),
        "compensation-delay-ms": readonly(param("float", serde_json::json!({"unit": "ms"}))),
    });
    let sum = serde_json::json!({
        "input-patch": param("array", serde_json::json!({"item": patch_entry_shape, "summing": true, "note": "a bus is a pure summer -- no gain/fader/mute of its own"})),
        "master-sends": param("array", serde_json::json!({"item": send_shape, "note": "same shape as a channel's own sends, minus pickoff (a bus has no fader stage to tap pre/post of)"})),
        "peakmeter": readonly(param("array", serde_json::json!({"item": "float (dBFS) | null"}))),
        "input-meter": readonly(param("array", serde_json::json!({"item": "float (dBFS) | null"}))),
    });
    let master = serde_json::json!({
        "fader": param("float", serde_json::json!({"unit": "dB", "min": -128, "max": 12, "default": 0})),
        "mute": param("bool", serde_json::json!({"default": false})),
        "chain": readonly(param("array", serde_json::json!({"item": "see channel.chain"}))),
        "master-sends": param("array", serde_json::json!({"item": send_shape})),
        "input-patch": param("array", serde_json::json!({"item": patch_entry_shape, "summing": true})),
        "peakmeter": readonly(param("array", serde_json::json!({"item": "float (dBFS) | null"}))),
        "input-meter": readonly(param("array", serde_json::json!({"item": "float (dBFS) | null"}))),
        "compensation-delay-ms": readonly(param("float", serde_json::json!({"unit": "ms"}))),
    });
    let input = serde_json::json!({
        "label": param("string", serde_json::json!({})),
        "channel_labels": readonly(param("array", serde_json::json!({"item": "string", "note": "this entry's own per-channel identity within the whole input grid's shared running numbering"}))),
        "peakmeter": readonly(param("array", serde_json::json!({"item": "float (dBFS) | null"}))),
    });
    let output = serde_json::json!({
        "label": param("string", serde_json::json!({})),
        "patch": param("array", serde_json::json!({"item": patch_entry_shape, "note": "exclusive -- one source per channel, like output.channels[i]"})),
        "peakmeter": readonly(param("array", serde_json::json!({"item": "float (dBFS) | null"}))),
    });

    serde_json::json!({ "channel": channel, "sum": sum, "master": master, "input": input, "output": output })
}

/// The mixer's own actual current state -- every track/bus/master/grid-entry's real identity
/// (id/label/channels/layout) plus its live values, reusing the exact same per-field JSON
/// `ws.rs`'s own periodic broadcast/PUT-echo paths already produce (`sends_json`/`chain_json`/
/// `adm_objects_json`/`master_sends_json`/`PatchState::*_json`) rather than a second, parallel
/// serialization a future change could silently let drift from the real wire shape.
fn topology_json(state: &WsState) -> serde_json::Value {
    let mixer = &state.mixer;
    let tracks = mixer.tracks_snapshot();
    let buses = mixer.buses_snapshot();
    let masters = mixer.masters_snapshot();

    let channels: Vec<serde_json::Value> = tracks
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id, "label": t.label, "channels": t.channels, "layout": t.layout,
                "gain": *t.gain_db.lock().unwrap(),
                "fader": *t.fader_db.lock().unwrap(),
                "lfe-trim": *t.lfe_trim_db.lock().unwrap(),
                "mute": t.mute.load(std::sync::atomic::Ordering::Relaxed),
                "solo": t.solo.load(std::sync::atomic::Ordering::Relaxed),
                "sends": crate::ws::sends_json(t, mixer),
                "chain": crate::ws::chain_json(&t.chain),
                "adm-objects": crate::ws::adm_objects_json(t),
                "input-patch": mixer.patch.track_in_json(t.id, t.channels),
            })
        })
        .collect();
    let sums: Vec<serde_json::Value> = buses
        .iter()
        .map(|b| {
            serde_json::json!({
                "id": b.id, "label": b.label, "channels": b.channels, "layout": b.layout,
                "input-patch": mixer.patch.bus_in_json(b.id, b.channels),
                "master-sends": crate::ws::master_sends_json(&b.master_sends.lock().unwrap(), b.layout, mixer),
            })
        })
        .collect();
    let master_list: Vec<serde_json::Value> = masters
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id, "label": m.label, "channels": m.channels, "layout": m.layout,
                "fader": *m.fader_db.lock().unwrap(),
                "mute": m.mute.load(std::sync::atomic::Ordering::Relaxed),
                "chain": crate::ws::chain_json(&m.chain),
                "master-sends": crate::ws::master_sends_json(&m.master_sends.lock().unwrap(), m.layout, mixer),
                "input-patch": mixer.patch.master_in_json(m.id, m.channels),
            })
        })
        .collect();

    serde_json::json!({
        "channels": channels,
        "sums": sums,
        "masters": master_list,
        "input_grid": mixer.input_grid.list_json(),
        "output_grid": mixer.output_grid.list_json(),
    })
}

/// The full `amixer/{mixerId}/init` payload -- see this module's own top doc comment for when/how
/// `ws.rs` sends this (once per new connection, and re-broadcast to everyone on any topology
/// change). `schema` is cheap to rebuild every call (a handful of small, static JSON literals) --
/// deliberately not cached, so there's no staleness question to reason about.
///
/// `path_prefix`/`collections`/`kind_labels` exist for one consumer: a generic aggregating proxy
/// (mxl-proxy) whose own UI client is built to render *any* backend's schema, not just this app's
/// -- a genuinely different backend (e.g. mxl-signal-gen's REST control surface) has no "amixer/
/// {mixerId}" path segment and no reason to name its topology collections in mixer.rs's own
/// plural-vs-singular convention (`channel` param-schema key vs `channels` topology key). Rather
/// than bake this app's own naming quirks into the proxy client as a hardcoded assumption, this
/// app just states them: `path_prefix` is what a proxy must splice between the mixer id and a
/// `<kind>/<id>/<param>` triple to get this app's own real WS path; `collections` maps each
/// `schema.params` kind to its `topology` array's own key; `kind_labels` is a plain display label
/// per kind, purely cosmetic. A backend with no such quirks (kind name == collection key, e.g.
/// mxl-signal-gen) can omit these entirely -- the proxy client falls back to identity/auto-labels.
pub fn init_json(state: &WsState) -> serde_json::Value {
    serde_json::json!({
        "mixer_id": state.mixer_id,
        "path_prefix": format!("amixer/{}", state.mixer_id),
        "collections": {
            "channel": "channels", "sum": "sums", "master": "masters",
            "input": "input_grid", "output": "output_grid",
        },
        "kind_labels": {
            "channel": "Channels", "sum": "Buses", "master": "Masters",
            "input": "Input grid", "output": "Output grid",
        },
        "schema": {
            "layouts": layouts_json(),
            "params": param_schemas_json(),
        },
        "topology": topology_json(state),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_wire_name_matches_the_real_serde_rename() {
        // Cross-check against a real round-trip rather than hand-typing the expected strings --
        // this is exactly the "never drift from what the wire actually sends" property this
        // function's own doc comment claims.
        assert_eq!(layout_wire_name(ChannelLayout::Surround5_1), "surround5_1");
        assert_eq!(layout_wire_name(ChannelLayout::Surround5_1_4), "surround5_1_4");
        assert_eq!(layout_wire_name(ChannelLayout::Mono), "mono");
    }

    #[test]
    fn layouts_json_covers_every_named_layout_with_the_right_channel_count() {
        let json = layouts_json();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 6); // Mono/Stereo/Quad/5.1/7.1/5.1.4 -- Discrete deliberately excluded, see its own doc comment
        assert_eq!(obj["surround5_1"]["channels"], 6);
        assert_eq!(obj["surround5_1"]["roles"].as_array().unwrap().len(), 6);
        assert_eq!(obj["mono"]["channels"], 1);
    }

    #[test]
    fn layouts_json_carries_the_real_role_angles_and_omits_them_where_none_exist() {
        let json = layouts_json();
        let roles = json["surround5_1"]["roles"].as_array().unwrap();
        let l = roles.iter().find(|r| r["role"] == "L").unwrap();
        assert_eq!(l["azimuth_deg"], 30.0);
        assert_eq!(l["adm_speaker_label"], "RC_L");
        // Lfe has no real BS.2051 position (role_angle's own doc comment) -- null, not a made-up angle.
        let lfe = roles.iter().find(|r| r["role"] == "LFE").unwrap();
        assert!(lfe["azimuth_deg"].is_null());
    }

    #[test]
    fn param_schemas_json_covers_every_resource_kind_with_real_params() {
        let json = param_schemas_json();
        for kind in ["channel", "sum", "master", "input", "output"] {
            let params = json[kind].as_object().unwrap_or_else(|| panic!("missing param schema for kind {kind}"));
            assert!(!params.is_empty(), "kind {kind} has no params at all");
        }
        // Spot-check a few real, specific entries rather than just "is present" for everything --
        // catches a field silently renamed/dropped in a future edit, not just a missing kind.
        assert_eq!(json["channel"]["gain"]["unit"], "dB");
        assert_eq!(json["channel"]["gain"]["min"], -20);
        assert_eq!(json["channel"]["mute"]["type"], "bool");
        assert_eq!(json["channel"]["peakmeter"]["readonly"], true);
        assert_eq!(json["channel"]["fader"]["readonly"], false);
        assert!(json["sum"].as_object().unwrap().contains_key("master-sends"));
        assert!(!json["sum"].as_object().unwrap().contains_key("gain")); // a bus is a pure summer, no gain of its own
        assert!(json["input"].as_object().unwrap().contains_key("label"));
        assert!(json["output"].as_object().unwrap().contains_key("patch"));
    }
}
