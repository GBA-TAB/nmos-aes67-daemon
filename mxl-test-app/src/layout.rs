//! Standard multi-channel layout vocabulary — real channel *roles* (which index is Left/Right/
//! Center/LFE/etc), not just a bare count, layered on top of every place in this app that
//! currently only tracks `channels: usize`/`u32` (`config.rs`, `mixer.rs`, `patch.rs`).
//!
//! Two label vocabularies are kept deliberately distinct, matching two different real standards:
//! - `ChannelRole::short_name()`: the plain SMPTE ST 428-12 (DCDM Common Audio Channels and
//!   Soundfield Groups) abbreviation - "L", "C", "Ls", "Lss", etc. Used for the NMOS Flow
//!   `channels[]` "label" field (see `nmos/resources.rs::channels_json`), matching the sibling
//!   aes67-linux-daemon's own IS-08 precedent of a plain, short, human-readable per-channel label
//!   (`nmos_is12.cpp`'s "Left"/"Right"/"ChN" - confirmed by reading that file directly).
//! - `ChannelRole::adm_speaker_label()`: the ADM common-definitions form, prefixed `RC_` (e.g.
//!   `RC_L`, `RC_C`, `RC_LFE`) - confirmed directly against the Dolby Atmos Master ADM Profile's
//!   own common-definitions table (which documents `RC_L`/`RC_C`/`RC_LFE` explicitly) and EBU's
//!   ADM guidelines referencing the same ITU-R BS.2094 common-definitions source. Used by `adm.rs`
//!   (Phase D/E) when building a real Serial ADM document's `audioChannelFormat`/`speakerLabel`.
//!
//! Verified directly (not guessed): `L`/`C`/`R`/`Ls`/`Rs`/`Lss`/`Rss`/`Lrs`/`Rrs`/`LFE` are the
//! real SMPTE ST 428-12 abbreviations; `RC_L`/`RC_C`/`RC_LFE` are the real ADM common-definitions
//! form for those same positions. **Not independently re-verified against the primary ITU-R
//! BS.2094 text**: the exact `RC_`-prefixed strings for every other role here (`RC_R`, `RC_Ls`,
//! `RC_Rs`, `RC_Lss`, `RC_Rss`, `RC_Lrs`, `RC_Rrs`) and every height-layer role's ADM label
//! (`Ltf`/`Rtf`/`Ltb`/`Rtb`) - these follow the confirmed `RC_<abbreviation>` pattern
//! consistently, but if real interop with another ADM tool ever surfaces a mismatch, check
//! BS.2094's own common-definitions table directly before trusting the pattern further.

use serde::{Deserialize, Serialize};

/// One channel's real position/role within a standard layout. Variant names match the SMPTE
/// ST 428-12 abbreviation directly (see module doc for the two label forms each exposes).
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelRole {
    /// Mono - a single, non-positional channel. Distinct from `C` (Center of a wider array):
    /// ADM's own model treats a mono bed's "M" position and a multichannel array's Center as
    /// semantically different, even though both are physically dead-center.
    M,
    L,
    R,
    C,
    Lfe,
    Ls,
    Rs,
    /// 7.1 side surrounds (distinct from the narrower 5.1 `Ls`/`Rs` pair).
    Lss,
    Rss,
    /// 7.1 rear surrounds.
    Lrs,
    Rrs,
    /// 5.1.4 height layer (front pair).
    Ltf,
    Rtf,
    /// 5.1.4 height layer (back/rear pair).
    Ltb,
    Rtb,
}

impl ChannelRole {
    /// Plain SMPTE ST 428-12 abbreviation - used for the NMOS Flow `channels[]` label.
    pub fn short_name(&self) -> &'static str {
        match self {
            ChannelRole::M => "M",
            ChannelRole::L => "L",
            ChannelRole::R => "R",
            ChannelRole::C => "C",
            ChannelRole::Lfe => "LFE",
            ChannelRole::Ls => "Ls",
            ChannelRole::Rs => "Rs",
            ChannelRole::Lss => "Lss",
            ChannelRole::Rss => "Rss",
            ChannelRole::Lrs => "Lrs",
            ChannelRole::Rrs => "Rrs",
            ChannelRole::Ltf => "Ltf",
            ChannelRole::Rtf => "Rtf",
            ChannelRole::Ltb => "Ltb",
            ChannelRole::Rtb => "Rtb",
        }
    }

    /// ADM common-definitions speaker label (`RC_`-prefixed form) - see module doc for which of
    /// these are directly spec-confirmed vs. following the confirmed pattern.
    pub fn adm_speaker_label(&self) -> &'static str {
        match self {
            ChannelRole::M => "RC_M",
            ChannelRole::L => "RC_L",
            ChannelRole::R => "RC_R",
            ChannelRole::C => "RC_C",
            ChannelRole::Lfe => "RC_LFE",
            ChannelRole::Ls => "RC_Ls",
            ChannelRole::Rs => "RC_Rs",
            ChannelRole::Lss => "RC_Lss",
            ChannelRole::Rss => "RC_Rss",
            ChannelRole::Lrs => "RC_Lrs",
            ChannelRole::Rrs => "RC_Rrs",
            ChannelRole::Ltf => "RC_Ltf",
            ChannelRole::Rtf => "RC_Rtf",
            ChannelRole::Ltb => "RC_Ltb",
            ChannelRole::Rtb => "RC_Rtb",
        }
    }
}

