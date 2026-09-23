//! ADM (ITU-R BS.2076) object-audio metadata — a real position/gain/extent data model for a track
//! explicitly authored as an ADM audio object, plus Serial ADM (ITU-R BS.2125) XML export/import
//! on top of it (`to_sadm_xml`/`from_sadm_xml`, below the data-model section).
//!
//! The S-ADM `<frame>`/`<frameHeader>`/`<audioFormatExtended>` structure and every element/
//! attribute name used by `to_sadm_xml` (`audioObject`, `audioPackFormat`, `audioChannelFormat`,
//! `audioBlockFormat`, `position` with its `coordinate` attribute, `speakerLabel`, `typeLabel`/
//! `typeDefinition` codes `0001`=DirectSpeakers/`0003`=Objects) were verified directly against
//! real, accepted test fixtures in `ebu/libadm` (a maintained, spec-conformant ADM library) —
//! `tests/test_data/simple_scene_itu.accepted.xml` (full `audioObject`/`audioPackFormat`/
//! `audioChannelFormat`/`audioBlockFormat` shape) and `tests/test_data/write_total_time_reference.
//! accepted.xml` (the real `<frame version="ITU-R_BS.2125-1">`/`<frameHeader><frameFormat .../>
//! </frameHeader>` wrapper) — fetched via `gh api repos/ebu/libadm/contents/...`, not guessed or
//! reconstructed from memory.
//!
//! Position representation and semantics verified directly against BS.2076's own
//! `audioBlockFormat` Objects typeDefinition (via the EBU ADM Guidelines' own worked
//! coordinate-system example, itself sourced from the ITU-R text — not guessed): **spherical**,
//! not Cartesian — `azimuth` in degrees (`0` = straight ahead, positive = left/anticlockwise
//! viewed from above), `elevation` in degrees (`0` = horizontal, positive = up), `distance`
//! normalized (`1.0` = the reference/default distance). BS.2076 also allows an alternative
//! Cartesian (X/Y/Z) representation for the same `<position>` element (via its `coordinate`
//! attribute) — not modeled here, spherical only, matching the worked examples and this app's
//! present needs.
//!
//! `gain_db`/`width`/`height`/`depth` follow this app's own existing conventions where they
//! differ from ADM's own defaults, to be converted only at the real external boundary (Phase E's
//! Serial ADM XML), not stored pre-converted: ADM's own `<gain>` element is a **linear**
//! multiplier (default `1.0`), not dB — `gain_db` here stays dB to match every other gain-like
//! field already in this codebase (`mixer::Track::gain_db`/`fader_db`, `mixer::db_to_linear`), the
//! same "one consistent internal unit, convert only at a real external boundary" precedent this
//! app already follows elsewhere. `width`/`height`/`depth` (object extent, each conventionally
//! `0.0..=1.0`, `0.0` = point source) are real BS.2076 `audioBlockFormat` Objects elements, used
//! here exactly as the spec defines them — no unit conversion needed.

use serde::{Deserialize, Serialize};

/// A position in ADM's spherical coordinate system — see this module's own doc comment for the
/// verified semantics of each field.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq)]
pub struct AdmPosition {
    #[serde(default)]
    pub azimuth: f64,
    #[serde(default)]
    pub elevation: f64,
    #[serde(default = "default_distance")]
    pub distance: f64,
}

fn default_distance() -> f64 {
    1.0
}

impl Default for AdmPosition {
    fn default() -> Self {
        Self { azimuth: 0.0, elevation: 0.0, distance: default_distance() }
    }
}

/// One ADM audio object's live metadata — a track carries zero or more of these, one per channel
/// (`TrackConfig.adm_objects`/`mixer::Track.adm_objects`, `config.rs`), controllable live over WS
/// (`channel/{id}/adm-objects`, `ws.rs`, one PUT replaces every channel's object at once), and
/// persisted the same way every other live track value is (`persistence.rs`).
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Default)]
pub struct AdmObjectMetadata {
    /// The ADM `audioObjectName` — independent of the track's own `label` (a track's label is a
    /// console-UI concept; this is the name that ends up in exported Serial ADM XML, Phase E).
    pub name: String,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub position: AdmPosition,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
    #[serde(default)]
    pub depth: f64,
}

