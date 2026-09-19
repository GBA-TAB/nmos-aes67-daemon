# Autonomous testing for random mixer setups — design proposal

Requested at the end of a long live-debugging session: "think about an autonomous testing process
for random setups." No code changes accompany this document — it's a proposal, prioritized and
scoped, grounded in the real bugs this session actually found by hand, so the plan targets the class
of defect this app has actually produced rather than a generic testing checklist.

## Why this is worth doing: what manual testing tonight actually caught, and how

Every real bug found this session was found by the same loop: form a hypothesis, write a one-off
Python/Playwright script, run it against a *live* instance, read the result, iterate. That loop
worked, but every single one of these would also have been caught automatically by a layer proposed
below, usually faster and without a human in it:

- **The `output_ids` panic landmine** and **LFE dropped on Rigid rotation** — pure logic bugs in
  `mixer.rs`, already fixed and covered by unit tests. §1 (property-based testing) generalizes this
  from "the specific cases someone thought to write" to "every case in the input space," which is
  exactly the category these two bugs came from (an edge case nobody happened to hand-write).
- **The AdmPanner duplicate-source-button bug** — turned out to be *real backend state*
  (contaminated leftover dynamically-created track/bus/master ids from earlier ad-hoc testing,
  surviving in the state file and winning back their identity on restart). No unit test would catch
  this — it's an emergent property of *sequences of operations over time* (CREATE, DELETE, restart,
  repeat) that only shows up after enough of them. §2 (topology-sequence fuzzing) targets exactly
  this.
- **The AdmPanner "no ring, no drag" bug** — a render-storm in the *dashboard*
  (`ParameterStateService` firing 1000+ events/sec into unthrottled component re-renders), invisible
  to any backend test since the backend's behavior was correct throughout. §3 (dashboard interaction
  fuzzing) is the only layer that would have caught this.
- **The elevation-fader/width-bar dead-zone bug** — a plain typo (`.elev-fill` vs
  `.ringpan-elev-fill`) that any test actually *reading rendered pixel state* back would have caught
  immediately, and any test that only checks "did the PUT arrive" would have missed completely (the
  PUT/state layer was correct — only the visual reflection was broken). Also §3.

So the plan below is really three independent layers, not one pipeline — they catch disjoint bug
classes, and only the first is fast enough to run on every commit.

## Layer 1 — Property-based testing of the pure mixer math (fast, in-process, run on every commit)

`mixer.rs` is already unusually well-suited to this: `mix_into_scaled_with_*`, `vbap_bed_gains`,
`apply_lfe_trim`, the downmix matrices — all pure functions over plain `Vec<Vec<f32>>` buffers, no
I/O, no shared state, already the thing the existing 151 unit tests exercise with hand-picked cases.
Property-based testing (the `proptest` crate) generates *many* random cases against a small set of
invariants instead of a few hand-picked ones, and shrinks any failure to a minimal repro
automatically — genuinely low-cost to add given the functions are already structured this way.

Concrete invariants worth encoding (none of these are new math — they're already true by
construction, this only makes them checked at every commit against inputs nobody thought to write
by hand):

- **No NaN/inf ever, for any finite input.** Every panning/downmix function should be checked across
  random `(layout_pair, azimuth, elevation)` combinations.
- **Conservation for a constant-power pan law**: `vbap_bed_gains`'s returned gains should satisfy
  `sum(g_i^2) ≈ 1.0` (within float tolerance) for any azimuth on any supported layout — this is the
  exact invariant a bug in the ring-pair bracketing math (wrong neighbor selected, wrong gain
  formula) would violate, and it generalizes the existing single-point assertions (`vbap_5_1_dead_
  center_pans_entirely_to_c`, `vbap_5_1_exact_speaker_azimuth_pans_entirely_to_that_speaker`, etc.)
  to the whole azimuth range instead of the handful of angles someone wrote a test for.
- **LFE never appears on a non-Lfe channel, for any pair of layouts.** This is *literally* the
  invariant the "LFE dropped on Rigid rotation" bug violated (though in the other direction — it
  silently zeroed Lfe instead of leaking it elsewhere; the general property to check is "Lfe energy
  in equals Lfe energy out, whenever the destination has an Lfe role, for every rotation angle," not
  just the one rotation angle a hand-written test happens to pick).
- **`apply_lfe_trim` never touches a non-Lfe channel, for any layout/trim-dB combination** — direct
  generalization of `apply_lfe_trim_scales_only_the_lfe_channel`.
- **Every downmix matrix row sums to a sane range** (no output channel can exceed roughly the sum of
  its largest few contributing inputs at unity gain) — a coarse anti-clipping-by-construction check
  across all seven `DownmixTable` pairs, random source sample values.

