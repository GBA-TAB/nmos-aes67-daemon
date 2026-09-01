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
//! **A stage that isn't part of a track's/master's chain (config.rs's `chain`/`ChannelTemplate`)
//! doesn't exist** — `Track.chain`/`MasterTrack.chain` is a `Vec<ProcessingStage>`, and an absent
//! stage is simply not an element of that `Vec`, not a present-but-bypassed one. This is what
//! makes an unconfigured stage genuinely free (no struct, no lock, nothing for the engine to skip
//! over) rather than just hidden — the user's own framing for why this had to be representable as
//! "doesn't exist," not "exists but off," when this module was first built (back when presence was
//! `Option<T>` per fixed field; the ordered/typed `chain` redesign — see `Track.chain`'s own doc
//! comment in mixer.rs — replaced the fixed fields with this `Vec`, keeping that same principle).

use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use serde::Deserialize;

/// Discriminant for which concrete stage type occupies a chain slot — the "typed" half of a
/// track's/master's ordered `Vec<ProcessingStage>` chain (`Track.chain`/`MasterTrack.chain`,
/// mixer.rs). Also `config.rs`'s `StageSlotConfig.kind` wire/config value and the `kind` field in
/// `ws.rs`'s per-slot `chain` broadcast — one enum for all three roles, so there's exactly one
/// place that spells "eq"/"dynamics"/etc.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    Filter,
    Eq,
    Dynamics,
    Phase,
    Delay,
}

impl StageKind {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::Eq => "eq",
            Self::Dynamics => "dynamics",
            Self::Phase => "phase",
            Self::Delay => "delay",
        }
    }
}

/// One occupied slot in a track's/master's ordered processing chain — replaces the old six
/// independent named `Option<Stage>` fields + binary `ChannelTemplate` gate that predated the
/// runtime chain-design feature. Each variant wraps the exact same struct that existed before this
/// enum (no field changes to `FilterStage`/`EqStage`/`DynamicsStage`/`PhaseStage`/`DelayStage`), so
/// `ws.rs`'s existing per-type `apply_*`/`*_json` logic is reused unchanged via `apply`/`to_json`
/// below — a slot's presence is now "this index exists in the chain `Vec`," not "this field is
/// `Some`." Multiple slots of the same kind (e.g. two `Dynamics` stages) are legitimately allowed —
/// unlike the old fixed `dyn1`/`dyn2` fields, nothing here caps how many of one kind a chain has.
pub enum ProcessingStage {
    Filter(FilterStage),
    Eq(EqStage),
    Dynamics(DynamicsStage),
    Phase(PhaseStage),
    Delay(DelayStage),
}

impl ProcessingStage {
    pub fn kind(&self) -> StageKind {
        match self {
            Self::Filter(_) => StageKind::Filter,
            Self::Eq(_) => StageKind::Eq,
            Self::Dynamics(_) => StageKind::Dynamics,
            Self::Phase(_) => StageKind::Phase,
            Self::Delay(_) => StageKind::Delay,
        }
    }

    /// A newly-added slot of `kind` at that type's own "present, inaudible" default — same values
    /// each type's own `default_on()` always used.
    pub fn default_on(kind: StageKind) -> Self {
        match kind {
            StageKind::Filter => Self::Filter(FilterStage::default_on()),
            StageKind::Eq => Self::Eq(EqStage::default_on()),
            StageKind::Dynamics => Self::Dynamics(DynamicsStage::default_on()),
            StageKind::Phase => Self::Phase(PhaseStage::default_on()),
            StageKind::Delay => Self::Delay(DelayStage::default_on()),
        }
    }

    /// Dispatches a PUT/CREATE-time params value to this slot's own type-specific field parser —
    /// same per-kind JSON shape `ws.rs` always accepted. Infallible now (no more "stage not present
    /// for this resource's ChannelTemplate" case — presence is index-existence, checked by the
    /// caller via `chain.get(idx)` before this is ever called).
    pub fn apply(&self, value: &serde_json::Value) {
        match self {
            Self::Filter(s) => crate::ws::apply_filter(s, value),
            Self::Eq(s) => crate::ws::apply_eq(s, value),
            Self::Dynamics(s) => crate::ws::apply_dynamics(s, value),
            Self::Phase(s) => crate::ws::apply_phase(s, value),
            Self::Delay(s) => crate::ws::apply_delay(s, value),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Filter(s) => crate::ws::filter_json(s),
            Self::Eq(s) => crate::ws::eq_json(s),
            Self::Dynamics(s) => crate::ws::dynamics_json(s),
            Self::Phase(s) => crate::ws::phase_json(s),
            Self::Delay(s) => crate::ws::delay_json(s),
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_on_matches_each_concrete_types_own_default_for_every_kind() {
        for kind in [StageKind::Filter, StageKind::Eq, StageKind::Dynamics, StageKind::Phase, StageKind::Delay] {
            let stage = ProcessingStage::default_on(kind);
            assert_eq!(stage.kind(), kind);
            // Every stage's own default_on() starts "on" (or, for Phase, non-inverted) -- confirm
            // the enum wrapper didn't lose that per the concrete type's own to_json shape.
            let json = stage.to_json();
            match kind {
                StageKind::Phase => assert_eq!(json["invert"], false),
                _ => assert_eq!(json["on"], true),
            }
        }
    }

    #[test]
    fn apply_and_to_json_round_trip_per_kind() {
        let filter = ProcessingStage::default_on(StageKind::Filter);
        filter.apply(&serde_json::json!({"hp_hz": 250.0}));
        assert_eq!(filter.to_json()["hp_hz"], 250.0);

        let eq = ProcessingStage::default_on(StageKind::Eq);
        eq.apply(&serde_json::json!({"bands": [{"freq_hz": 1000.0, "gain_db": 3.0, "q": 1.0}]}));
        assert_eq!(eq.to_json()["bands"][0]["gain_db"], 3.0);

        let dynamics = ProcessingStage::default_on(StageKind::Dynamics);
        dynamics.apply(&serde_json::json!({"threshold_db": -20.0}));
        assert_eq!(dynamics.to_json()["threshold_db"], -20.0);

        let phase = ProcessingStage::default_on(StageKind::Phase);
        phase.apply(&serde_json::json!({"invert": true}));
        assert_eq!(phase.to_json()["invert"], true);

        let delay = ProcessingStage::default_on(StageKind::Delay);
        delay.apply(&serde_json::json!({"delay_ms": 12.5}));
        assert_eq!(delay.to_json()["delay_ms"], 12.5);
    }

    #[test]
    fn stage_kind_wire_names_match_serde_rename() {
        // Pins the wire() strings against StageKind's own #[serde(rename_all = "snake_case")] --
        // ws.rs's chain_json uses wire() directly, config.rs's StageSlotConfig deserializes via
        // serde; both must agree on the same five strings.
        for (kind, name) in [
            (StageKind::Filter, "filter"),
            (StageKind::Eq, "eq"),
            (StageKind::Dynamics, "dynamics"),
            (StageKind::Phase, "phase"),
            (StageKind::Delay, "delay"),
        ] {
            assert_eq!(kind.wire(), name);
            let parsed: StageKind = serde_json::from_value(serde_json::json!(name)).unwrap();
            assert_eq!(parsed, kind);
        }
    }
}