// --- Serial ADM (ITU-R BS.2125) XML export/import -------------------------------------------
//
// Scope (matches the approved plan's explicit boundary): this app has no real ADM timeline (no
// live wire transport this pass, see the plan's "explicitly out of scope" section) — `to_sadm_xml`
// exports a single-frame *snapshot* of the current live state, not a real streaming position, so
// every `audioBlockFormat` uses a fixed nominal `rtime`/`duration` rather than a meaningful
// timestamp. `audioTrackUID`/`audioStreamFormat`/`audioTrackFormat`/`audioProgramme`/
// `audioContent` (the elements that link ADM metadata to actual PCM track numbers within a real
// file/stream) are deliberately omitted — this app has no real track-number binding to report
// (that's exactly the live-wire-transport work explicitly out of scope this pass).
//
// `from_sadm_xml` parses "enough to round-trip what `to_sadm_xml` itself produces" (the plan's own
// scoping language), not a fully general ADM document parser: it matches an `audioObject`'s
// position/gain/extent by *document order* against the `audioBlockFormat`s of `Objects`-typed
// `audioChannelFormat`s, exactly the order `to_sadm_xml` itself emits them in (one object, then
// its own pack/channel/block, then the next object, ...). A hand-authored or reordered S-ADM
// document that doesn't follow that emission order will not round-trip correctly through this
// parser — resolving the real `audioPackFormatIDRef`/`audioChannelFormatIDRef` cross-references
// properly would need a much larger general-purpose ADM parser, out of scope for this pass.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};
use std::io::Cursor;

/// One resolved update from a parsed S-ADM document, keyed by `audioObjectName` — applied back
/// onto whichever live track *channel* currently carries that name by `apply_sadm_updates`
/// (`nmos/server.rs`'s import route), matching one entry of `mixer::Track.adm_objects`'s own
/// `name` field.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmObjectUpdate {
    pub name: String,
    pub metadata: AdmObjectMetadata,
}

fn write_text_element(writer: &mut Writer<Cursor<Vec<u8>>>, tag: &str, text: &str) {
    writer.write_event(Event::Start(BytesStart::new(tag))).unwrap();
    writer.write_event(Event::Text(BytesText::new(text))).unwrap();
    writer.write_event(Event::End(BytesEnd::new(tag))).unwrap();
}

fn write_position_element(writer: &mut Writer<Cursor<Vec<u8>>>, coordinate: &str, value: f64) {
    let mut el = BytesStart::new("position");
    el.push_attribute(("coordinate", coordinate));
    writer.write_event(Event::Start(el)).unwrap();
    writer.write_event(Event::Text(BytesText::new(&format!("{value:.6}")))).unwrap();
    writer.write_event(Event::End(BytesEnd::new("position"))).unwrap();
}

