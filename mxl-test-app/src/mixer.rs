use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::config::{BusConfig, TrackConfig};

/// One input strip: gain (trim, applied first) -> fader (applied second) -> bus-assign (which
/// buses this track's post-fader signal sums into). Pan is deliberately not modeled yet (see the
/// Phase 2 plan's Verification section note on this app) — a track assigned to a bus of the same
/// channel count sums in directly; a mono track assigned to a wider bus goes equally to every
/// channel instead (see `mix_into`), which is the furthest a "pan" concept goes without a real
/// pan law.
pub struct Track {
    pub id: u32,
    pub label: String,
    /// This track's own channel count (1 = mono, 2 = stereo, ...) — independent of every other
    /// track's and bus's own count, resolved once at startup from `TrackConfig::channels` (see
    /// `main.rs`).
    pub channels: usize,
    pub gain_db: Mutex<f32>,
    pub fader_db: Mutex<f32>,
    pub mute: AtomicBool,
    pub solo: AtomicBool,
    pub bus_assign: Mutex<HashSet<u32>>,
    /// The real MXL reader, if this track has a source configured — `None` reads as silence.
    /// `std::sync::Mutex`, not `tokio::sync::Mutex`: the audio engine (a plain OS thread) needs a
    /// blocking lock every period, and the WebSocket handlers (async) only ever hold it briefly to
    /// swap the reader, never across an `.await` — same reasoning as mxl-bridge's own
    /// `nmos/is08.rs::routing` field.
    pub reader: Mutex<Option<crate::flow::FlowReader>>,
    /// The `sender_id` this track's Receiver was last activated against, for IS-05's
    /// `subscription.sender_id` — purely informational, set alongside `reader` by
    /// `open_source`/`close_source`'s callers (nmos/server.rs); the amixer WebSocket `source` PUT
    /// (ws.rs) sets a raw flow_id directly and doesn't have an NMOS sender_id to report here.
    pub sender_id: Mutex<Option<String>>,
    /// Post-fader peak, one value per channel, in dBFS (`f32::NEG_INFINITY` for silence) — written
    /// by the engine once per period, read by the WebSocket broadcaster.
    pub meter_db: Mutex<Vec<f32>>,
}

impl Track {
    pub fn new(cfg: &TrackConfig, channels: usize) -> Self {
        Self {
            id: cfg.id,
            label: cfg.label.clone(),
            channels,
            gain_db: Mutex::new(cfg.gain_db),
            fader_db: Mutex::new(cfg.fader_db),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            bus_assign: Mutex::new(cfg.bus_assign.iter().copied().collect()),
            reader: Mutex::new(None),
            sender_id: Mutex::new(None),
            meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
        }
    }

    /// Opens `flow_id` as this track's reader (at this track's own `channels` count — the flow
    /// being opened must actually have that many channels, or this fails), replacing whatever it
    /// had before (or nothing). Shared by both the amixer WebSocket `source` PUT (ws.rs) and IS-05
    /// receiver activation (nmos/server.rs) — same underlying action, two different protocols
    /// asking for it. Does not touch `sender_id` — callers that have one (nmos/server.rs) set it
    /// themselves alongside this.
    pub fn open_source(&self, mxl_domain: &str, mxl_so_path: &std::path::Path, flow_id: &str) -> anyhow::Result<()> {
        let reader = crate::flow::FlowReader::open(mxl_domain, mxl_so_path, flow_id, self.channels)?;
        *self.reader.lock().unwrap() = Some(reader);
        Ok(())
    }

    /// Drops this track's reader, if any — IS-05 deactivation (nmos/server.rs). The WS protocol
    /// has no equivalent "clear source" PUT yet (not asked for; `source` only ever sets one).
    pub fn close_source(&self) {
        *self.reader.lock().unwrap() = None;
        *self.sender_id.lock().unwrap() = None;
    }
}

