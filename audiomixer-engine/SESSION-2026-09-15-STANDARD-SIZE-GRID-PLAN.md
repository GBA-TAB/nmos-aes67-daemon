# Plan: standard-sized placeholder grid entries (Rx + Tx)

## Status (updated 2026-09-15, end of overnight autonomous session)

**All phases (A-F) are implemented, tested, and live-verified.** 119/119 tests pass
(`cargo test --release`). Both running reference instances (`mxl-test-app-dynamic-rx.conf`,
`mxl-test-app-adm-demo.conf`) restarted cleanly on the new binary and are healthy.

**Live-verified, reproducing tonight's exact original failure and confirming the fix**:
- PATCHing `adm-demo`'s 2ch "Stereo In" receiver to an 8ch sender ("Monitor Out") now returns a
  real `400` (`"flow has 8 channel(s), this receiver's placeholder only accepts up to 2"`) instead
  of the original `500`.
- PATCHing an 8ch placeholder receiver to a 2ch sender — impossible before tonight (exact-match
  only) — now succeeds (`200`), and the meter broadcast correctly shows the full 8-channel
  placeholder width (real 2 channels + 6 silent-padded), not truncated to the real subscribed count.

**One real design mistake caught and fixed mid-implementation, before it broke anything live**:
the plan as originally written applied the same *discrete bucket* check
(`is_standard_stream_size`, `{1,2,8,16,64}`) to both input **and** output grids. Building it and
test-migrating the reference configs immediately exposed the bug: `mxl-test-app-adm-demo.conf`'s
own output grid has real, legitimate 4ch (quad), 6ch (5.1), and 10ch (5.1.4) entries — none a
"round bucket" number, but every one a completely valid real ST 2110-30 payload (Level A's actual
constraint is the *range* 1-8, not the specific values 1/2/8). The bucket concept only makes sense
for Rx *capacity provisioning* (an operator picks a round placeholder size in advance); on Tx, an
entry's `channels` **is** the real transmitted signal, with no equivalent "placeholder vs. real"
distinction. Fixed by adding a second, range-based check
(`layout::is_valid_st2110_30_channel_count`, `1..=64`) used for output-grid entries instead —
see `layout.rs`'s own doc comments on both functions for the full reasoning. This is exactly the
kind of thing "verify before implementing, catch mistakes via real testing" is supposed to surface,
and it did.

**Phase F (NMOS capability advertisement) scope note**: real, verified BCP-004-01 research
(`specs.amwa.tv`'s own published example, fetched directly) confirmed `constraint_sets` is a
**Receiver-only** mechanism — there is no NMOS Sender-side equivalent (matches what was already
independently confirmed and documented in last night's ADM/layout handoff: empty `caps: {}` on
Senders/Sources is spec-normal). So Phase F only ever applied to `receiver_json` — there was never
a symmetric Tx piece of this phase to build, not an oversight.

## Original plan follows (kept for phase-by-phase implementation detail)

Builds directly on `SESSION-2026-09-15-DYNAMIC-RX-SIZING-DESIGN.md` (all 5 open questions there are
resolved — read that file first for the full reasoning behind each decision below). This file turns
those decisions into a real, phased build. Written to work autonomously overnight; the user is
asleep, no further questions until they're back — flagged decisions below are things I decided
myself, following this codebase's own established conventions, not things left silently ambiguous.

## Scope (confirmed with the user before they left)

Applies to **both** the input grid (Rx) and output grid (Tx) — not Rx-only. Does not touch
Track/Bus/MasterTrack's own `channels`/`layout` (internal mixer objects, unrelated to real 2110
stream profiles).

## Key implementation finding (from reading the code before writing this plan)

`patch.rs:618`'s `SourceRef::Input` resolution already does
`input_bufs.get(entry_id).and_then(|b| b.get(*channel)).map(Vec::as_slice)` — a channel index past
the end of whatever buffer length is actually stored already resolves to `None` (silence), not a
panic. **The crosspoint/patch layer needs zero changes** for "accepts up to N, silent beyond the
real subscribed count" (decision #2) — it was already built defensively enough. The real gaps are
narrower than the design doc worried about:
1. `FlowReader::open`'s own hard `channels != expected_channels` check (`src/flow.rs:146-148`).
2. `engine.rs`'s per-period read step (`:306-329`) stores the reader's own real-length buffer
   directly into `input_bufs`/`entry.meter_db` — shorter than the placeholder size whenever the
   real subscription is narrower. Patching is already safe against this (point above), but the
   entry's own **meter broadcast** would silently be the wrong (shorter) length instead of the full
   placeholder size with trailing silence — worth padding for a consistent operator-facing display,
   even though nothing downstream actually breaks without it (tonight's own dashboard meter fix is
   self-correcting to whatever length arrives either way).
3. Startup/CREATE-time validation that `channels` is one of the standard sizes at all.
4. NMOS capability advertisement (real BCP-004-01 research needed, not yet done).

## Phase A — Standard-size validation

- New small function in `layout.rs` (or a new tiny module if that reads oddly there —
  decide while implementing): `pub const STANDARD_STREAM_SIZES: [u32; 5] = [1, 2, 8, 16, 64];` and
  `pub fn is_standard_stream_size(n: u32) -> bool`.