fn write_audio_object(writer: &mut Writer<Cursor<Vec<u8>>>, id: u32, meta: &AdmObjectMetadata) {
    let ao_id = format!("AO_{id:04}");
    let ap_id = format!("AP_0003{id:04}");
    let ac_id = format!("AC_0003{id:04}");
    let ab_id = format!("AB_0003{id:04}_00000001");

    let mut ao = BytesStart::new("audioObject");
    ao.push_attribute(("audioObjectID", ao_id.as_str()));
    ao.push_attribute(("audioObjectName", meta.name.as_str()));
    ao.push_attribute(("start", "00:00:00.00000"));
    writer.write_event(Event::Start(ao)).unwrap();
    write_text_element(writer, "audioPackFormatIDRef", &ap_id);
    write_text_element(writer, "gain", &format!("{:.6}", crate::mixer::db_to_linear(meta.gain_db)));
    writer.write_event(Event::End(BytesEnd::new("audioObject"))).unwrap();

    let mut ap = BytesStart::new("audioPackFormat");
    ap.push_attribute(("audioPackFormatID", ap_id.as_str()));
    ap.push_attribute(("audioPackFormatName", meta.name.as_str()));
    ap.push_attribute(("typeLabel", "0003"));
    ap.push_attribute(("typeDefinition", "Objects"));
    writer.write_event(Event::Start(ap)).unwrap();
    write_text_element(writer, "audioChannelFormatIDRef", &ac_id);
    writer.write_event(Event::End(BytesEnd::new("audioPackFormat"))).unwrap();

    let mut ac = BytesStart::new("audioChannelFormat");
    ac.push_attribute(("audioChannelFormatID", ac_id.as_str()));
    ac.push_attribute(("audioChannelFormatName", meta.name.as_str()));
    ac.push_attribute(("typeLabel", "0003"));
    ac.push_attribute(("typeDefinition", "Objects"));
    writer.write_event(Event::Start(ac)).unwrap();

    let mut ab = BytesStart::new("audioBlockFormat");
    ab.push_attribute(("audioBlockFormatID", ab_id.as_str()));
    ab.push_attribute(("rtime", "00:00:00.00000"));
    ab.push_attribute(("duration", "00:00:01.00000"));
    writer.write_event(Event::Start(ab)).unwrap();
    write_position_element(writer, "azimuth", meta.position.azimuth);
    write_position_element(writer, "elevation", meta.position.elevation);
    write_position_element(writer, "distance", meta.position.distance);
    write_text_element(writer, "width", &format!("{:.6}", meta.width));
    write_text_element(writer, "height", &format!("{:.6}", meta.height));
    write_text_element(writer, "depth", &format!("{:.6}", meta.depth));
    write_text_element(writer, "gain", &format!("{:.6}", crate::mixer::db_to_linear(meta.gain_db)));
    writer.write_event(Event::End(BytesEnd::new("audioBlockFormat"))).unwrap();

    writer.write_event(Event::End(BytesEnd::new("audioChannelFormat"))).unwrap();
}

/// Writes one real BS.2051 DirectSpeakers `audioPackFormat` + one `audioChannelFormat` per channel
/// (the real ADM DirectSpeakers shape: each channel is its own `audioChannelFormat`, all
/// referenced from one shared pack) for a bed track/bus with a known named layout — using
/// `ChannelRole::adm_speaker_label()` (`layout.rs`, itself verified against the Dolby Atmos Master
/// ADM Profile's own common-definitions table). `next_id` is a shared, document-wide counter so
/// every id in the whole exported document stays unique regardless of how many objects/beds
/// precede this one.
fn write_bed_channel_format(writer: &mut Writer<Cursor<Vec<u8>>>, next_id: &mut u32, name: &str, layout: crate::layout::ChannelLayout) {
    *next_id += 1;
    let pack_id = format!("AP_0001{:04}", *next_id);
    let roles = layout.roles();
    // (channel format id, its own bare 4-digit instance number reused for that channel's block id)
    let channels: Vec<(String, u32)> = roles
        .iter()
        .map(|_| {
            *next_id += 1;
            (format!("AC_0001{:04}", *next_id), *next_id)
        })
        .collect();

    let mut ap = BytesStart::new("audioPackFormat");
    ap.push_attribute(("audioPackFormatID", pack_id.as_str()));
    ap.push_attribute(("audioPackFormatName", name));
    ap.push_attribute(("typeLabel", "0001"));
    ap.push_attribute(("typeDefinition", "DirectSpeakers"));
    writer.write_event(Event::Start(ap)).unwrap();
    for (chan_id, _) in &channels {
        write_text_element(writer, "audioChannelFormatIDRef", chan_id);
    }
    writer.write_event(Event::End(BytesEnd::new("audioPackFormat"))).unwrap();

    for (role, (chan_id, chan_num)) in roles.iter().zip(channels.iter()) {
        let mut ac = BytesStart::new("audioChannelFormat");
        ac.push_attribute(("audioChannelFormatID", chan_id.as_str()));
        ac.push_attribute(("audioChannelFormatName", role.short_name()));
        ac.push_attribute(("typeLabel", "0001"));
        ac.push_attribute(("typeDefinition", "DirectSpeakers"));
        writer.write_event(Event::Start(ac)).unwrap();
        let block_id = format!("AB_0001{chan_num:04}_00000001");
        let mut ab = BytesStart::new("audioBlockFormat");
        ab.push_attribute(("audioBlockFormatID", block_id.as_str()));
        writer.write_event(Event::Start(ab)).unwrap();
        write_text_element(writer, "speakerLabel", role.adm_speaker_label());
        writer.write_event(Event::End(BytesEnd::new("audioBlockFormat"))).unwrap();
        writer.write_event(Event::End(BytesEnd::new("audioChannelFormat"))).unwrap();
    }
}

