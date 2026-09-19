# Pan object matrix: every (track type, bus type) pair

## Status (2026-09-16, overnight autonomous pass)

**Backend: implemented, tested, live.** Not "design only" anymore -- see below for exactly what
landed and what's still pending.

- `PanObject::classify` (`mixer.rs`) is real code, not just this doc's table -- a pure function
  covering all 36 named-layout cells plus the `Discrete`/unset fallback, locked in by
  `classify_matches_the_design_doc_matrix_exactly` (one assertion per cell, so this doc and the
  code can't silently drift apart again the way the 5.1.4 RIG(9)-not-10 mislabel already did once).
- **Quad is now a real VBAP destination** (`vbap_bed_gains`/`vbap_supports_layout`) -- the ² marker
  in the matrix below is resolved.
- **`Rigid(n)` is real and wired into the engine's own mix dispatch** (`mix_into_scaled_with_rigid_
  array_pan`, `engine.rs`'s send-mixing loop) -- a bed source rotates/elevates as one body into a
  compatible destination, exactly as designed. Each `Send` gained its own live `rotation_deg`/
  `elevation_deg` (WS: part of the existing `sends` array PUT/broadcast, one new field each --
  no new param path). `warn_incompatible_sends` updated so a now-Rigid-covered pair (e.g.
  Quad->5.1) no longer gets a spurious "incompatible" warning.
- **Downmix coefficients are now genuinely user-editable** (the addendum below, also implemented,
  not just proposed): `DownmixTable` on `MixerState`, seeded with exactly today's compiled defaults,
  overridable via `PUT amixer/{mixerId}/downmix/{src}/{dst}` (value = the raw matrix), discoverable
  via the same periodic broadcast tick, and persisted through `state_path` (a `"downmix"` section in
  the capture/apply snapshot). The *proposed-by-analogy* matrices in this doc (Quad<->Stereo, etc.)
  are still **not** implemented as real compiled defaults -- an operator can PUT their own for those
  pairs right now, but nothing seeds one automatically yet.
- 129/129 tests passing (`cargo test --release`), all new coverage: `classify` (2 tests), Quad VBAP
  ring (1), rigid-array panning incl. the identity/rotation properties (3), `DownmixTable` (3),
  downmix persistence round-trip (1).
- Live-verified against the running instance (`mxl-test-app.conf`, port 3214) after a restart:
  config loads clean, WS protocol responds, `downmix` PUT/broadcast round-trips correctly. Also
  added `state_path` to this config (previously unset) so this and all the *existing* persistence
  this app already had actually survives a restart from now on, not just the new downmix table.

## Extension: bus→master and master→master, same session

Caught during live matrix testing: the classification system above only ever covered track→bus.
`resolve_master_in` (`patch.rs`) is a plain per-channel crosspoint with zero layout awareness --
bus-in/master-in patches were, and remain, exact manual wiring, never automatically panned or
downmixed. Confirmed with the user this should be extended, not left as the only path in. Built and
live-verified the same session:

- New `mixer::MasterSend` (no `pickoff` field -- neither a bus, which has no fader at all, nor a
  master, whose scratch never retains a pre-fader signal, has a second real pickoff point to offer).
  `Bus` and `MasterTrack` each gained `master_sends: Mutex<Vec<MasterSend>>`, additive alongside the
  existing patch mechanism (which still exists for exact hand-wired routing).
- `engine.rs`'s master loop now mixes every bus's and every *other* master's `master_sends`
  targeting it, through the identical `PanObject::classify` dispatch the track-send loop already
  uses (same three function calls: rigid-array pan, downmix-table lookup, or plain layout mix) --
  zero new mixing code, full reuse. A bus sends its *this-period* output (bus summing already
  finished); a master sends its *previous*-period output, same ordering rule every other
  master-to-master read in this pipeline already follows, so no new cycle detection was needed.
- Full WS protocol parity with `sends`: `PUT`/broadcast `amixer/{mixerId}/sum/{id}/master-sends` and
  `.../master/{id}/master-sends`, persisted through `state_path` (`capture`/`apply_snapshot`, both
  the live-value and dynamically-created-topology sections).
- `config.rs` gained `MasterSendConfig` + a `master_sends: Vec<MasterSendConfig>` field on both
  `BusConfig` and `MasterTrackConfig` for config-time authoring, same `#[serde(default)]` convention
  as every other optional field here.
- 130/130 tests (one new: `capture_apply_round_trips_bus_and_master_master_sends`) -- the mixing
  math itself needed no new tests, since it's the exact same already-tested functions.
- **Live-verified on the running instance**, extending the earlier one-of-each-type check: created
  one master per named layout (Mono/Stereo/Quad/5.1/7.1/5.1.4) and wired all 6 existing test buses
  into all 6, 36 more real combinations -- every master shows real, non-silent signal on every
  channel. Also isolate-tested master→master specifically (a fresh 5.1 master fed *only* by a Mono
  master's own `master_sends`, nothing else patched in) -- real signal on all 6 channels, confirming
  the cascade path works standalone, not just riding along with other contributions.

**Not done, deliberately left for a follow-up (this was already a lot for one pass):**
- Dashboard UI: no downmix-matrix editor page, no bus/master-sends editor either (`BusRow.razor`/
  `MasterStrip.razor` don't expose `master_sends` at all yet), and `SendsEditor`/the strip UI don't
  yet expose rotation/elevation controls or route to the ring panner for a `Rigid(n)` send -- a
  track's send still only has the fader/mute/level controls it always had; the new backend fields are
  PUT-able over the wire but nothing in the dashboard shows or edits them yet.
- The proposed-by-analogy downmix matrices (Quad<->Stereo, 5.1<->Quad, 7.1<->Quad, 5.1.4->Quad,
  5.1.4->7.1) are not seeded as compiled defaults -- see above.
- Config-time `"downmix_matrices"` override (only the runtime WS PUT path exists).
- **Known side effect of the restart needed to ship this**: the live demo's NMOS IS-05 connection
  from mxl-signal-gen into the `in-gen` input-grid entry did not survive the restart (a separate
  connection-management layer from the WS protocol this session's own testing used -- registered
  once, presumably via the Routing Matrix orchestrator or a manual connection, not something this
  process re-establishes on its own; `nmos/discovery.rs`'s own periodic retry logged one
  `FlowNotFound` at startup and hasn't self-healed since). Everything *downstream* of that input
  (tracks 50/51/52, the bus, the master, the output-grid patch) was recreated identically to before
  the restart and is ready the moment that one connection is remade -- likely a quick action in
  whatever tool made it originally, not something broken in mxl-test-app itself.

---

**Original framing (below), now superseded by the Status above where it says "design only":**

Answers the user's request directly: track/source type on the X axis, bus/destination type on the
Y axis, one cell per pair naming the real interaction ("pan object") and, where a static downmix
applies, its actual coefficients. Grounded in what's already real in this codebase — the five
downmix matrices in `mixer.rs::downmix_matrix` and the single-object VBAP panner
(`mixer.rs::vbap_bed_gains`, now live in the dashboard's ADM Panner page) — extended by direct,
clearly-flagged analogy where nothing exists yet. Nothing here is implemented; this is the
reference the next implementation pass works from.

## The general rule (what decides which pan object applies)

1. **Destination is Mono** → always **Mono Sum**. No spatial image exists to preserve; every
   source, whatever its own type, is just summed in (LFE excluded by convention). No panner button
   — a plain on/level send, same as today's ordinary `sends`.
2. **Destination is Stereo**:
   - Source is Mono → **Pan-Pot**: one knob, the classic constant-power law
     (`gainL = cos(θ), gainR = sin(θ)`, θ mapped from pan -1..+1), not the VBAP ring — a different,
     simpler, well-established law for exactly two speakers.
   - Source is Stereo → **Balance**: direct L→L/R→R, only a balance trim — a stereo source already
     carries its own image; there's nothing to reposition.
   - Source is Quad/5.1/7.1/5.1.4 → **Static Downmix** (source is "bigger" than a 2-speaker
     destination — see rule 4).
3. **Destination is a bed with real angular structure** (Quad/5.1/7.1/5.1.4 — "complex" in the
   user's own words) → the **VBAP ring panner** (this session's ADM Panner page), generalized from
   today's single point to **N points, rigidly linked to each other's relative angle**:
   - Mono source = 1 point → today's **Object Panner**, unchanged, already live.
   - Stereo source = 2 points → **Linked-Pair Panner**: the *same* ring UI, except the pair is
     always linked (inherent to being a stereo source, not a user-created gang) — width is always
     live, no "Link sources" step.
   - Quad/5.1/7.1 source = the source's own real channels, each keeping its own fixed angle
     relative to the others (from `ChannelLayout::roles()`/`role_angle`), rotating and — only when
     the destination has a height ring — elevating together as one rigid body.
4. **A source "bigger" than its destination** (more real channels, or a topology the destination
   can't represent) never gets a live panner — it gets a **Static Downmix** instead, same as
   today's principle for 5.1→Stereo etc. "Bigger" here means: strictly more channels, **or** the
   5.1.4-specific rule below.
5. **Topology rule (the one the user stated explicitly)**: a two-ring source (5.1.4 — bed ring +
   height ring) can only rotate against a two-ring destination (5.1.4). Rotating a two-ring rigid
   body into a single-ring destination has no well-defined meaning (which ring wins?), so it simply
   isn't offered — 5.1.4 as a source targeting anything other than 5.1.4 always falls to Static
   Downmix, regardless of channel count.
6. **Same layout on both sides** → still the rigid-array panner (rule 3), not a bypass — rotating a
   same-topology bed by some offset is a legitimate creative move (rotate a whole 5.1 print 15°).
   At the default 0° rotation this is mathematically identical to today's plain 1:1 patch (VBAP's
   own gain is exactly 1.0 at a perfect angle match), so nothing already working regresses.

## Legend

| Code | Pan object | What it is |
|---|---|---|
| **MS** | Mono Sum | Fixed sum, no panner button (level/on only) |
| **PP** | Pan-Pot | One knob, constant-power mono→stereo law |
| **BAL** | Balance | Direct L/R + trim, no repositioning |
| **DMX** | Static Downmix | Fixed matrix, no live panner button |
| **OBJ** | Object Panner | 1-point VBAP ring (built, live today) |
| **LNK** | Linked-Pair Panner | 2-point VBAP ring, always-linked width |
| **RIG(N)** | Rigid N-Point Panner | N-point VBAP ring, fixed relative angles, rotate (+elevate if dest has a height ring) |

## The matrix

Columns = track/source type. Rows = bus/destination type.

| Dest ↓ / Source → | Mono | Stereo | Quad | 5.1 | 7.1 | 5.1.4 |
|---|---|---|---|---|---|---|
| **Mono**   | MS | MS | MS | MS | MS | MS |
| **Stereo** | PP | BAL | DMX¹ | DMX✅ | DMX✅ | DMX✅ |
| **Quad**   | OBJ² | LNK² | RIG(4)² | DMX¹ | DMX¹ | DMX¹ |
| **5.1**    | OBJ | LNK | RIG(4) | RIG(5) | DMX✅ | DMX✅ |
| **7.1**    | OBJ | LNK | RIG(4) | RIG(5) | RIG(7) | DMX¹ |
| **5.1.4**  | OBJ✅ | LNK | RIG(4) | RIG(5) | RIG(7) | RIG(9)³ |

✅ = real, already implemented in `mixer.rs` today. ¹ = no existing matrix; proposed by direct
analogy below, **not verified against a published standard**, same caveat the existing code already
carries for its own extrapolated pairs. ² = Quad as a *destination* needs a new VBAP ring added to
`vbap_bed_gains` (today only 5.1/7.1/5.1.4 have ring data) — see Implementation notes. ³ = 9, not
10: `Surround5_1_4` has 10 roles but LFE never pans (`vbap_bed_gains` already zeroes it), so the
real rigid-array point count excludes it — same "exclude LFE" convention already applied to the
5.1/7.1 rows above (their own N already excludes LFE too); this cell's earlier "RIG(10)" label was
an inconsistency, caught by `mixer.rs`'s own `classify_matches_the_design_doc_matrix_exactly` test.

## Downmix coefficients

`k = 1/√2 ≈ 0.707` (`DOWNMIX_COEFF`, the real ITU-R BS.775 half-power constant already in
`mixer.rs`). `kk = k² = 0.5`.

### Already real (verbatim from `mixer.rs::downmix_matrix`)

- **5.1 → Stereo**: `Lo = L + k·C + k·Ls`, `Ro = R + k·C + k·Rs` (LFE excluded) — the one directly
  BS.775-verified pair everything else extrapolates from.
- **7.1 → Stereo**: `Lo = L + k·C + k·Lss + k·Lrs`, `Ro = R + k·C + k·Rss + k·Rrs`.
- **7.1 → 5.1**: `L,R,C,Lfe` pass 1:1; `Ls = Lss + Lrs`, `Rs = Rss + Rrs` (both side/rear pairs fold
  fully, no attenuation — a real 1:1 channel-count match per destination role, not a power sum).
- **5.1.4 → 5.1**: `L = L + k·Ltf`, `R = R + k·Rtf`, `C,Lfe` pass 1:1, `Ls = Ls + k·Ltb`,
  `Rs = Rs + k·Rtb` (each height channel folds into its nearest front/surround counterpart at -3dB).
- **5.1.4 → Stereo**: `Lo = L + k·C + k·Ls + kk·Ltf + kk·Ltb`, `Ro = R + k·C + k·Rs + kk·Rtf + kk·Rtb`
  (a height channel folding through the bed compounds to two -3dB steps, i.e. ~-6dB/0.5 linear —
  already the existing code's own documented reasoning).

### Proposed by direct analogy (¹ in the matrix — needs review before implementing)

Same folding principle as above (front channels pass 1:1, everything else folds toward its nearest
counterpart at `k`), extended to the pairs nothing currently defines:

- **Quad → Stereo**: `Lo = L + k·Ls`, `Ro = R + k·Rs` (no `C`/`Lfe` in Quad, nothing to fold there).
- **5.1 → Quad**: `L = L + k·C`, `R = R + k·C`, `Ls = Ls`, `Rs = Rs` (1:1) — `C` folds into
  front, `Lfe` dropped.
- **7.1 → Quad**: `L = L + k·C`, `R = R + k·C`, `Ls = Lss + Lrs`, `Rs = Rss + Rrs` (same
  side/rear-fold pattern as the real 7.1→5.1 matrix, plus `C` folding like 5.1→Quad above).
- **5.1.4 → Quad**: `L = L + k·C + k·Ltf`, `R = R + k·C + k·Rtf`, `Ls = Ls + k·Ltb`,
  `Rs = Rs + k·Rtb` (compose the 5.1.4→5.1 fold with the 5.1→Quad fold).
- **5.1.4 → 7.1**: lowest-confidence entry — 5.1.4's single `Ls`/`Rs` has no clean 1:1 target among
  7.1's *two* side/rear pairs (`Lss`/`Lrs` on a side). Flagging rather than inventing a number: this
  needs either a real published mapping or a deliberate product decision (e.g. `Lss = Ls`,
  `Lrs = k·Ls` — send the near side fully, the far side attenuated) before it's implemented, not a
  silent guess.

## Implementation notes (for whenever this is built — not done now)

- **Quad as a VBAP destination is new**: `vbap_bed_gains`/`vbap_supports_layout` only know
  `Surround5_1`/`Surround7_1`/`Surround5_1_4` today. Quad needs one more single-ring arm (same
  shape as the existing 5.1/7.1 arm — `roles().filter_map(role_angle)`, no height ring), plus adding
  `Quad` to `vbap_supports_layout`.
- **RIG(N) generalizes OBJ, it doesn't replace it**: `mix_into_scaled_with_object_pan` already
  takes one azimuth/elevation and pans one signal into a bed. The N-point case is the same
  function called once per source channel, each at that channel's own *fixed* angle **plus** the
  send's live rotation/elevation offset — not N independent objects. LNK (stereo) is the same
  mechanism at N=2, just exposed as an always-on link rather than opt-in gang.
- **A "downmix" send has no live panner button by design** (rule 4) — the UI's own "panner button
  called as soon as it's the way to solve the pan" (the user's own framing) simply doesn't render
  one for a DMX/MS/BAL/PP-classified pair; those show the fixed relationship (and PP's single knob,
  BAL's trim) instead.
- **Custom/arbitrary destination layouts** (the user's own "output custom fields" ask — a quad-like
  bed with different angles, or an operator-defined speaker set) are out of scope for this specific
  matrix (which only covers the six named `ChannelLayout` variants) but fit the same architecture
  cleanly: `vbap_bed_gains` already takes any `(role, angle)` ring — a custom layout just needs a
  way to declare its own ring(s) instead of reading from the fixed `role_angle` table, which
  `downmix_matrix`'s hardcoded per-pair matrices *don't* generalize as easily (a custom destination
  would need its own downmix coefficients defined too, or fall back to the count-only rule).
- **Discrete/no-layout sources or destinations** are unaffected — they keep today's existing
  count-only rule (`mix_into_scaled`'s equal/mono-expand/wide-to-mono-average cases), same as now.

## Addendum: user-editable coefficient tables

Confirmed with the user: the downmix matrices above should be runtime-editable, not fixed
constants. This has to live in the backend, not the dashboard — unlike the gang feature (pure UI
convenience with no real audio effect), these coefficients directly drive the live mix, so an
editable table only means something if mxl-test-app itself owns it.

- `downmix_matrix`'s current compiled `match` arms become a runtime table —
  `HashMap<(ChannelLayout, ChannelLayout), Vec<Vec<f32>>>` on `MixerState`, seeded at startup with
  exactly today's coefficients (the ✅ pairs above) as defaults, so a deployment that never touches
  this behaves byte-identically to now.
- Exposed over the same WS protocol as everything else: one JSON blob per `(src, dst)` pair (the
  same `matrix[dst_channel][src_channel]` shape `downmix_matrix` already returns), PUT to replace
  the whole matrix for that pair (matching the established "structured value, one PUT replaces it
  all" convention `sends`/`input-patch` already use), read back via the periodic broadcast tick —
  same reasoning as this session's own `adm-object` fix (a client needs a way to discover the
  current table on connect, not just after editing it).
- Persisted through the existing `state_path` save/load mechanism (`persistence.rs`) so an edited
  matrix survives a restart, same as every other runtime-mutable value in this app.
- Config-time override also available: a new optional top-level `"downmix_matrices"` key in the
  `.conf` file, same "config default, runtime-overridable" pattern every other setting here follows.
- Dashboard side: a small grid editor (its own page, same shape as the ADM Panner) — pick source/
  destination layout from two dropdowns, see the current matrix as an editable table with real role
  names as row/column headers (rows = destination roles, columns = source roles, e.g. 5.1→Stereo
  shows a 2-row × 6-column grid labeled L/R down the side and L/R/C/Lfe/Ls/Rs across the top), edit
  a cell, PUT the whole matrix back.
- Not addressed here (a smaller, separate decision if wanted later): whether the pan-object
  *classification* itself (which cell is DMX vs RIG(N) vs MS/PP/BAL) should also be overridable per
  pair, e.g. forcing a normally-RIG(4) pair to use a custom fixed downmix instead. The table above
  assumes the classification stays fixed and only the DMX cells' own numeric coefficients are
  user-editable.