Cost: low. This runs in the same `cargo test` invocation already used all session (a few seconds),
needs one new dev-dependency (`proptest`), and every invariant above is check-only against functions
that already exist — no engine/topology/WS layer involved.

## Layer 2 — Randomized topology + WS-protocol fuzzing (an isolated throwaway instance, run in CI or nightly)

Tonight's own scratchpad workflow (`create.py`/`listen_full.py`/a throwaway config on a spare
`ws_port`, e.g. the `grid-channel-test.conf` instance used to prove the new `grid_channel`
auto-input addressing) is already a complete, working manual version of this layer — it just needs
to be driven by a generator instead of by hand, and to assert invariants instead of a human reading
JSON.

Shape:

1. **Random config generator**: produces a `Config` (this session's own `TrackConfig`/`BusConfig`/
   `MasterTrackConfig`/grid-entry shapes) with a random number of tracks/buses/masters (small — 1-12
   is enough to exercise real interaction, no need for hundreds), random channel counts/layouts
   drawn from the supported set, random `sends`/`auto_input`/`adm_objects`/`chain` contents, and
   random input/output grid entries. Every generated config must itself be valid against this app's
   own startup validation (channel-count agreement, standard stream sizes, `adm_objects` length) —
   the generator encodes the *same* constraints `layout::resolve_channels`/`topology::build_track`
   already enforce, so a generated config is guaranteed to either start cleanly or reveal a genuine
   validation bug if it doesn't.
2. **Boot a throwaway instance** against it, on a scratch `ws_port`, exactly like tonight's manual
   proof — isolated, no interference with anything else.
3. **Randomized operation sequence** against the live WS protocol: PUT random valid values at random
   valid paths (gain/fader/lfe-trim/mute/solo/sends/chain params, `input-patch`/`bus-in`/`master-in`
   patches, `adm-objects`), CREATE/DELETE dynamic tracks/buses/masters, restart the process mid-
   sequence at random points (this is exactly the operation-sequence-over-time category the AdmPanner
   phantom-track bug came from). A fixed random seed makes every run reproducible and any failure
   replayable.
4. **Invariants checked after every operation** (all already observable over the existing WS
   protocol — no new instrumentation needed):
   - `channel-list`/`sum-list`/`master-list` counts match what CREATE/DELETE actually did.
   - No `panicked at` in the process's stderr, ever (this is the exact category the `output_ids`
     panic landmine was in — a genuinely crashing bug, not just a wrong-value one).
   - A PUT's own broadcast echo (`ws.rs`'s trailing `publish` call) reflects exactly the value that
     was PUT, for every param.
   - **Grid-numbering invariant** (new tonight, worth checking automatically given how easy the
     unification bug was to introduce): every input-grid entry's own `channel_labels` are
     contiguous, 1-based, gapless, and start at "Grid In 01" for the very first entry, no matter how
     many entries or what order they were declared/discovered in.
   - **State round-trip invariant**: capture the full live state, restart the process, capture again
     — every *config-authored* resource's live-value fields (gain/fader/lfe-trim/mute/solo/sends/
     chain/adm_objects) must be identical before and after, and every *dynamically-created* resource
     must still exist with the same topology (this is `persistence.rs`'s own contract, already unit
     tested at the function level — this layer checks it survives a real process restart, not just
     a `capture`/`apply_snapshot` call in-process).
5. **Failure reporting**: dump the generated config, the exact operation sequence (with its seed),
   and the process's full stderr — enough to replay the exact failure deterministically and turn it
   into a proper regression test once diagnosed, the same way the LFE/output_ids fixes tonight each
   became a hand-written unit test after being found live.