/// Exports a single-frame Serial ADM (ITU-R BS.2125) XML snapshot of `mixer`'s current live
/// state: one real ADM object (position/gain/extent) per *channel* of every track with non-empty
/// `adm_objects` (a genuinely multi-object track emits one independent `audioObject`/
/// `audioPackFormat`/`audioChannelFormat` per channel, not one shared between them), plus one real
/// DirectSpeakers bed (`write_bed_channel_format`) per remaining track with a known named `layout`
/// — see this section's own module-level doc comment for the exact, deliberate scope boundary (no
/// timeline, no track/stream-number binding).
pub fn to_sadm_xml(mixer: &crate::engine::MixerState) -> String {
    let mut writer = Writer::new_with_indent(Cursor::new(Vec::new()), b' ', 2);
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("utf-8"), None))).unwrap();

    let mut frame = BytesStart::new("frame");
    frame.push_attribute(("version", "ITU-R_BS.2125-1"));
    writer.write_event(Event::Start(frame)).unwrap();

    writer.write_event(Event::Start(BytesStart::new("frameHeader"))).unwrap();
    let mut ff = BytesStart::new("frameFormat");
    ff.push_attribute(("frameFormatID", "FF_00000001"));
    ff.push_attribute(("start", "00:00:00.00000"));
    ff.push_attribute(("duration", "00:00:01.00000"));
    ff.push_attribute(("type", "full"));
    ff.push_attribute(("timeReference", "total"));
    writer.write_event(Event::Empty(ff)).unwrap();
    writer.write_event(Event::End(BytesEnd::new("frameHeader"))).unwrap();

    writer.write_event(Event::Start(BytesStart::new("audioFormatExtended"))).unwrap();

    let tracks = mixer.tracks_snapshot();
    let mut next_id: u32 = 1000;
    // A track's own adm_objects now always exists (SESSION-2026-09-18, mixer::Track.adm_objects's
    // own doc comment: latent per-channel position data, present whether or not it's actually
    // being used) -- what decides whether this track exports as real ADM audioObjects vs. a plain
    // bed channel format is now whether any of its own sends actually has SendPanMode::Adm
    // selected, not whether adm_objects itself is non-empty (that's true for every track).
    let is_adm_active = |track: &std::sync::Arc<crate::mixer::Track>| {
        track.sends.lock().unwrap().iter().any(|s| *s.pan_mode.lock().unwrap() == crate::mixer::SendPanMode::Adm)
    };
    for track in &tracks {
        if !is_adm_active(track) {
            continue;
        }
        for slot in &track.adm_objects {
            next_id += 1;
            let meta = slot.lock().unwrap().clone();
            write_audio_object(&mut writer, next_id, &meta);
        }
    }
    for track in &tracks {
        if is_adm_active(track) {
            continue;
        }
        let Some(layout) = track.layout else { continue };
        if layout.roles().is_empty() {
            continue;
        }
        write_bed_channel_format(&mut writer, &mut next_id, &track.label.lock().unwrap(), layout);
    }

    writer.write_event(Event::End(BytesEnd::new("audioFormatExtended"))).unwrap();
    writer.write_event(Event::End(BytesEnd::new("frame"))).unwrap();

    String::from_utf8(writer.into_inner().into_inner()).expect("quick-xml only ever writes valid UTF-8")
}

