# Handoff: channel layouts + ADM object metadata for mxl-test-app

Written to let a fresh session pick this up overnight with full context, without re-deriving
what a previous session already confirmed by reading the real code. Everything below marked
"confirmed" was verified directly against this repo's source (file:line citations included) —
treat it as fact, not as something to re-investigate from scratch.

## Status (updated 2026-09-14, end of overnight session)

**All 5 phases (A through E) are fully implemented, unit-tested, and live-verified against a
running instance. The plan is complete.** 106/106 tests pass (`cargo test --release`). See
"Progress" below for exactly what was built, where, and how it was verified — read that before
re-reading the original plan text further down, which now describes what was *proposed* (mostly
still accurate as a description of the approach actually taken, but the Progress section is the
authoritative record of what's real).

**Not yet done, deliberately left for the user**: nothing code-related — the plan's own scope is
finished. Two things outside this plan's scope that a fresh session (or the user) should know
about:
1. An unrelated, already-committed fix from earlier in this same session
   (`decklink-mxl-gateway`, commit `f7c85bf`, "Don't tear down a Tx pipeline that's genuinely
   transmitting just because SDP never populates") was never pushed — the push question was asked
   and never actually answered before the topic moved on. It's the user's own repo (`origin`), safe
   to push, but left alone rather than pushed without explicit confirmation.
2. The `mxl-test-app-adm-demo.conf` reference instance's `state_path` points into this session's
   scratchpad (`/tmp/claude-1000/.../scratchpad/adm-demo-state.json`) — move it to a durable path
   if this instance needs to keep surviving restarts beyond this session's temp-file lifetime.

## Progress

- **Phase A** (`src/layout.rs`, new): `ChannelRole` (15 roles, `short_name()`/`adm_speaker_label()`
  verified against real SMPTE ST 428-12 / ADM common-definitions `RC_`-prefixed forms via
  WebSearch/WebFetch — see the module's own doc comment for exactly which labels are spec-confirmed
  vs. pattern-followed), `ChannelLayout` (`Mono`/`Stereo`/`Quad`/`Surround5_1`/`Surround7_1`/
  `Surround5_1_4`/`Discrete(u32)`, `.channel_count()`/`.roles()`/`.role_at()`), and
  `resolve_channels()` — the one shared validation/resolution function every `layout`-bearing
  config struct funnels through (hard-errors if an explicit `channels` disagrees with a named
  layout; supplies `channels` when unset; `Discrete` never implicitly sets `channels`). Wired into
  `config.rs` (`layout: Option<ChannelLayout>` added to `TrackConfig`/`BusConfig`/
  `MasterTrackConfig`/`InputGridEntryConfig`/`OutputGridEntryConfig`), `topology.rs`
  (`build_track`/`build_bus`/`build_master` now return `Result<_, String>` and call
  `resolve_channels`), and `main.rs` (startup construction hard-fails via `?`; state-file topology
  reconstruction warns+skips, matching the existing malformed-entry precedent; input/output-grid
  entries also validated). **Live-verified**: a deliberately-inconsistent config
  (`channels: 2` + `layout: surround5_1`) fails startup with
  `Error: track 1: layout Surround5_1 implies 6 channel(s) but channels is explicitly set to 2 --
  remove one or make them agree`, exit code 1.

- **Phase B** (`src/mixer.rs`): `Track`/`Bus`/`MasterTrack` gained a `layout: Option<ChannelLayout>`
  field (populated from `cfg.layout` at construction). New `downmix_matrix(src, dst)` implements 5
  real matrices — **5.1→stereo verified directly against ITU-R BS.775's own equation**
  (`Lo = L + 0.707·C + 0.707·Ls` etc.); the other 4 (7.1→stereo, 7.1→5.1, 5.1.4→5.1, 5.1.4→stereo)
  are a documented, principled-but-not-independently-spec-verified extrapolation of that same rule
  — see `downmix_matrix`'s own doc comment for exactly what's verified vs. extrapolated, and
  revisit against ITU-R BS.775 Annex 4 / Dolby's Atmos-bed downmix guidance before mastering-
  critical use. New `mix_into_scaled_with_layout()` applies the matrix when both layouts are known
  and a matrix exists for that pair, else falls back to the untouched, byte-identical
  `mix_into_scaled`. `layouts_compatible()` added so `topology::warn_incompatible_sends` doesn't
  flag a mismatch the matrix actually handles. `engine.rs`'s track→bus send loop now calls
  `mix_into_scaled_with_layout` instead of `mix_into_scaled` directly (the bus-in→bus summing call
  stays on plain `mix_into_scaled`, always equal-length by construction). 4 new unit tests in
  `mixer.rs` (exact-matrix-value checks for 5.1→stereo and 7.1→5.1, plus two fallback-is-unchanged
  regression tests). **Live-verified**: the ADM demo config (below) sends 5.1/7.1/5.1.4 tracks into
  both a stereo bus and a 5.1 bus with zero incompatible-send warnings logged, confirming
  `layouts_compatible` correctly recognizes every one of those 5 pairings.

- **Phase C** (`src/nmos/resources.rs`, `src/patch.rs`): `InputGridEntry`/`OutputGridEntry` gained
  a `layout: Option<ChannelLayout>` field (populated from the corresponding config entry's own
  `layout` in `main.rs`; `None` for network-auto-discovered input-grid entries,
  `nmos/discovery.rs`, which have no config to read one from). `channels_json()` now emits real
  `ChannelRole::short_name()` labels when the owning entry has a named layout whose role count
  agrees with the channel count, else falls back to the original generic `"Channel N"` behavior
  (defensive fallback only — `resolve_channels` already guarantees agreement for any real
  resource). 4 new unit tests. **Live-verified** via `curl .../x-nmos/node/v1.3/sources/` against
  the running ADM demo instance — confirmed real output:
  `5.1 Out -> ['L', 'R', 'C', 'LFE', 'Ls', 'Rs']`, `7.1 Out -> ['L', 'R', 'C', 'LFE', 'Lss', 'Rss',
  'Lrs', 'Rrs']`, `5.1.4 Out -> [..., 'Ltf', 'Rtf', 'Ltb', 'Rtb']`, etc.

- **Phase D** (`src/adm.rs`, new): `AdmPosition { azimuth, elevation, distance }` (spherical, not
  Cartesian — verified directly against BS.2076's own `audioBlockFormat` Objects typeDefinition via
  the EBU ADM Guidelines' worked coordinate-system example; `distance` defaults to `1.0`, the real
  BS.2076 reference distance, not `0.0`) and `AdmObjectMetadata { name, gain_db, position, width,
  height, depth }` — `gain_db` deliberately stays in this app's own dB convention rather than
  ADM's native linear `<gain>` (to be converted only at Phase E's XML boundary, matching this
  codebase's existing "one internal unit, convert at the real external boundary" pattern). Wired
  into `config.rs` (`TrackConfig.adm_object: Option<AdmObjectMetadata>`), `mixer::Track`
  (`adm_object: Option<Mutex<AdmObjectMetadata>>`, `None` for an ordinary bed track), `ws.rs` (new
  `channel/{id}/adm-object` PUT/GET param, following the exact `sends`-style established
  convention; PUT on a track with no slot logs a warning and no-ops, doesn't panic), and
  `persistence.rs` (`adm_object` added to both `capture()`'s per-track live-value section and its
  dynamically-created-topology section, plus `apply_snapshot` resume logic). 5 new unit tests
  (`adm.rs` round-trip + defaults, `persistence.rs` capture/resume round-trip and the
  ordinary-bed-track-stays-null case). **Live-verified end-to-end** against the running ADM demo
  instance: PUT via a raw WebSocket client
  (`/tmp/.../scratchpad/ws_probe.py` — a from-scratch client since `websockets` isn't pip-
  installable in this environment's externally-managed Python; reusable for future live WS checks)
  → echoed back correctly; PUT on a non-ADM track → correctly rejected with a logged warning,
  `null` echo; SIGTERM → confirmed the state file captured the live value; deleted the state file,
  restarted, confirmed `"resumed live state from previous run"` in the log; deleted the state file
  *again* and re-triggered a save from live memory only (no old file to fall back to) — the value
  reappeared, proving it genuinely round-tripped through the live `Mutex` slot, not just an inert
  copy sitting in JSON.

- **Phase E** (`src/adm.rs`'s S-ADM section, new; `Cargo.toml` gained `quick-xml = "0.36"`; two new
  HTTP routes in `main.rs`): `to_sadm_xml(mixer) -> String` and
  `from_sadm_xml(xml) -> anyhow::Result<Vec<AdmObjectUpdate>>` plus `apply_sadm_updates`. The real
  S-ADM `<frame version="ITU-R_BS.2125-1">`/`<frameHeader><frameFormat .../></frameHeader>`
  wrapper, and every `audioObject`/`audioPackFormat`/`audioChannelFormat`/`audioBlockFormat`/
  `position`/`speakerLabel` element and attribute name, were verified directly against **real,
  accepted test fixtures fetched from `ebu/libadm`** (a maintained, spec-conformant ADM library)
  via `gh api repos/ebu/libadm/contents/...` — `tests/test_data/simple_scene_itu.accepted.xml` and
  `tests/test_data/write_total_time_reference.accepted.xml` — not guessed or reconstructed from
  memory (see `adm.rs`'s own S-ADM section doc comment for the exact citation and the deliberate
  scope boundary: single-frame snapshot export only, no real timeline, no track/stream-number
  binding — all explicitly out of scope per the plan's own "no live wire transport" decision).
  `to_sadm_xml` exports one real ADM object (position/gain/extent, `gain_db` converted to ADM's
  native linear form via the new `mixer::linear_to_db`/existing `db_to_linear`) per
  `adm_object`-tagged track, plus one real DirectSpeakers bed (one `audioChannelFormat` per
  channel, correct per-channel `audioBlockFormat`/`speakerLabel` using `ChannelRole::
  adm_speaker_label()`) per remaining track with a known named `layout`. `from_sadm_xml` round-
  trips exactly what `to_sadm_xml` produces (matches an `audioObject` to its `audioBlockFormat` by
  document order — not a full ID-cross-reference resolver, a deliberate, documented scope choice).
  Two new HTTP routes on the existing Node-API port (`main.rs`): `GET /adm.xml` (export) and
  `POST /adm.xml` (import, `200`+applied-count on success, `400`+real parse error on malformed
  XML). 9 new unit tests in `adm.rs` (frame/object shape, DirectSpeakers bed shape, full
  write→parse round trip, DirectSpeakers-blocks-correctly-ignored, apply-by-name, apply-ignores-
  unknown-name). **Live-verified end-to-end** against the running ADM demo instance:
  `curl http://172.30.3.77:3220/adm.xml` returned the real, well-formed document (confirmed every
  section — the "Dialogue" object's live position/gain, and a correct DirectSpeakers bed for every
  one of the 5 layout-tagged tracks); fetched the document, edited the object's azimuth via a
  plain Python string replace, `POST`ed it back (`applied 1 of 1 object update(s)`), re-fetched and
  confirmed the live azimuth actually changed server-side; confirmed malformed XML POSTed back
  returns `400` with `quick-xml`'s own real parse error message, not a silent failure or a panic.
  **One real bug caught during this phase's own verification**: the first live `curl` showed
  malformed `audioBlockFormatID`s for bed channels (e.g. `AB_0001AC_00011003_00000001` — the
  channel format's own already-prefixed id string had been concatenated wholesale instead of just
  its bare numeric instance number); fixed in `write_bed_channel_format` by carrying `(chan_id,
  chan_num)` pairs through the loop instead of re-deriving the block id from the full id string,
  rebuilt, re-verified live (`AB_00011003_00000001` now, matching the real convention shape) —
  purely a cosmetic/convention issue (the malformed ids were still unique strings, so the document
  was technically well-formed XML throughout), but worth knowing about if anything in this area is
  touched again.

**One real bug hit and fixed during verification**: after finishing the Phase D `ws.rs`/
`persistence.rs` edits, `cargo test --release` was run (which only rebuilds the *test* binary) but
`cargo build --release` (the plain binary actually being run as the demo instance) was not re-run
before the first live WS check — so the live check briefly appeared to fail (PUT echoed `null`)
against a stale binary that predated the whole `adm-object` param. Rebuilding
(`cargo build --release`) and restarting fixed it immediately; **lesson for any future session**:
after editing non-test code, `cargo build --release` before trusting a live process's behavior,
`cargo test --release` alone does not update `target/release/mxl-test-app`.

**Reference instance still running**: `mxl-test-app-adm-demo.conf` (this directory) — 2 input-grid
entries (mono/stereo), 6 output-grid entries (mono/stereo/quad/5.1/7.1/5.1.4), 6 tracks (one per
named layout plus one plain ADM-object track, id 6, currently holding a live "Dialogue" object at
azimuth 45°/elevation 10°), 3 buses (mono/stereo/5.1), `ws_port: 3220`, `instance_name: "adm-demo"`,
registered against the registry at `172.30.3.201`, state persisted to
`/tmp/claude-1000/.../scratchpad/adm-demo-state.json` (a scratchpad path — move to a durable
location if this instance needs to survive beyond this session's temp-file lifetime). Full test
suite: **106/106 passing** (`cargo test --release` in this directory).

## Original plan text follows (kept for Phase E's context and the parts of A-D's rationale not
## already restated above)

Everything below marked "confirmed" was verified directly against this repo's source (file:line
citations included) at plan-approval time — treat it as fact, not as something to re-investigate
from scratch.

## The ask

Add real support for standard multi-channel layouts (mono, stereo, quad, 5.1, 7.1, 5.1.4 — plus
today's generic "just a channel count" case) with actual channel-*role* semantics (which index is
Left/Right/Center/LFE/etc, not just a count), and a first real implementation of ADM (ITU-R
BS.2076) object-oriented audio metadata: a data model plus Serial ADM (BS.2125) XML import/export.

**Confirmed decisions from the approving session** (don't re-ask these):
- ADM scope: real object-metadata data model + S-ADM XML serialize/parse. **No live wire
  transport** (SMPTE ST 2110-41 or otherwise) this pass — that's a distinct, much larger spec
  with zero existing groundwork in this codebase, deserving its own dedicated plan later.
- Layout mutability: set at config-time or `CREATE` only, matching this app's existing "chain
  order fixed at construction, delete+recreate to change" convention (`PICKOFFS.md` §1a). No live
  PUT to change an existing resource's layout after the fact.

## Already-running reference instance

`mxl-test-app-16x16.conf` (in this directory) — 16 mono tracks → 16 mono buses → 16 output-grid
entries, a plain smoke-test config with no layout/ADM content, generated and verified live this
session (registered as "mxl-test-app 16x16" against the registry at `172.30.3.201`). Useful as a
known-good baseline to diff behavior against once layout/ADM work lands — every existing config
including this one must keep behaving byte-identically once layout support is additive (see
Phase A).

## What the current code actually does (verified directly, not assumed)

- **`mixer::mix_into_scaled`** (`src/mixer.rs:308-344`) is the one place a channel-count mismatch
  between a track's send and its target bus is handled today: equal counts sum 1:1; mono→wide
  dual-mono-expands (same sample to every bus channel); wide→mono averages down; **any other
  mismatch (e.g. a 5.1 track into a stereo bus) is a silent, permanent no-op**, only flagged once
  at startup by `channels_compatible`/`topology::warn_incompatible_sends`
  (`mixer.rs:349-351`, `topology.rs:47-51`). This is the main functional gap layout support needs
  to close — real layout-aware downmix matrices for the common standard paths.
- **`patch.rs`** crosspoints are explicit, per-channel, one-source-channel-to-one-dest-channel
  only — no automatic expansion at all (`set_track_in`/`set_bus_in` etc. require an exact-length
  array). Layout work does **not** need to touch this file's mismatch behavior, only
  `mixer.rs`'s bus-send path.
- **`dsp.rs`** stages (filter/eq/dynamics/phase/delay) are already fully per-channel-independent
  with zero role awareness — nothing here needs to change for layout support itself.
- **NMOS side** (`nmos/resources.rs:97-99`, `channels_json`): each Flow/Source channel is
  `{"label": "Channel N"}`, generic, always. The MXL flow descriptor (`flow.rs:8-37`) bakes in
  only a bare `channel_count: u32` at creation time — no layout metadata slot exists there
  either.
- **IS-08 is absent by deliberate design** — `PICKOFFS.md` explicitly discusses and rejects it in
  favor of the grid/patch model. Layout work stays inside the existing Flow-`channels[]`
  mechanism; it does not add IS-08.
- **Zero pre-existing layout/ADM vocabulary anywhere**: an exhaustive case-insensitive grep across
  the whole `mxl-test-app` tree (source + `PICKOFFS.md`) for `layout`, `channel_symbol`,
  `surround`, `5.1`, `7.1`, `LFE`, `ADM`, `audioObject`, `audioPackFormat`, `BS.2076`, `BW64`
  found zero genuine hits (only false-positive substring matches, e.g. `tower-http 0.7.1`'s
  version string). This is genuinely greenfield work, not an extension of something partial.
- **Extension points already established, to reuse rather than invent new ones**:
  - `TrackConfig`/`BusConfig`/`MasterTrackConfig` feed static config-load, state-file topology
    reconstruction, *and* runtime `CREATE` from one shared struct
    (`topology.rs:6-10`, "one implementation, not three to keep in sync") — a new optional field
    added to these structs is automatically available in all three places for free.
  - `ws.rs`'s `amixer/{mixerId}/{kind}/{id}/{param}` dispatch already has an established
    convention for a structured (non-scalar) param value (`sends`, `input-patch`, `stage` all
    work this way) — a new param slots in with one `match` arm each in
    `apply_track_param`/`current_track_value` (`ws.rs:274-334`, `:405-419`), no protocol change
    needed.
  - `persistence.rs` degrades new fields gracefully via `serde_json::Value` + `.get(...)`
    everywhere (no schema version, no migration mechanism needed) — but its per-track JSON
    sections are hand-built field lists (`capture()`, `persistence.rs:44-52` etc.), so a new field
    needs *explicit* addition there to survive a dynamically-created resource's restart (a
    config-authored resource doesn't have this gap, since it reloads fresh from `config.json`
    every restart).
  - 34 existing `#[serde(default)]` fields in `config.rs` confirm the additive-optional-field
    pattern is this codebase's own established, safe convention for exactly this kind of change.

## The plan (5 phases)

### Phase A — Channel layout vocabulary + config [DONE, see Progress above]

New module `src/layout.rs`:
- `ChannelRole` enum (`M`, `L`, `R`, `C`, `Lfe`, `Ls`, `Rs`, `Lss`, `Rss`, `Lrs`, `Rrs`, `Ltf`,
  `Rtf`, `Ltb`, `Rtb`, ...) with `.speaker_label(&self) -> &'static str`. **Verify exact ITU-R
  BS.2051/BS.2076 `speakerLabel` strings and canonical channel *order* per standard layout
  directly against the spec text while implementing this file — do not guess.** File/WAV
  convention vs. ITU vs. Dolby channel ordering genuinely differ; pick one, document the choice
  explicitly in the module doc comment.
- `ChannelLayout` enum: `Mono` (1ch), `Stereo` (2ch), `Quad` (4ch), `Surround5_1` (6ch),
  `Surround7_1` (8ch), `Surround5_1_4` (10ch: 5.1 bed + 4 height), `Discrete(u32)` (today's
  behavior — no role semantics, arbitrary count). Each exposes `.channel_count() -> u32` and
  `.roles() -> &[ChannelRole]`.
- `config.rs`: add `#[serde(default)] pub layout: Option<ChannelLayout>` to `TrackConfig`,
  `BusConfig`, `MasterTrackConfig`, `InputGridEntryConfig`, `OutputGridEntryConfig` — additive,
  matches the 34 existing `serde(default)` fields' convention exactly, zero effect on any config
  that doesn't set it (including `mxl-test-app-16x16.conf`).
- Startup validation (alongside the existing `channels_compatible`/`warn_incompatible_sends`
  check in `topology.rs`/`main.rs`): if `layout` is set and `channels` is also explicitly set,
  they must agree (hard error — mirrors this codebase's "validate at startup, don't guess at
  runtime" philosophy); if `channels` is unset, `layout.channel_count()` supplies it.

### Phase B — Layout-aware downmix (`mixer.rs`) [DONE, see Progress above]

- Give `Track`/`Bus` an optional `layout: Option<ChannelLayout>` field alongside their existing
  `channels: usize` (resolved the same place `channels` itself already is, `Track::new`/
  `Bus::new`).
- Extend `mix_into_scaled`'s current "any other mismatch → no-op" branch: when *both* sides have
  a known `ChannelLayout`, apply a standard downmix matrix instead (5.1→stereo, 7.1→stereo,
  7.1→5.1, 5.1.4→5.1, 5.1.4→stereo — the practically common paths) using published ITU-R
  BS.775/industry-standard coefficients (e.g. 5.1→stereo: `Lo = L + 0.707·C + 0.707·Ls`,
  `Ro = R + 0.707·C + 0.707·Rs`, LFE excluded by default). When either side's layout is unknown,
  behavior must be **byte-identical to today** — the existing 3-case rule and all its current
  tests stay untouched.
- New unit tests in `mixer.rs`'s existing test module (same round-trip/fixture style already
  used there): at minimum, 5.1→stereo produces the expected matrix result, and every existing
  no-layout-info case still behaves exactly as before.

### Phase C — Real speaker labels on NMOS resources [DONE, see Progress above]

- `patch.rs`: give `InputGridEntry`/`OutputGridEntry` an optional `layout: Option<ChannelLayout>`
  alongside their existing `channels: usize`, resolved from the new config field.
- `nmos/resources.rs::channels_json` (`:97-99`): when the owning grid entry has a known layout,
  emit each channel's real speaker label (`ChannelRole::speaker_label`) instead of the generic
  `"Channel N"`; fall back to today's behavior when layout is `Discrete`/unset. Zero change for
  any deployment that doesn't set `layout`.

### Phase D — ADM object metadata model [DONE, see Progress above]

New module `src/adm.rs`:
- `AdmPosition { azimuth: f64, elevation: f64, distance: f64 }` and `AdmObjectMetadata { name:
  String, gain_db: f32, position: AdmPosition, width: f64, height: f64, depth: f64 }` — field
  shapes/bounds/defaults verified against the ITU-R BS.2076 spec text directly while
  implementing (spherical vs. cartesian position representation is a real spec choice to
  confirm, not assume).
- `config.rs`: new optional `TrackConfig.adm_object: Option<AdmObjectConfig>` (additive, same
  convention as every other optional field here) — `None` for an ordinary bed/channel track (the
  default for every existing config), `Some` only for a track explicitly authored as an ADM
  object.
- `mixer::Track` gains `adm_object: Option<Mutex<AdmObjectMetadata>>`.
- `ws.rs`: new `channel/{id}/adm-object` param — one `match` arm each in `apply_track_param`
  (PUT, live position/gain/extent control) and `current_track_value` (GET/broadcast), following
  the exact established pattern `sends`/`input-patch`/`stage` already use for a structured value.
- `persistence.rs`: add `adm_object` to `capture()`'s per-track live-value section *and* to the
  dynamically-created-topology section (both hand-built field lists need it explicitly —
  confirmed this is not automatic) so a dynamically-created ADM-object track's position survives
  a restart the same way its gain/fader already do.

### Phase E — Serial ADM (S-ADM) XML serialize/parse [DONE, see Progress above]

- `adm.rs` gains `pub fn to_sadm_xml(mixer: &MixerState) -> String`: a real ITU-R
  BS.2125-conformant Serial ADM document — one `audioObject`/`audioPackFormat`/
  `audioChannelFormat`/`audioBlockFormat` per ADM-flagged track's current live
  position/gain/extent, plus a plain channel-bed `audioPackFormat`/`audioChannelFormat` set
  (using Phase A's role labels where a track/bus has a known layout) for everything else — exact
  element/attribute names and required-vs-optional fields checked against the BS.2125/BS.2076
  spec text directly, not guessed.
- `pub fn from_sadm_xml(xml: &str) -> anyhow::Result<Vec<AdmObjectUpdate>>` parsing enough to
  round-trip what `to_sadm_xml` produces, applying updates back onto matching tracks by name.
- Exposed via a new `GET /adm.xml` route in `server.rs` (export) and a POST/CREATE-style import
  route — exact routing shape matched to whatever web framework `server.rs` already uses (confirm
  during implementation, follow its existing route-registration convention).
- Round-trip unit tests in `adm.rs`'s own test module: serialize known metadata → parse → assert
  equality, matching the round-trip convention already established in `persistence.rs`/
  `config.rs`.

## Explicitly out of scope this pass

- Live wire transport of ADM metadata (ST 2110-41 or any other real-time mechanism) — flag as a
  distinct future plan.
- Live PUT-based layout changes after construction (per the confirmed decision above).
- A general NxM pan/matrix engine — only the specific standard downmix paths listed in Phase B
  get real coefficients; every other mismatch keeps today's existing rule.

## Verification

1. ~~`cargo test` in `mxl-test-app/` after each phase~~ — **done**, 100/100 passing
   (`config.rs`/`mixer.rs`/`persistence.rs`/`layout.rs`/`adm.rs`/`nmos/resources.rs` all covered).
2. ~~`cargo build --release`, run a companion config with mixed layouts + an ADM-object
   track~~ — **done**, `mxl-test-app-adm-demo.conf` (this directory), currently running on
   `ws_port: 3220`.
3. ~~Confirm real speaker labels via `curl`~~ — **done**, see Progress above for the exact
   confirmed output.
4. ~~Confirm a 5.1→stereo (etc.) send actually mixes, not a silent no-op~~ — **done** via the
   absence of any incompatible-send warning in the log for all 5 layout-mismatched sends in the
   demo config (direct proof `layouts_compatible` engaged); the underlying arithmetic itself is
   covered by exact-value unit tests in `mixer.rs` rather than a live meter read (a live meter
   check would need a real signal source patched into the track, out of scope for tonight's
   verification pass).
5. ~~`curl http://<host>:<ws_port>/adm.xml` returning spec-shaped XML, PUT-then-refetch, and
   round-tripping through `from_sadm_xml`~~ — **done**, see the Phase E entry in Progress above for
   the exact live sequence run (GET, edit, POST, re-GET, confirmed the change).

## How to run things locally (for reference)

```
cd /home/gregorbaumann/DEV/nmos/aes67-linux-daemon/mxl-test-app
cargo build --release   # NOTE: cargo test --release alone does NOT rebuild this binary — see the
                         # "one real bug hit" note above
./target/release/mxl-test-app mxl-test-app-16x16.conf      # 16x16 plain smoke-test reference
./target/release/mxl-test-app mxl-test-app-adm-demo.conf   # mixed-layout + ADM-object demo, port 3220
```

The MXL shared domain (`/dev/shm/mxl-shared-domain`) is a `tmpfs` path and does **not** survive a
host reboot — `mkdir -p /dev/shm/mxl-shared-domain` if a fresh `mxl-test-app` run fails at startup
with `Failed to create instance : filesystem error: Domain path is not a directory` (hit and fixed
once already this session, after the host was restarted mid-session for the unrelated DeckLink
firmware investigation — see this repo's git history / prior session context for that, unrelated to
this ADM/layout work).

Registry lives at `172.30.3.201` (see `local-register/start-register.sh` at the top of `~/DEV/`
if it needs restarting — it only starts the `nmos-registry` service, not the on-demand
`nmos-virtnode`/`nmos-testing` test tools also defined in that compose file).

A minimal raw-WebSocket Python client (no `pip install` needed — this environment's Python is
externally-managed and refuses `pip install websockets` without `--break-system-packages`) is
at `/tmp/claude-1000/-home-gregorbaumann-DEV-nmos/f67b3202-ed9c-464f-9b9f-ab5921973e22/scratchpad/ws_probe.py`
— usage: `python3 ws_probe.py <host> <port> /amixer/api/socket '<json-message>' ['<json-message>'...]`,
prints every text frame received in the following ~1s window. **Important**: its `recv_frames`
helper uses a real deadline, not a per-`recv()` timeout — this server streams periodic meter
broadcasts continuously, so a naive per-call timeout never actually elapses. That path is likely
useful again for Phase E's live route testing (a WS client isn't needed for the plain HTTP
`GET /adm.xml` route itself, but may help if any part of Phase E's design ends up going through the
existing WS protocol instead of a new HTTP route). It is a scratchpad file and will not survive
past this session — copy it somewhere durable first if a future session wants to reuse it.
