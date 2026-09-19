# Plan: adaptive meters (dashboard) + ADM-object VBAP panning (mxl-test-app)

## Status (updated 2026-09-14, end of autonomous 2-hour window)

**Both parts are fully implemented and tested.**

- **Part 1 (dashboard meters)**: done. `wwwroot/js/ppmCanvas.js`'s hardcoded `CHANNELS = 2` is gone
  — `initPPM` now reads a real per-instance channel count from a new `data-channels` attribute
  (`AudioMeter.razor`), and `updatePPM` self-corrects to the live `values.length` on every call
  (the authoritative source, since a fresh resource's first render can only guess via the
  placeholder single-element array `TrackStrip.razor`'s `PeakMeterValues` returns before any real
  peakmeter broadcast lands). `draw()`/`drawBars()` now render however many bars the resource
  actually has, evenly spread across the existing fixed canvas width (narrower per-bar as channel
  count grows — the canvas itself isn't resized, a separate follow-up if a roomier 5.1.4 meter is
  wanted later). `dotnet build` clean. **Not visually confirmed in a real browser** — no headless
  browser was available in this environment (`chromium`/`playwright` absent) to screenshot the
  actual canvas render; the dashboard is up at `http://localhost:5005` with the fix live and the
  server-side wiring double-checked by hand, but please open it and eyeball a wide-channel strip
  (any of `dynamic-rx`'s 8ch tracks/bus, port 3222) when you're back.
- **Part 2 (ADM-object VBAP panning)**: done, in `mixer.rs`/`engine.rs`, exactly per the confirmed
  scope below. 9 new unit tests, all passing (full suite: **115/115**). Live-verified that the new
  code path doesn't regress `warn_incompatible_sends` (a real bug caught *during* this work, see
  below) — did **not** live-verify actual panned audio through a real signal chain (would need
  rigging a live source through input-grid/IS-05 patching into the ADM-object track first, non-
  trivial extra setup for marginal extra confidence beyond the precise, exact-value unit tests
  already in place — same trade-off already made and documented for last night's Phase B).

**One real bug caught mid-implementation**: `topology::warn_incompatible_sends` had no idea the new
VBAP path exists — it would have logged a spurious "channel count is not compatible" warning for
every ADM-object track sending into a VBAP-supported bed bus (exactly the *intended*, newly-working
configuration), even though `engine.rs` handles it correctly. Fixed by adding `mixer::
vbap_supports_layout` and consulting it alongside the existing `channels_compatible`/
`layouts_compatible` checks. Confirmed fixed live: restarted `mxl-test-app-adm-demo.conf` (its
"Dialogue" ADM-object track's sends extended to also target the 5.1 Bus, bus id 3) — zero
"incompatible" warnings in the log.

**Unrelated port collision found and fixed along the way**: `mxl-test-app-adm-demo.conf`'s
`ws_port` (3220) collided with `mxl-signal-gen (generate)`, started earlier this session for the
COP1/COP2 hardware work and still running — changed the demo config's port to **3223** (was 3220).

Written to work autonomously for ~2 hours while the user is away, per their explicit request
("plan that autonomous"). Two independent pieces of work, in two different repos:

1. **AudioMixerDashboard** (`~/DEV/audiomixer`): the meter widget hardcodes 2 channels — fix so it
   adapts to a track's/bus's real channel count (mono/stereo/quad/5.1/7.1/5.1.4/discrete-N).
2. **mxl-test-app** (`~/DEV/nmos/aes67-linux-daemon/mxl-test-app`): an ADM-object track's own live
   position should drive real per-channel panning coefficients into a layout-tagged bed bus,
   independent of (multiplied with, not replacing) the existing scalar `send.level_db`.

## Confirmed decisions (from the user, don't re-ask)

- Panning applies **only** to a track with `adm_object: Some(...)` sending into a bed bus with a
  known named layout — driven by that object's own live `position` (azimuth/elevation). An
  ordinary (non-ADM-object) track keeps today's exact existing rule (dual-mono/BS.775 downmix,
  `mixer::mix_into_scaled_with_layout`, last night's Phase B) — **completely untouched, zero
  regression risk** on that already-tested path.
- Algorithm: VBAP (Vector-Base Amplitude Panning), the real algorithm real ADM/object-audio
  renderers use — not a toy azimuth-only cosine law.
- Layout coverage this pass: **5.1, 7.1, and 5.1.4** (5.1.4 explicitly requested, including its
  height layer). Mono/Stereo/Quad beds are out of scope (never asked about) — an ADM-object track
  sent to one of those keeps today's existing fallback rule, same as an ordinary track would.
- Distance/width stay inert this pass (VBAP is direction-only: azimuth+elevation as a unit
  vector) — `gain_db` (object-level) and `send.level_db` still scale the final result, exactly as
  today. Explicitly out of scope, flagged for later.
- This is **additive only** — `mixer::downmix_matrix`/`mix_into_scaled_with_layout` (Phase B, already
  tested and live-verified last night) are not modified at all. VBAP is a new, parallel code path
  engaged only when the sending track has a live `adm_object`.

## Part 1 — Dashboard: adaptive meters

### The bug (confirmed by reading the code, not assumed)

`wwwroot/js/ppmCanvas.js`:
- `const CHANNELS = 2;` at module scope.
- `initPPM` allocates `levels`/`peakLevels`/`peakHolds`/`holdTimers` all sized to exactly 2,
  regardless of how many values the Blazor side actually has (`Channel.InferredChannelCount`,
  `Models/Parameter.cs`, which is *already* correct — it reads the real backend-reported channel
  count from the `channel-list`/`sum-list` broadcast or the peakmeter array's own length).
- `updatePPM(canvas, values)` only ever copies `values[0]`/`values[1]`.
- `draw(state)` hardcodes exactly two `drawBar()` calls at two fixed X positions.

Net effect: a mono track always shows a dead second bar; anything wider than stereo (quad/5.1/7.1/
5.1.4/an 8ch discrete track) silently loses every channel past the first two — never drawn, no
error, no warning. This is the literal bug behind the user's "adapted to bus type audio channel
count number of meters."

### The fix

- `initPPM(canvas)`: read the real channel count at init time. `AudioMeter.razor` already knows
  it (`Values` is bound from `PeakMeterValues`/`GetPeakMeterValues()`, itself sized from
  `InferredChannelCount`) — pass it through explicitly as a new `data-channels` attribute on the
  `<canvas>` (matching the existing `data-compact`/`data-theme` convention already used for
  `compact`/`theme`), so `initPPM` reads `canvas.dataset.channels` instead of assuming a fixed 2.
- Re-size `levels`/`peakLevels`/`peakHolds`/`holdTimers` to that real count (module-level `CHANNELS`
  constant becomes a per-instance `state.channels` field instead).
- `updatePPM`: loop `for (let i = 0; i < state.channels; i++)`, not a fixed 2. **Also handle a
  channel-count change on an existing instance** — a track's own channel count is fixed for its
  lifetime (backend never changes it live), but if the *same DOM canvas element* gets reused for a
  different resource (Blazor component reuse across a re-render) the arrays need to be resized,
  not just overwritten out of bounds. Detect via `values.length !== state.channels` and reallocate.
- `draw(state)`: replace the two hardcoded `drawBar` calls with a loop over `state.channels`,
  spacing bars evenly across the available canvas width (`barWidth`/gaps scaled down as channel
  count grows, so a 10-channel 5.1.4 meter still fits the existing canvas dimensions — same
  `DefaultCanvasWidth`/`CompactCanvasWidth` constants in `AudioMeter.razor`, not resized, since
  changing the strip's own physical layout width is a bigger, separate design decision the user
  hasn't asked for). A reasonable rule: `barWidth = max(2, floor((canvasWidth - margins) /
  channelCount) - gap)`, clamped so it never overlaps `drawScale`'s reserved right-hand strip in
  non-compact mode.
- `AudioMeter.razor`: add a `data-channels="@(Values?.Length ?? 2)"` attribute to the `<canvas>`
  element (mirrors `data-compact`/`data-theme` exactly).

### Verification

- `dotnet build` clean.
- Live: with the dashboard already pointed at the `dynamic-rx` mxl-test-app instance (port 3222,
  already running with 3x 8ch tracks + 1x 8ch bus), open the Mixer page and visually confirm each
  strip's meter shows the real channel count (8 bars for the 8ch Gen/COP1/COP2 tracks and Monitor
  Bus) instead of 2. Also spot-check a 1ch (mono) track shows exactly 1 bar, not a dead second one.

## Part 2 — mxl-test-app: ADM-object VBAP panning

### Real, verified speaker angles (not guessed)

Fetched directly from `ebu/libadm`'s own `resources/common_definitions.xml` (the real BS.2051
common-definitions catalog a maintained, spec-conformant ADM library ships), via
`gh api repos/ebu/libadm/contents/resources/common_definitions.xml`. Confirms `layout.rs`'s own
existing role naming (`Lss`/`Rss` = BS.2051 "SideLeft"/"SideRight" M±090; `Ltf`/`Rtf`/`Ltb`/`Rtb`
role order = the exact `9.1_5.1.4_(4+5+0)` pack's own channel order) was already correct.

| `ChannelRole` | azimuth | elevation | BS.2051 source |
|---|---|---|---|
| `L`   |  30° | 0° | M+030 |
| `R`   | -30° | 0° | M-030 |
| `C`   |   0° | 0° | M+000 |
| `Lfe` | **excluded from VBAP entirely** — see below | | |
| `Ls`  | 110° | 0° | M+110 |
| `Rs`  |-110° | 0° | M-110 |
| `Lss` |  90° | 0° | SideLeft M+090 |
| `Rss` | -90° | 0° | SideRight M-090 |
| `Lrs` | 135° | 0° | BackLeftMidDiffuse M+135_Diff |
| `Rrs` |-135° | 0° | BackRightMidDiffuse M-135_Diff |
| `Ltf` |  30° |30° | TopFrontLeft U+030 |
| `Rtf` | -30° |30° | TopFrontRight U-030 |
| `Ltb` | 110° |30° | TopSurroundLeft U+110 |
| `Rtb` |-110° |30° | TopSurroundRight U-110 |

`Lfe` excluded from VBAP's own speaker set deliberately — real object-audio renderers never pan a
source directly to LFE (it's a dedicated low-frequency-effects channel, not part of the spatial
image); an ADM object's panning always contributes `0` gain to a bed's LFE channel. `M`/`Mono` has
no BS.2051 angle at all (out of scope this pass per the confirmed decision above).

### Algorithm — honest scoping (this is *not* full spherical-triangulation VBAP)

True textbook VBAP needs a pre-computed triangulation of the loudspeaker set (which speaker
*pairs*/*triplets* legitimately bound a region of the sphere) — for a fully generic, arbitrary
layout that's a real convex-hull-on-a-sphere computation. For the three *specific, fixed* layouts
in scope here, a much simpler and fully tractable scheme is both correct and honest about what it
is:

- **5.1/7.1 (horizontal-only, every speaker at elevation 0°)**: classic **2-speaker pairwise VBAP**
  around the horizontal ring — sort the layout's speakers by azimuth, find the adjacent pair
  bracketing the object's own azimuth (wrapping at ±180°), solve the real 2-speaker VBAP gain
  equations for that pair (a 2x2 linear solve, not an approximation — this *is* real VBAP for the
  2D case), zero everywhere else.
- **5.1.4 (two elevation rings — 0° bed, 30° height)**: the same horizontal pairwise-VBAP solve
  *independently* on each ring (bed ring: L/R/C/Ls/Rs; height ring: Ltf/Rtf/Ltb/Rtb), then blend
  the two rings' results by elevation — `t = clamp(object_elevation / 30°, 0, 1)`, final gain per
  channel = `(1-t) * bed_ring_gain` (bed channels only) or `t * height_ring_gain` (height channels
  only), each ring's own pairwise result already normalized to unit power *within its own ring*
  before the elevation blend (so total power stays sensible at every elevation, including the
  t=0/t=1 edges which reduce to plain single-ring VBAP exactly). **This is a documented
  two-ring approximation of full 3D VBAP, not textbook spherical triangulation** — correct and
  reasonable for exactly this speaker arrangement (no literal ceiling speaker exists in 5.1.4 to
  triangulate against anyway), but flagged explicitly here and in the code's own doc comment so a
  future session doesn't mistake it for a fully general 3D VBAP engine.

### Where this lives (`mixer.rs`, alongside last night's `downmix_matrix`)

- New module-private angle table: `fn role_angle(role: ChannelRole) -> Option<(f64, f64)>` (azimuth,
  elevation in degrees; `None` for `M`/`Lfe`).
- New `fn vbap_pairwise_gains(roles: &[ChannelRole], azimuth: f64) -> Vec<f32>`: the 2-speaker
  ring solve described above, for a single elevation ring's own role list.
- New `fn vbap_bed_gains(dst_layout: ChannelLayout, azimuth: f64, elevation: f64) -> Option<Vec<f32>>`:
  dispatches to the 5.1/7.1 single-ring case or the 5.1.4 two-ring blend; `None` for any other
  layout (Mono/Stereo/Quad/Discrete) — caller falls back to the existing rule untouched.
- New `pub fn mix_into_scaled_with_object_pan(src, dst, frames, scale, azimuth, elevation, dst_layout: Option<ChannelLayout>)`:
  applies `vbap_bed_gains` when `dst_layout` resolves to one, else falls back to
  `mix_into_scaled_with_layout`'s own existing behavior unchanged (so a track with `adm_object` set
  but sending into an unsupported bed layout, or a `Discrete` bus, behaves exactly as before —
  no silent behavior change outside the 3 covered layouts).
- `engine.rs`'s track→bus send loop (the exact call site `mix_into_scaled_with_layout` was wired
  into last night): if `track.adm_object` is `Some`, read its live `position` (already a
  `Mutex<AdmObjectMetadata>`, already updated live via WS/S-ADM import) and call
  `mix_into_scaled_with_layout_object_pan` instead; else keep calling
  `mix_into_scaled_with_layout` exactly as today.

### Unit tests (new, in `mixer.rs`'s existing test module)

- A source dead-center in front of a 5.1 bed (azimuth 0) pans entirely to `C` (gain 1.0 there, ~0
  elsewhere) — the trivial, easiest-to-verify-by-hand VBAP case.
- A source exactly between `L` and `R` at azimuth 15° (halfway) splits energy roughly evenly between
  them (equal-power point) — the standard textbook VBAP sanity check.
- A source at azimuth 110° (exactly at `Ls`'s own position) pans entirely to `Ls`.
- 5.1.4: a source at elevation 30°, azimuth 30° (exactly at `Ltf`) pans entirely to `Ltf`, zero on
  every bed-ring channel.
- 5.1.4: a source at elevation 15° (halfway between bed and height rings) contributes to both rings.
- LFE always receives exactly `0.0` regardless of source position.
- A track with `adm_object: Some` sending into a `Discrete`/Quad/unsupported-layout bus falls back
  to `mix_into_scaled_with_layout`'s exact existing behavior (regression guard).

### Live verification plan

1. `cargo test --release` — full suite must stay green (currently 106/106) plus the new tests above.
2. `cargo build --release`, restart the `mxl-test-app-adm-demo.conf` reference instance (already has
   a live ADM-object track, "Dialogue", currently sending into the Stereo Bus — extend its `sends`
   to also target the 5.1 Bus for this test, or add a fresh demo track/bus pair for a clean check).
3. Drive the object's live position via `POST /adm.xml` or the existing `channel/{id}/adm-object` WS
   param to a few known points (dead center, hard left at `Ls`'s own azimuth, straight up-front at
   `Ltf`'s position for the 5.1.4 case) and confirm — via the WS meter broadcast on the destination
   bus — gain concentrates on the expected channel(s) at each position, not spread arbitrarily.

## Explicitly out of scope this pass (confirmed with the user)

- Distance/width affecting the panning math (stays inert, round-trips correctly, just doesn't
  change gains yet).
- Extending VBAP to ordinary (non-ADM-object) tracks, or to Mono/Stereo/Quad bed targets.
- Replacing/touching last night's `downmix_matrix`/`mix_into_scaled_with_layout` — untouched.
- A fully general, arbitrary-layout 3D VBAP triangulation engine — the two-ring approach above is
  a deliberate, documented, correct-for-these-three-layouts simplification, not a stepping stone
  left half-built toward a generic engine (if a future layout needs real 3D triangulation, that's
  new work, not an extension of this).

## How to resume if interrupted

- This file is the plan; check `git status`/`git diff` in both repos to see what's actually landed
  vs. still planned.
- The dashboard fix (`ppmCanvas.js`/`AudioMeter.razor`) and the backend fix (`mixer.rs`/`engine.rs`)
  are fully independent — either can be finished/tested without the other.
- Reference instances already running as of plan-write time: `mxl-test-app-dynamic-rx.conf` (port
  3222), `mxl-test-app-adm-demo.conf` (port 3220, needs restart to pick up any backend rebuild),
  AudioMixerDashboard (`http://localhost:5005`, needs restart to pick up any frontend rebuild).