/// Parses a Serial ADM XML document (as produced by `to_sadm_xml`, or anything following the same
/// document-order convention — see this section's own module-level doc comment for the precise
/// scope) into one `AdmObjectUpdate` per `audioObject` element found, matched by document order
/// against the `audioBlockFormat`s of `Objects`-typed `audioChannelFormat`s.
pub fn from_sadm_xml(xml: &str) -> anyhow::Result<Vec<AdmObjectUpdate>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut object_names: Vec<String> = Vec::new();
    let mut object_gains_linear: Vec<f32> = Vec::new();
    let mut blocks: Vec<AdmObjectMetadata> = Vec::new();

    let mut in_objects_channel_format = false;
    let mut in_block = false;
    let mut cur_position = AdmPosition::default();
    let mut cur_width = 0.0f64;
    let mut cur_height = 0.0f64;
    let mut cur_depth = 0.0f64;
    let mut cur_block_gain_linear = 1.0f32;
    let mut cur_position_coord: Option<String> = None;
    let mut cur_text_tag: Option<String> = None;
    let mut cur_object_name: Option<String> = None;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match name.as_str() {
                    "audioObject" => {
                        cur_object_name = e
                            .attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == b"audioObjectName")
                            .map(|a| a.decode_and_unescape_value(reader.decoder()).unwrap_or_default().to_string());
                    }
                    "audioChannelFormat" => {
                        let type_def = e
                            .attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == b"typeDefinition")
                            .map(|a| a.decode_and_unescape_value(reader.decoder()).unwrap_or_default().to_string());
                        in_objects_channel_format = type_def.as_deref() == Some("Objects");
                    }
                    "audioBlockFormat" if in_objects_channel_format => {
                        in_block = true;
                        cur_position = AdmPosition::default();
                        cur_width = 0.0;
                        cur_height = 0.0;
                        cur_depth = 0.0;
                        cur_block_gain_linear = 1.0;
                    }
                    "position" if in_block => {
                        cur_position_coord = e
                            .attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == b"coordinate")
                            .map(|a| a.decode_and_unescape_value(reader.decoder()).unwrap_or_default().to_string());
                    }
                    _ => {}
                }
                if in_block {
                    cur_text_tag = Some(name);
                } else if name == "gain" && cur_object_name.is_some() {
                    cur_text_tag = Some(name);
                }
            }
            Event::Text(e) => {
                let text = e.unescape()?.to_string();
                match cur_text_tag.as_deref() {
                    Some("position") => {
                        let v: f64 = text.trim().parse().unwrap_or(0.0);
                        match cur_position_coord.as_deref() {
                            Some("azimuth") => cur_position.azimuth = v,
                            Some("elevation") => cur_position.elevation = v,
                            Some("distance") => cur_position.distance = v,
                            _ => {}
                        }
                    }
                    Some("width") => cur_width = text.trim().parse().unwrap_or(0.0),
                    Some("height") => cur_height = text.trim().parse().unwrap_or(0.0),
                    Some("depth") => cur_depth = text.trim().parse().unwrap_or(0.0),
                    Some("gain") => {
                        let v: f32 = text.trim().parse().unwrap_or(1.0);
                        if in_block {
                            cur_block_gain_linear = v;
                        } else {
                            object_gains_linear.push(v);
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match name.as_str() {
                    "audioObject" => {
                        if let Some(n) = cur_object_name.take() {
                            object_names.push(n);
                            // A missing `<gain>` for this object hasn't pushed a value yet -- keep
                            // the lists index-aligned by defaulting to unity (0 dB) here too.
                            if object_gains_linear.len() < object_names.len() {
                                object_gains_linear.push(1.0);
                            }
                        }
                    }
                    "audioBlockFormat" if in_block => {
                        in_block = false;
                        blocks.push(AdmObjectMetadata {
                            name: String::new(),
                            gain_db: crate::mixer::linear_to_db(cur_block_gain_linear),
                            position: cur_position,
                            width: cur_width,
                            height: cur_height,
                            depth: cur_depth,
                        });
                    }
                    "audioChannelFormat" => in_objects_channel_format = false,
                    _ => {}
                }
                cur_text_tag = None;
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(object_names
        .into_iter()
        .zip(blocks)
        .map(|(name, mut meta)| {
            meta.name = name.clone();
            AdmObjectUpdate { name, metadata: meta }
        })
        .collect())
}

/// Applies every parsed update onto whichever live track *channel* whose own object name matches,
/// by name — silently ignores an update whose name matches no current ADM-object channel (same
/// "trust the client, don't guard every possible misuse" posture `topology.rs`'s own docs describe
/// elsewhere), since a name collision or a stale export is a real, recoverable situation, not a
/// hard error. A name collision across two different channels (same track or different tracks)
/// applies the update to the *first* one found, same as it always would have for two whole tracks
/// sharing a name before multi-object tracks existed -- not a new caveat, just a finer grain.
pub fn apply_sadm_updates(mixer: &crate::engine::MixerState, updates: &[AdmObjectUpdate]) -> usize {
    let mut applied = 0;
    for track in mixer.tracks_snapshot() {
        for slot in &track.adm_objects {
            let current_name = slot.lock().unwrap().name.clone();
            if let Some(update) = updates.iter().find(|u| u.name == current_name) {
                *slot.lock().unwrap() = update.metadata.clone();
                applied += 1;
            }
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelTemplate, TrackConfig};
    use crate::engine::MixerState;
    use crate::mixer::Track;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};

    /// Same minimal-`MixerState` construction trick as `persistence.rs`'s own `test_mixer` (a real
    /// `Bus` needs a real MXL flow writer, not constructible in a plain unit test) -- here scoped
    /// to exactly the tracks `to_sadm_xml` needs to see.
    fn mixer_with_tracks(tracks: Vec<Arc<Track>>) -> MixerState {
        MixerState {
            tracks: Mutex::new(tracks.into_iter().map(|t| (t.id, t)).collect::<HashMap<_, _>>()),
            buses: Mutex::new(HashMap::new()),
            masters: Mutex::new(HashMap::new()),
            input_grid: crate::patch::InputGrid::default(),
            output_grid: crate::patch::OutputGrid::default(),
            app_input_grid: crate::patch::AppInputGrid::default(),
            patch: crate::patch::PatchState::default(),
            default_channels: 2,
            topology_generation: AtomicU64::new(0),
            fault_notify_tx: tokio::sync::mpsc::unbounded_channel().0,
            fault_notify_rx: Mutex::new(None),
            period_frames: 480,
            sample_rate: 48000,
            mxl_domain: String::new(),
            mxl_so_path: std::path::PathBuf::new(),
            downmix_table: crate::mixer::DownmixTable::new(),
        }
    }

    /// One track carrying `metas.len()` independent ADM objects, one per channel -- the channel
    /// count itself comes from `metas.len()`, matching `topology::build_track`'s own real
    /// validation (adm_objects must be empty or exactly `channels` long). Carries one send in
    /// `SendPanMode::Adm` -- SESSION-2026-09-18: `to_sadm_xml` now exports a track as real ADM
    /// audioObjects only when at least one of its own sends actually has Adm mode selected (every
    /// track has latent adm_objects now, so non-emptiness alone no longer means anything).
    fn object_track(id: u32, metas: Vec<AdmObjectMetadata>) -> Arc<Track> {
        let channels = metas.len();
        Arc::new(Track::new(
            &TrackConfig {
                id,
                label: format!("T{id}"),
                channels: None,
                layout: None,
                adm_objects: metas,
                auto_input: None, lfe_trim_db: 0.0,
                sends: vec![crate::config::SendConfig {
                    bus_id: 1,
                    on: true,
                    level_db: 0.0,
                    pickoff: Default::default(),
                    rotation_deg: 0.0,
                    elevation_deg: 0.0,
                    route: None,
                    pan_mode: "adm".to_string(),
                }],
                gain_db: 0.0,
                fader_db: 0.0,
                template: ChannelTemplate::Simple,
                chain: vec![],
            },
            channels,
            48000,
        ))
    }

    fn bed_track(id: u32, layout: crate::layout::ChannelLayout) -> Arc<Track> {
        Arc::new(Track::new(
            &TrackConfig {
                id,
                label: format!("Bed{id}"),
                channels: None,
                layout: Some(layout),
                adm_objects: vec![],
                auto_input: None, lfe_trim_db: 0.0,
                sends: vec![],
                gain_db: 0.0,
                fader_db: 0.0,
                template: ChannelTemplate::Simple,
                chain: vec![],
            },
            layout.channel_count() as usize,
            48000,
        ))
    }

    #[test]
    fn to_sadm_xml_emits_the_real_frame_wrapper_and_object_shape() {
        let meta = AdmObjectMetadata {
            name: "Dialogue".into(),
            gain_db: 0.0,
            position: AdmPosition { azimuth: 30.0, elevation: 0.0, distance: 1.0 },
            width: 0.0,
            height: 0.0,
            depth: 0.0,
        };
        let mixer = mixer_with_tracks(vec![object_track(1, vec![meta])]);
        let xml = to_sadm_xml(&mixer);
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(xml.contains("<frame version=\"ITU-R_BS.2125-1\">"));
        assert!(xml.contains("<frameFormat"));
        assert!(xml.contains("audioObjectName=\"Dialogue\""));
        assert!(xml.contains("typeDefinition=\"Objects\""));
        assert!(xml.contains("coordinate=\"azimuth\">30.000000</position>"));
    }

    #[test]
    fn to_sadm_xml_emits_one_independent_audio_object_per_channel_of_a_multi_object_track() {
        // The core of the multi-object feature: an 8-channel track carrying 8 independent
        // AdmObjectMetadata entries must export 8 real, independently-positioned audioObjects, not
        // one shared object (the old singular-adm_object behavior) and not silently just the first.
        let metas: Vec<AdmObjectMetadata> = (0..8)
            .map(|i| AdmObjectMetadata { name: format!("Obj{i}"), position: AdmPosition { azimuth: i as f64 * 10.0, ..Default::default() }, ..Default::default() })
            .collect();
        let mixer = mixer_with_tracks(vec![object_track(1, metas)]);
        let xml = to_sadm_xml(&mixer);
        for i in 0..8 {
            assert!(xml.contains(&format!("audioObjectName=\"Obj{i}\"")), "missing Obj{i} in:\n{xml}");
            assert!(xml.contains(&format!("coordinate=\"azimuth\">{:.6}</position>", i as f64 * 10.0)));
        }
        assert_eq!(xml.matches("<audioObject ").count(), 8, "expected exactly 8 <audioObject> elements, one per channel");

        // And it round-trips: 8 real, independent updates, each its own object's own azimuth intact.
        let updates = from_sadm_xml(&xml).unwrap();
        assert_eq!(updates.len(), 8);
        for i in 0..8 {
            let u = updates.iter().find(|u| u.name == format!("Obj{i}")).unwrap();
            assert_eq!(u.metadata.position.azimuth, i as f64 * 10.0);
        }
    }

    #[test]
    fn to_sadm_xml_emits_direct_speakers_bed_for_a_layout_tagged_track() {
        let mixer = mixer_with_tracks(vec![bed_track(1, crate::layout::ChannelLayout::Stereo)]);
        let xml = to_sadm_xml(&mixer);
        assert!(xml.contains("typeDefinition=\"DirectSpeakers\""));
        assert!(xml.contains("<speakerLabel>RC_L</speakerLabel>"));
        assert!(xml.contains("<speakerLabel>RC_R</speakerLabel>"));
    }

    #[test]
    fn to_sadm_xml_then_from_sadm_xml_round_trips_object_metadata() {
        let meta = AdmObjectMetadata {
            name: "Dialogue".into(),
            gain_db: -6.0,
            position: AdmPosition { azimuth: 45.0, elevation: 10.0, distance: 1.0 },
            width: 0.2,
            height: 0.1,
            depth: 0.0,
        };
        let mixer = mixer_with_tracks(vec![object_track(1, vec![meta.clone()])]);
        let xml = to_sadm_xml(&mixer);

        let updates = from_sadm_xml(&xml).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].name, "Dialogue");
        // Gain round-trips through a linear-dB-linear conversion, so allow float slop.
        assert!((updates[0].metadata.gain_db - meta.gain_db).abs() < 0.01);
        assert_eq!(updates[0].metadata.position, meta.position);
        assert_eq!(updates[0].metadata.width, meta.width);
        assert_eq!(updates[0].metadata.height, meta.height);
        assert_eq!(updates[0].metadata.depth, meta.depth);
    }

    #[test]
    fn from_sadm_xml_skips_direct_speakers_blocks_entirely() {
        // A bed-only document (no ADM object at all) must parse to zero updates, not accidentally
        // pick up a DirectSpeakers audioBlockFormat as if it were an object's position.
        let mixer = mixer_with_tracks(vec![bed_track(1, crate::layout::ChannelLayout::Stereo)]);
        let xml = to_sadm_xml(&mixer);
        let updates = from_sadm_xml(&xml).unwrap();
        assert!(updates.is_empty());
    }

    #[test]
    fn apply_sadm_updates_writes_back_onto_the_matching_live_track_by_name() {
        let original = AdmObjectMetadata {
            name: "Dialogue".into(),
            gain_db: 0.0,
            position: AdmPosition::default(),
            width: 0.0,
            height: 0.0,
            depth: 0.0,
        };
        let track = object_track(1, vec![original]);
        let mixer = mixer_with_tracks(vec![track.clone()]);

        let update = AdmObjectUpdate {
            name: "Dialogue".to_string(),
            metadata: AdmObjectMetadata {
                name: "Dialogue".into(),
                gain_db: -12.0,
                position: AdmPosition { azimuth: 90.0, elevation: 0.0, distance: 1.0 },
                width: 0.0,
                height: 0.0,
                depth: 0.0,
            },
        };
        let applied = apply_sadm_updates(&mixer, &[update.clone()]);
        assert_eq!(applied, 1);
        assert_eq!(track.adm_objects[0].lock().unwrap().gain_db, -12.0);
        assert_eq!(track.adm_objects[0].lock().unwrap().position.azimuth, 90.0);
    }

    #[test]
    fn apply_sadm_updates_ignores_an_update_whose_name_matches_no_live_track() {
        let mixer = mixer_with_tracks(vec![object_track(1, vec![AdmObjectMetadata { name: "Dialogue".into(), ..Default::default() }])]);
        let update = AdmObjectUpdate { name: "Nonexistent".to_string(), metadata: Default::default() };
        assert_eq!(apply_sadm_updates(&mixer, &[update]), 0);
    }

    #[test]
    fn position_default_matches_bs2076s_own_default_distance() {
        // Azimuth/elevation default to dead ahead (0, 0); distance defaults to the reference
        // distance (1.0), not 0.0 -- a real, verified BS.2076 default, not an arbitrary zero.
        assert_eq!(AdmPosition::default(), AdmPosition { azimuth: 0.0, elevation: 0.0, distance: 1.0 });
    }

    #[test]
    fn object_metadata_round_trips_through_json() {
        let meta = AdmObjectMetadata {
            name: "Dialogue".into(),
            gain_db: -3.0,
            position: AdmPosition { azimuth: 30.0, elevation: 10.0, distance: 1.0 },
            width: 0.2,
            height: 0.0,
            depth: 0.0,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: AdmObjectMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back, meta);
    }

    #[test]
    fn object_metadata_defaults_everything_but_name_when_deserialized_from_a_minimal_value() {
        let meta: AdmObjectMetadata = serde_json::from_value(serde_json::json!({ "name": "Ambience" })).unwrap();
        assert_eq!(meta.name, "Ambience");
        assert_eq!(meta.gain_db, 0.0);
        assert_eq!(meta.position, AdmPosition::default());
        assert_eq!((meta.width, meta.height, meta.depth), (0.0, 0.0, 0.0));
    }
}
