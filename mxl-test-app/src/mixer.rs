use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::config::{BusConfig, TrackConfig};

/// One input strip: gain (trim, applied first) -> fader (applied second) -> bus-assign (which
/// buses this track's post-fader signal sums into). Pan is deliberately not modeled yet (see the
/// Phase 2 plan's Verification section note on this app) — every track and bus is the same fixed
/// channel count, so summing is a plain elementwise add, no downmix/pan law needed.
pub struct Track {
    pub id: u32,
    pub label: String,
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
    /// Post-fader peak, one value per channel, in dBFS (`f32::NEG_INFINITY` for silence) — written
    /// by the engine once per period, read by the WebSocket broadcaster.
    pub meter_db: Mutex<Vec<f32>>,
}

impl Track {
    pub fn new(cfg: &TrackConfig, channels: usize) -> Self {
        Self {
            id: cfg.id,
            label: cfg.label.clone(),
            gain_db: Mutex::new(cfg.gain_db),
            fader_db: Mutex::new(cfg.fader_db),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            bus_assign: Mutex::new(cfg.bus_assign.iter().copied().collect()),
            reader: Mutex::new(None),
            meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
        }
    }
}

/// One output strip: sums every track assigned to it (respecting mute/solo), applies its own
/// fader, writes the result to its own real MXL flow.
pub struct Bus {
    pub id: u32,
    pub label: String,
    pub fader_db: Mutex<f32>,
    pub mute: AtomicBool,
    pub writer: Mutex<crate::flow::FlowWriter>,
    pub meter_db: Mutex<Vec<f32>>,
}

impl Bus {
    pub fn new(cfg: &BusConfig, writer: crate::flow::FlowWriter, channels: usize) -> Self {
        Self {
            id: cfg.id,
            label: cfg.label.clone(),
            fader_db: Mutex::new(cfg.fader_db),
            mute: AtomicBool::new(false),
            writer: Mutex::new(writer),
            meter_db: Mutex::new(vec![f32::NEG_INFINITY; channels]),
        }
    }
}

/// dB -> linear amplitude multiplier.
pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
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