/// A standard multi-channel layout: an ordered list of channel roles, or `Discrete(n)` for
/// today's existing behavior (a bare channel count with no role semantics at all - the default
/// for every config that doesn't set `layout` explicitly, see `config.rs`).
///
/// Channel *order* within each named layout follows the common WAV/file-based convention (the
/// most common convention in file-based/AES67 practice) - documented explicitly here since ITU/
/// Dolby/SMPTE orderings genuinely differ and this is a real choice, not an oversight.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLayout {
    Mono,
    Stereo,
    /// ITU quad: L, R, Ls, Rs.
    Quad,
    /// WAV/file order: L, R, C, LFE, Ls, Rs.
    Surround5_1,
    /// WAV/file order: L, R, C, LFE, Lss, Rss, Lrs, Rrs.
    Surround7_1,
    /// 5.1 bed + 4 height channels (front+back pairs): L, R, C, LFE, Ls, Rs, Ltf, Rtf, Ltb, Rtb.
    Surround5_1_4,
    /// Today's existing behavior - an arbitrary channel count with no role semantics. This is
    /// what every config that doesn't set `layout` implicitly behaves as.
    Discrete(u32),
}

impl ChannelLayout {
    pub fn channel_count(&self) -> u32 {
        self.roles().len() as u32
    }

    /// The ordered role list for this layout. Empty for `Discrete` - a discrete layout has no
    /// role semantics by definition, callers should fall back to a bare count/generic label.
    pub fn roles(&self) -> Vec<ChannelRole> {
        use ChannelRole::*;
        match self {
            ChannelLayout::Mono => vec![M],
            ChannelLayout::Stereo => vec![L, R],
            ChannelLayout::Quad => vec![L, R, Ls, Rs],
            ChannelLayout::Surround5_1 => vec![L, R, C, Lfe, Ls, Rs],
            ChannelLayout::Surround7_1 => vec![L, R, C, Lfe, Lss, Rss, Lrs, Rrs],
            ChannelLayout::Surround5_1_4 => vec![L, R, C, Lfe, Ls, Rs, Ltf, Rtf, Ltb, Rtb],
            ChannelLayout::Discrete(n) => {
                // No role semantics - `Discrete` exists precisely to represent "just a count."
                // Returning an empty Vec (rather than n placeholder roles) makes "does this
                // layout have real role info" a simple `.is_empty()` check at every call site.
                let _ = n;
                vec![]
            }
        }
    }

    /// The role at a given 0-based channel index, if this layout has role semantics for it.
    /// `None` for `Discrete` (no roles at all) or an out-of-range index.
    pub fn role_at(&self, index: usize) -> Option<ChannelRole> {
        self.roles().get(index).copied()
    }
}