- `InputGridEntryConfig`/`OutputGridEntryConfig` (`config.rs`): startup validation (`main.rs`'s
  existing layout-validation pass, alongside `resolve_channels`) hard-errors if `channels` (once
  resolved) isn't a standard size — same "validate at startup, don't guess at runtime" precedent
  `resolve_channels` itself already established last night. `channels` on these two config structs
  **is** the placeholder size now — no new field name (decision #4: this replaces the old "exact
  count" meaning, doesn't add a parallel concept).
- Same validation on `topology`-adjacent CREATE paths if input/output grid entries ever become
  runtime-CREATEable (confirmed during implementation: today they're static-config-only, no runtime
  CREATE exists for grid entries at all — only tracks/buses/masters can be CREATEd — so this is
  startup-config validation only, nothing to add on a WS CREATE path that doesn't exist).
- Migrate the three existing reference configs (`mxl-test-app-16x16.conf`,
  `mxl-test-app-adm-demo.conf`, `mxl-test-app-dynamic-rx.conf`) to comply — check each `input_grid`/
  `output_grid` entry's own `channels` (explicit or defaulted) against the standard set; fix any
  that aren't (expect most already are, since 1/2/8 are common existing choices).

## Phase B — Rx: accept-up-to-N + typed distinguishable error

- `FlowReader::open` (`src/flow.rs`): rename the parameter's own meaning in its doc comment
  (`expected_channels` → the placeholder size), change `if channels != expected_channels` to
  `if channels > expected_channels`. Store the *real* channel count on `FlowReader` as today
  (`self.channels`), not the placeholder — `read_next` already only ever returns real-channel-count
  data, that's correct and unchanged.
- New small public error type (`flow.rs`) implementing `std::error::Error`, e.g.
  `pub struct ChannelCountExceedsPlaceholder { pub actual: usize, pub placeholder: usize }` with a
  real `Display` message, wrapped via `anyhow::Error::from`/`.context()` so `FlowReader::open` still
  returns `anyhow::Result<Self>` (no wider refactor of this codebase's existing anyhow-everywhere
  convention) — the *specific* failure is recoverable via `e.downcast_ref::<ChannelCountExceedsPlaceholder>()`
  at the one call site that needs to distinguish it (below).

## Phase C — Status code fix (the original ask this whole design grew from)

- `nmos/server.rs`'s receiver-activation error handling (`:377-384`): downcast the error; if it's
  `ChannelCountExceedsPlaceholder`, return `400 Bad Request` with a real, specific error message
  ("sender has N channels, this receiver's placeholder only accepts up to M"); every other
  `FlowReader::open` failure (not-found flow, MXL subsystem fault, etc.) keeps today's `500`
  — genuinely a server-side fault, not a client mistake.

## Phase D — Engine: pad the meter (and input_bufs entry) to the full placeholder size

- `engine.rs`'s per-period read step (`:306-329`): after a successful `read_next`, if
  `planar.len() < entry.channels` (placeholder), pad `planar` with `entry.channels - planar.len()`
  empty/zero-filled `Vec<f32>` channels (length `period`, all `0.0`) before storing into
  `input_bufs` and before computing `meters` — so the entry's own meter broadcast and `input_bufs`
  are always exactly `entry.channels` (placeholder-size) long, with genuinely silent trailing
  channels shown as `-Infinity`/`0.0` explicitly rather than simply absent. `patch.rs`'s own
  resolution doesn't strictly need this (already safe either way, see the "key implementation
  finding" above), but this makes broadcasts/UI consistently show the full placeholder width.

## Phase E — Output grid: same standard-size validation

- `OutputGridEntryConfig.channels` (already resolved via `layout.rs`'s `resolve_channels`, same as
  input grid) gets the same `is_standard_stream_size` startup check as Phase A.
- No equivalent of Phase B/C/D exists for output — an output-grid entry's own channel count is fixed
  for its lifetime and it already writes silence for anything unpatched (today's existing, already-
  correct behavior, confirmed by reading `main.rs`'s output-grid construction and `engine.rs`'s
  output-write step before writing this plan) — there's no "a sender subscribed to us with a
  different channel count" scenario on the Tx side the way there is for Rx. Applying the size
  *validation* symmetrically is the whole of what "also apply to output" means here.

## Phase F — NMOS capability advertisement (BCP-004-01 `constraint_sets`)

Real primary-spec research required before writing this phase's own code — fetch and read the
actual AMWA BCP-004-01 spec text (or a verified secondary source, same rigor as every other spec
claim this session has made) for `constraint_sets`' real JSON shape and whatever parameter name
constrains channel count, then add it to `receiver_json` (Rx, `nmos/resources.rs`) and `sender_json`/
`source_json` (Tx) for a grid entry whose `channels` is a standard size. Not guessed from memory.

## Verification plan

1. `cargo test --release` after each phase — must stay at 115/115 plus new tests (standard-size
   validation accept/reject, `FlowReader::open`'s new accepts-up-to-N boundary behavior, the padded-
   meter behavior).
2. `cargo build --release`, restart affected running instances.
3. Live: re-run tonight's exact failing case — PATCH `mxl-test-app-adm-demo.conf`'s Receiver
   (already migrated to a standard placeholder size ≥ 8, per Phase A) to an 8ch sender (`mxl-signal-
   gen`'s "Test Tones", or `dynamic-rx`'s "Monitor Out") — confirm it now succeeds (no more `500`),
   confirm the meter broadcast shows real data, confirm subscribing something *larger* than the
   placeholder now fails with a real `400` and a clear message instead of `500`.
4. Confirm the migrated reference configs (`16x16`, `adm-demo`, `dynamic-rx`) all still start
   cleanly with the new startup validation in place.

## Explicitly out of scope (per the design doc's own decisions, restated for this plan)

- Real IS-08 — routing stays the internal `patch.rs` crosspoint bay.
- Ephemeral Stream Rx — confirmed persistent.
- Live re-subscription to a *different placeholder size* — the placeholder itself is fixed once
  configured (only the real subscribed sender underneath it can vary, up to that fixed placeholder).