/// One output strip: sums every track assigned to it (respecting mute/solo), applies its own
/// fader, writes the result to its own real MXL flow.
pub struct Bus {
    pub id: u32,
    pub label: String,
    /// This bus's own real MXL flow_id (resolved once at startup — see `BusConfig::resolve_flow_id`)
    /// — also the NMOS Flow.id its mirrored Flow/Sender advertise (nmos/resources.rs), kept here so
    /// the NMOS layer doesn't need to re-derive or separately track it.
    pub flow_id: uuid::Uuid,
    /// This bus's own channel count — see `Track::channels`'s docs, same idea.
    pub channels: usize,
    pub fader_db: Mutex<f32>,
    pub mute: AtomicBool,
    pub writer: Mutex<crate::flow::FlowWriter>,
    pub meter_db: Mutex<Vec<f32>>,
    /// The `receiver_id` a controller last PATCHed this bus's mirrored Sender's `subscription`
    /// to — purely informational (see nmos/resources.rs's `sender_json` docs: nothing here is
    /// actually gated by it, unlike mxl-bridge's Sinks).
    pub receiver_id: Mutex<Option<String>>,
}

impl Bus {
    pub fn new(cfg: &BusConfig, flow_id: uuid::Uuid, writer: crate::flow::FlowWriter, channels: usize) -> Self {
        Self {
            id: cfg.id,
            label: cfg.label.clone(),
            flow_id,
            channels,
            fader_db: Mutex::new(cfg.fader_db),
            mute: AtomicBool::new(false),
            writer: Mutex::new(writer),
            meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
            receiver_id: Mutex::new(None),
        }
    }
}

/// dB -> linear amplitude multiplier.
pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Adds `src` (one track's post-fader planar samples, `src.len()` channels) into `dst` (a bus's
/// running sum buffer, `dst.len()` channels), `frames` samples each. No real panner yet (see the
/// Phase 2 plan's Verification section note on this app), so a channel-count mismatch between a
/// track and a bus it's assigned to is handled the simplest way that's still unambiguous without
/// one:
/// - equal counts: direct elementwise sum, one-to-one.
/// - mono track (`src.len() == 1`) into a wider bus: the single channel goes into *every* bus
///   channel at unity (dual-mono — the closest thing to "centered" without an actual pan law).
/// - wider track into a mono bus (`dst.len() == 1`): downmixed by averaging all of the track's
///   channels, avoiding the level buildup a plain sum would cause.
/// - any other mismatch (e.g. 3 channels into 2): left alone, a no-op — genuinely ambiguous
///   without a real panner/matrix, and validated against at startup instead of guessed at here
///   (see main.rs's channel-compatibility check, which warns about exactly this case once, rather
///   than the engine silently doing nothing every single period).
pub fn mix_into(src: &[Vec<f32>], dst: &mut [Vec<f32>], frames: usize) {
    let (sc, dc) = (src.len(), dst.len());
    if sc == dc {
        for ch in 0..sc {
            for i in 0..frames {
                dst[ch][i] += src[ch][i];
            }
        }
    } else if sc == 1 && dc > 1 {
        for dst_ch in dst.iter_mut() {
            for i in 0..frames {
                dst_ch[i] += src[0][i];
            }
        }
    } else if dc == 1 && sc > 1 {
        let scale = 1.0 / sc as f32;
        for i in 0..frames {
            let sum: f32 = src.iter().map(|ch| ch[i]).sum();
            dst[0][i] += sum * scale;
        }
    }
}

/// True if `mix_into` can actually do something meaningful for these two channel counts (used at
/// startup to validate every track-to-bus assignment once, rather than silently no-op'ing forever
/// at audio rate for a mismatched pair — see `mix_into`'s own docs for which combinations work).
pub fn channels_compatible(track_channels: usize, bus_channels: usize) -> bool {
    track_channels == bus_channels || track_channels == 1 || bus_channels == 1
}

/// Peak sample magnitude in a block -> dBFS (`NEG_INFINITY` for exact silence, matching the
/// AudioMixerDashboard's own "-Infinity" convention for a silent channel).
pub fn peak_to_db(peak: f32) -> f32 {
    if peak <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * peak.log10()
    }
}

pub fn is_muted(mute: &AtomicBool) -> bool {
    mute.load(Ordering::Relaxed)
}

pub fn is_soloed(solo: &AtomicBool) -> bool {
    solo.load(Ordering::Relaxed)
}