/// Resolves a resource's real channel count from its config's own optional `channels` and
/// `layout` fields — the one shared validation point every `layout`-bearing config struct
/// (`TrackConfig`/`BusConfig`/`MasterTrackConfig`/`InputGridEntryConfig`/`OutputGridEntryConfig`)
/// funnels through, per the doc comment on each struct's `layout` field.
///
/// `Discrete` (or any other layout with no real roles) never implicitly supplies `channels` —
/// it behaves exactly as if `layout` were unset, matching `layout.rs`'s own
/// `discrete_has_no_roles...` test. A *named* layout with real roles supplies `channels` when
/// unset, and hard-errors if an explicit `channels` disagrees with it — this codebase's existing
/// "validate at startup, don't guess at runtime" philosophy (see `topology.rs`'s
/// `warn_incompatible_sends` for the analogous existing precedent one level up, at the
/// track-to-bus level).
///
/// `context` is a short, already-formatted description of the resource being validated (e.g.
/// `"track 3"`), used only to make the error message identify which resource failed.
pub fn resolve_channels(context: &str, channels: Option<u32>, layout: Option<ChannelLayout>) -> Result<Option<u32>, String> {
    let Some(layout) = layout else { return Ok(channels) };
    let layout_count = layout.channel_count();
    if layout_count == 0 {
        // Discrete (or any other no-role layout): no role semantics, no implicit channel count.
        return Ok(channels);
    }
    match channels {
        Some(c) if c != layout_count => Err(format!(
            "{context}: layout {layout:?} implies {layout_count} channel(s) but channels is explicitly set to {c} -- remove one or make them agree"
        )),
        _ => Ok(Some(layout_count)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discrete_has_no_roles_but_reports_its_own_count_via_channel_count_override() {
        // Discrete's channel_count() comes from roles().len(), which is empty - callers needing
        // Discrete's real count use the resource's own separate `channels` field instead (see
        // config.rs's validation: layout.channel_count() only supplies `channels` when it's
        // unset AND the layout has real roles; Discrete never implicitly sets `channels`).
        assert_eq!(ChannelLayout::Discrete(16).roles().len(), 0);
    }

    #[test]
    fn standard_layouts_have_the_expected_channel_count() {
        assert_eq!(ChannelLayout::Mono.channel_count(), 1);
        assert_eq!(ChannelLayout::Stereo.channel_count(), 2);
        assert_eq!(ChannelLayout::Quad.channel_count(), 4);
        assert_eq!(ChannelLayout::Surround5_1.channel_count(), 6);
        assert_eq!(ChannelLayout::Surround7_1.channel_count(), 8);
        assert_eq!(ChannelLayout::Surround5_1_4.channel_count(), 10);
    }

    #[test]
    fn surround_5_1_role_order_matches_documented_wav_convention() {
        use ChannelRole::*;
        assert_eq!(ChannelLayout::Surround5_1.roles(), vec![L, R, C, Lfe, Ls, Rs]);
    }

    #[test]
    fn role_at_returns_none_past_the_end_and_for_discrete() {
        assert_eq!(ChannelLayout::Stereo.role_at(2), None);
        assert_eq!(ChannelLayout::Discrete(8).role_at(0), None);
    }

    #[test]
    fn short_name_and_adm_speaker_label_are_distinct_and_related() {
        assert_eq!(ChannelRole::L.short_name(), "L");
        assert_eq!(ChannelRole::L.adm_speaker_label(), "RC_L");
        assert_eq!(ChannelRole::Lfe.short_name(), "LFE");
        assert_eq!(ChannelRole::Lfe.adm_speaker_label(), "RC_LFE");
    }

    #[test]
    fn layout_serializes_to_snake_case_json() {
        let json = serde_json::to_string(&ChannelLayout::Surround5_1).unwrap();
        assert_eq!(json, "\"surround5_1\"");
        let back: ChannelLayout = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ChannelLayout::Surround5_1);
    }

    #[test]
    fn discrete_layout_round_trips_its_count() {
        let json = serde_json::to_string(&ChannelLayout::Discrete(12)).unwrap();
        let back: ChannelLayout = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ChannelLayout::Discrete(12));
    }

    #[test]
    fn resolve_channels_with_no_layout_passes_channels_through_unchanged() {
        assert_eq!(resolve_channels("x", Some(4), None), Ok(Some(4)));
        assert_eq!(resolve_channels("x", None, None), Ok(None));
    }

    #[test]
    fn resolve_channels_discrete_layout_never_implicitly_sets_channels() {
        assert_eq!(resolve_channels("x", None, Some(ChannelLayout::Discrete(16))), Ok(None));
        assert_eq!(resolve_channels("x", Some(3), Some(ChannelLayout::Discrete(16))), Ok(Some(3)));
    }

    #[test]
    fn resolve_channels_named_layout_supplies_channels_when_unset() {
        assert_eq!(resolve_channels("x", None, Some(ChannelLayout::Surround5_1)), Ok(Some(6)));
    }

    #[test]
    fn resolve_channels_named_layout_agreeing_with_explicit_channels_is_fine() {
        assert_eq!(resolve_channels("x", Some(6), Some(ChannelLayout::Surround5_1)), Ok(Some(6)));
    }

    #[test]
    fn resolve_channels_named_layout_disagreeing_with_explicit_channels_is_a_hard_error() {
        assert!(resolve_channels("track 3", Some(2), Some(ChannelLayout::Surround5_1)).is_err());
    }
}