Cost: moderate. Needs a small standalone Rust or Python driver (Python is lower-friction given
tonight's own scratchpad tooling is already Python/`websockets`), and real wall-clock time per run
(each generated instance needs a few seconds to boot/settle) — not something to run on every commit,
but a reasonable nightly/pre-release gate. Bounding the config generator to a modest size (1-12
tracks) keeps each run fast; running *many* small random configs beats one huge one for the same
wall-clock budget, since it explores more distinct topologies.

## Layer 3 — Dashboard interaction fuzzing (Playwright against a live randomized backend)

This is the layer tonight's two dashboard-side bugs (the render storm, the dead-zone fader) actually
needed — neither was reachable from the backend alone. Shape, directly modeled on the ad-hoc scripts
already written this session (`adm_verify.mjs`, `measure_signalr_rate.mjs`, the pixel-sampling
`getImageData` checks):

1. Point the dashboard at a Layer-2-generated live instance (reuse the same random-topology
   generator — one throwaway backend, one throwaway dashboard pointed at it).
2. **Navigate every route** (`/`, `/mixer/{id}`, `/mixer/{id}/topology`, `/mixer/{id}/patchbay`,
   `/mixer/{id}/downmix`, `/adm-panner`, `/compact-dashboard`) and assert zero browser console
   errors and zero uncaught `pageerror`s on load — this alone would have caught the
   `.elev-fill`/`.ringpan-elev-fill` mismatch class of bug immediately if the check also read back
   the actual rendered canvas state (not just "no JS exception was thrown" — that bug threw no
   exception at all, it just silently queried a selector that matched nothing).
3. **Randomized click/drag interaction**: for each page, enumerate its interactive elements
   (buttons, sliders, canvases with drag handlers, `@onclick` targets) and drive a bounded random
   sequence of clicks/drags against them — the same shape as tonight's manual "click a source
   button, drag the ring, read back the value" checks, just driven by a generator instead of a
   fixed script.
4. **Responsiveness invariant, not just correctness** — this is the one genuinely new idea tonight's
   own bug hunt surfaced: measure *wall-clock time from action to visible/observable effect*
   (canvas non-transparent-pixel-count going non-zero, an input's displayed value changing), not
   just "did it eventually happen." The render-storm bug produced a *technically correct*
   eventual state — the ring did paint, drag did work — just five-plus seconds late, which is
   exactly the kind of regression a pure correctness check (no timing budget) would never flag. A
   simple time budget per interaction (e.g. "a click must produce a visible effect within 500ms
   under a live but otherwise idle backend") turns "eventually correct" bugs like this into a hard
   failure instead of something that only surfaces when a human happens to notice the app *feels*
   slow.
5. **SignalR/WS traffic-volume invariant**: reuse tonight's own `measure_signalr_rate.mjs`
   methodology as a standing check — frames/sec on a freshly-loaded, otherwise-idle page should stay
   within some multiple of the backend's actual notification rate (now throttled to ~20/sec at the
   source, so a healthy page should track that, not blow past it). This is the automated version of
   the exact debug counter (`[PSS-DEBUG] updates/sec=... fires/sec=...`) used tonight to confirm the
   throttle fix actually worked — worth keeping as a real regression check, not a one-off debug
   instrument thrown away after use.

Cost: highest of the three layers (a real browser, a real two-process live stack, wall-clock-
sensitive assertions that need care to keep from being flaky) — proposed as a smaller, curated set
of scenarios (the known page list above, a fixed but periodically-refreshed set of interaction
scripts) rather than a fully random one, at least to start; the *topology* underneath it can still
come from Layer 2's random generator even if the *interaction* script itself is hand-curated.

## What to actually build first

In order of cost-to-value, given tonight's own bug mix (2 pure-logic bugs, 1 operation-sequence bug,
2 dashboard-interop bugs — genuinely spread across all three layers, so no single layer would have
caught everything):

1. **Layer 1** (property-based tests on `mixer.rs`) — smallest cost, runs in the existing `cargo
   test` loop, and the functions are already shaped for it. Start here.
2. **Layer 3's responsiveness/traffic-volume checks specifically** (items 4-5, not the full random-
   interaction fuzzer yet) — these two would have caught tonight's most user-visible bug (the render
   storm) and are cheap to write as standing checks against the *existing* small set of
   ad-hoc scripts already sitting in the scratchpad from tonight, just promoted into the repo instead
   of thrown away.
3. **Layer 2** (topology/WS fuzzing with restart-cycling) — moderate cost, catches the state/
   persistence class of bug that's hardest to find by hand (it depends on *sequences* of operations
   over real wall-clock time, exactly what made the AdmPanner phantom-track bug so confusing to
   diagnose live tonight).
4. **Layer 3's full random-interaction fuzzer** — highest cost, most valuable once 1-3 exist and the
   obvious bugs are already screened out; diminishing-but-real returns after that (real UI fuzzing
   tends to find the "nobody clicked this specific sequence before" class of bug, which is genuinely
   rare but has real user impact when it happens, as tonight's own reports show).

## Open questions, not decided here

- **Where Layer 2/3 actually run** (nightly CI job vs. a manually-triggered pre-release gate) is an
  infrastructure decision outside this app's own repo, not addressed here.
- **How much of Layer 3 needs a *real* signal path** (an actual MXL flow with real audio, not just
  WS-state/meter assertions) — everything proposed above only needs the WS/DOM layer, matching how
  this whole session's own live verification worked; genuinely testing *audio correctness* (not just
  "the UI shows the number that was PUT") would need a signal generator + a way to read samples back
  out of an output-grid flow, which is a materially bigger lift and wasn't attempted here.
- **Flakiness budget for Layer 3's timing checks** — a 500ms responsiveness budget is a starting
  guess, not measured against real variance across machines/CI runners; would need tuning once it's
  actually running somewhere other than this one dev machine.
