//! Processing-chain stages a `Track` or `Bus` (mixer.rs) can optionally carry — filter, EQ, two
//! dynamics stages, phase invert, delay — modeled after a real console's own channel chain (see
//! `~/DEV/yam bus.png`'s PRE_FILTER/PRE_DYN1/PRE_DYN2 pickoff naming, and `~/DEV/Vista grid.png`'s
//! "Gain / HP-LP / Phase -> EQ -> INS -> DYN -> DLY -> Fader" block, "INS" — an external patch
//! insert point — deliberately not modeled here, that's the pickoff-point patch bay's territory,
//! not a native stage).
//!
//! **Structural placeholders, not real DSP** (confirmed with the user before building this): each
//! stage here is a real, addressable position in the chain — its own on/off, its own parameters,
//! stored and exposed over the WS protocol (ws.rs) exactly like a real one would be — but
//! `engine.rs`'s per-period loop does not yet apply any of them to the signal. Turning a stage
//! into real DSP later means implementing its effect in `engine.rs`; this module and the data
//! model around it don't need to change.
//!
//! **A stage that isn't part of a track's/bus's chosen `ChannelTemplate` (config.rs) doesn't
//! exist** — `Track.filter`/`.eq`/etc. are `Option<_>`, `None` for a `Simple`-template resource,
//! not a present-but-bypassed stage. This is what makes an unconfigured stage genuinely free (no
//! struct, no lock, nothing for the engine to skip over) rather than just hidden — the user's own
//! framing for why this had to be `Option<T>` on one uniform `Track`/`Bus` type, not a type per
//! channel template.

use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

pub struct FilterStage {
    pub on: AtomicBool,
    pub hp_hz: Mutex<f32>,
    pub lp_hz: Mutex<f32>,
}

impl FilterStage {
    /// A newly-added filter stage starts on, with corner frequencies wide enough to be inaudible
    /// even once real DSP is implemented (20 Hz HP / 20 kHz LP) — "present in the chain, no
    /// audible effect yet" is the right default for a placeholder stage, not "present but off".
    pub fn default_on() -> Self {
        Self { on: AtomicBool::new(true), hp_hz: Mutex::new(20.0), lp_hz: Mutex::new(20_000.0) }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EqBand {
    pub freq_hz: f32,
    pub gain_db: f32,
    pub q: f32,
}

pub struct EqStage {
    pub on: AtomicBool,
    /// Empty by default (no bands = no shaping, even once real DSP exists) — bands are added by a
    /// controller PUTting a full replacement list, same "whole-array PUT" convention as
    /// `patch.rs`'s crosspoint entries.
    pub bands: Mutex<Vec<EqBand>>,
}

impl EqStage {
    pub fn default_on() -> Self {
        Self { on: AtomicBool::new(true), bands: Mutex::new(Vec::new()) }
    }
}

/// One dynamics stage (`Track.dyn1`/`.dyn2`, `~/DEV/yam bus.png`'s PRE_DYN1/PRE_DYN2) — shared
/// shape for either position; which processor it behaves as (gate, compressor, ...) is an
/// operator/controller convention this app doesn't distinguish, same as a real console's own
/// generic dynamics block.
pub struct DynamicsStage {
    pub on: AtomicBool,
    pub threshold_db: Mutex<f32>,
    pub ratio: Mutex<f32>,
    pub attack_ms: Mutex<f32>,
    pub release_ms: Mutex<f32>,
    pub makeup_db: Mutex<f32>,
}

impl DynamicsStage {
    /// threshold at 0 dBFS + a 1:1 ratio is a real, standard "no gain reduction" no-op point for
    /// a compressor -- consistent with every other stage's "present, inaudible" default.
    pub fn default_on() -> Self {
        Self {
            on: AtomicBool::new(true),
            threshold_db: Mutex::new(0.0),
            ratio: Mutex::new(1.0),
            attack_ms: Mutex::new(10.0),
            release_ms: Mutex::new(100.0),
            makeup_db: Mutex::new(0.0),
        }
    }
}

pub struct PhaseStage {
    pub invert: AtomicBool,
}

impl PhaseStage {
    pub fn default_on() -> Self {
        Self { invert: AtomicBool::new(false) }
    }
}

pub struct DelayStage {
    pub on: AtomicBool,
    pub delay_ms: Mutex<f32>,
}

impl DelayStage {
    pub fn default_on() -> Self {
        Self { on: AtomicBool::new(true), delay_ms: Mutex::new(0.0) }
    }
}
