# Design capture: standard-sized placeholder input-grid Receivers ("Stream Rx")

**Status: design reflection only, not implemented.** The user explicitly asked for this to be
captured as a plan/design doc for later, not built now. Nothing in this file has landed in code.
**All open questions below are now resolved** (2026-09-15) — see "Confirmed decisions" — this doc
is ready to become a real phased implementation plan whenever that's actually requested.

## Confirmed decisions (2026-09-15, don't re-ask)

1. **Placeholder sizing scheme**: config-declared per entry, chosen from the standard ST 2110-30-
   aligned sizes (1/2/8/16/64) — not a single universal default, not WS-only. A `TrackSource`-style
   config field naming the operator's chosen standard size per input-grid entry.
2. **Unused placeholder channels**: silent, exactly like an unpatched entry already is today — no
   new silence-handling concept needed, reuses the existing "always produce, never stall" precedent
   throughout `patch.rs`/`engine.rs`.
3. **Live re-subscription**: allowed. The same persistent Stream Rx can be re-activated later
   against a different real sender with a different (still ≤ placeholder) channel count, without a
   DELETE+CREATE cycle — matches real IS-05 behavior (a receiver can be re-PATCHed any time).
   `patch.rs`'s own crosspoint validation needs to tolerate an input-grid entry's *real* subscribed
   channel count changing under an already-existing `input-patch`, not just at construction time.
4. **Coexistence with today's static exact-sized entries**: **replaced entirely**, not additive —
   a deliberate, confirmed exception to this session's usual backward-compatible-by-default
   convention. Every input-grid entry becomes a standard-sized placeholder going forward; this is a
   real breaking change to existing config files' `input_grid` entries (today's bare `channels: N`
   field on an input-grid entry no longer describes an exact/fixed count the same way — existing
   configs need their `input_grid` entries migrated to declare a standard size instead).
5. **NMOS capability advertisement**: the placeholder's own standard size gets advertised as a real
   BCP-004-01 `constraint_sets` capability (not left as today's minimal `caps: {
   media_types: [...] }`) — real primary-spec research into BCP-004-01's exact `constraint_sets`
   shape for a channel-count-style capability is required at implementation time, not assumed or
   guessed from this design pass.

## The problem this addresses (confirmed live, not hypothetical)

Tonight, PATCHing an input-grid Receiver on `mxl-test-app-adm-demo.conf` (whose `in-mono`/
`in-stereo` entries are statically sized to exactly 1ch/2ch, per `Config.input_grid`) to a real 8ch
sender on the network (`mxl-signal-gen`'s "Test Tones" or `mxl-test-app-dynamic-rx.conf`'s "Monitor
Out") failed with `500`:
```
receiver activation failed to open flow error=MXL flow channel_count (8) does not match expected (2)
```
Root cause, confirmed by reading the code (`src/flow.rs:134-148`, `src/nmos/server.rs:372-385`):
`FlowReader::open` hard-requires an **exact** channel-count match between the input-grid entry's
own fixed, config-authored `channels` and whatever the real subscribed flow actually is — any
mismatch is a hard failure (currently surfaced as an unconditional `500`, itself arguably wrong per
IS-05 — see the note at the end — but that's a symptom, not the real problem).

The user's point: this is backwards. A real ST 2110/AES67 world doesn't hand you exactly the
channel count your receiver happened to be pre-configured for — it hands you whatever the sender
actually is, clustered around a handful of standard sizes. The receiver should be flexible enough
to accept that, not reject it outright.

## Real-world grounding: SMPTE ST 2110-30 conformance levels (verified via WebSearch, not guessed)

ST 2110-30 (which directly references AES67) defines six conformance levels — A, B, C, AX, BX, CX.
**Level A** (the mandatory baseline every conformant device supports) is 48kHz, 16/24-bit, **1 to 8
channels**, 1ms packet time. **Level C** extends this to **1 to 64 channels** at a 125µs packet
time (a device claiming Level C must also support Level A as a fallback). Level B sits between the
two (still channel-count-flexible, shorter packet-time variants). Exact channel-count boundaries
for B specifically vary a bit by source and weren't pinned down to the same confidence as A (≤8)
and C (≤64) — worth confirming against the primary ST 2110-30 text directly before this is actually
implemented, not re-guessed from search summaries at that point either.

This matches the user's own observation directly: real-world 2110 audio streams cluster around a
small set of standard sizes — **1, 2, 8, 16, 64** — not arbitrary N. A receiver designed around
"exactly the N channels I was configured for" fights that reality; a receiver designed around "a
standard-sized placeholder that can accept anything up to its own size" matches it.

## The proposed model

- An input-grid entry ("Stream Rx") is created **ahead of time** (config-authored, same as today —
  confirmed **persistent**, not ephemeral, per the user's own answer: "Persistent once created"),
  but sized to one of the **standard placeholder channel counts** — confirmed config-declared per
  entry, chosen from 1/2/8/16/64 (decision #1) — rather than an exact count matched to one specific
  expected sender.
- That placeholder Receiver is real, NMOS-discoverable, and exists specifically **so an NMOS
  controller has something to see and trigger an IS-05 subscription against** — the user's own
  framing: "it exposes the rxs to nmos for triggering the process." This is the actual point of a
  Stream Rx existing at all, independent of what ends up flowing through it.
- It can then accept a subscription from **any real sender whose own channel count fits within the
  placeholder's size** (an 8ch placeholder accepting a 2ch sender, using only 2 of its 8 slots, the
  rest silent — matching this codebase's own existing "always produce, never stall, silence for
  anything unpatched" precedent already used everywhere else in `patch.rs`/`engine.rs`) — "populated
  on the fly."
- **Stream Rx and Track stay fully decoupled**, exactly matching this codebase's existing
  architecture (`PICKOFFS.md`'s own intro: the input-grid pickoff-point patch bay is deliberately
  NOT fused with the internal mixer's track/bus/master objects). Routing a Stream Rx's received
  channels to a Track happens through the **existing internal patch bay** (`patch.rs`'s
  `input-patch` crosspoint mechanism, already built and working) — confirmed **not** a request to
  add real IS-08: the user's own follow-up, when asked directly, described the placeholder-sizing
  idea itself without asking for real IS-08 Map/ActiveMap resources to be added, and `PICKOFFS.md`'s
  existing, deliberate rejection of IS-08 (documented there already, from before this session) isn't
  being revisited here. "Either IS-08 or inside the app" in the user's first answer was naming the
  general concept of "channel mapping happens as a separate step," not asking for real IS-08
  specifically — the internal patch bay already *is* that separate step.

## Former open questions — now resolved, see "Confirmed decisions" above

The five items below were the open design questions this section originally raised; all five are
now answered (2026-09-15) — kept here only as the fuller reasoning/implementation-pointer behind
each of the "Confirmed decisions" bullets above, not as still-open items.

1. **Placeholder sizing scheme** → config-declared per entry among 1/2/8/16/64 (decision #1 above).
2. **"Accepts up to N, not exactly N"** → confirmed (decision #2: silent, like unpatched today). A
   real, non-trivial change to `FlowReader::open` (`src/flow.rs:146-148`)'s current hard
   `channels != expected_channels` check — becomes something like `channels > placeholder_size` as
   the only failure condition, with the receiver's own internal per-period read/patch-bay logic
   needing to handle "this entry's real subscribed channel count is smaller than its own declared
   placeholder size" (extra placeholder channels read as silence, not an error) — touches
   `patch.rs`'s crosspoint range-checking too (`InputGridEntry`'s channel count is currently assumed
   fixed and exactly right for validating a `SourceRef::Input` crosspoint's channel index).
3. **NMOS capability advertisement** → confirmed real BCP-004-01 `constraint_sets` (decision #5
   above) — a real capability range so a controller can see "this accepts up to Nch" before
   attempting a subscription; today's codebase has no channel-count capability anywhere
   (`caps: { media_types: [...] }` only) — real primary-spec research into `constraint_sets`'s exact
   shape for this is genuinely new work at implementation time, not assumed here.
4. **Re-subscription to a different real channel count** → confirmed allowed live (decision #3
   above), matching real IS-05 behavior. Still has real design implications for `patch.rs` beyond
   just `flow.rs`'s own open-time check: what happens to an existing `input-patch` crosspoint into a
   Track sized for the *previous* subscription's real channel count when the same Stream Rx gets
   re-activated at a different real count needs its own resolution at implementation time.
5. **Relationship to today's static, exact-sized input-grid entries** → confirmed **replaced
   entirely** (decision #4 above), not additive — a deliberate, explicit exception to this session's
   otherwise-consistent backward-compatible-by-default convention. Existing configs' `input_grid`
   entries will need migrating.

The original narrower `500` vs `400` status-code question this design reflection grew out of is now
largely moot — once a mismatch is a real, expected "channel count doesn't fit any available
placeholder" case rather than "this entry only ever accepts exactly N," the failure condition
itself is different — but the underlying correctness point (a client-caused validation failure
shouldn't be a `500`) still applies to whatever that new failure condition ends up being (e.g.
subscribing something bigger than even the largest available placeholder).

## Explicitly not in scope (confirmed)

- Real IS-08 channel mapping — `PICKOFFS.md`'s existing rejection stands; routing stays the internal
  `patch.rs` crosspoint bay.
- Ephemeral/auto-deleted Stream Rx entries — confirmed persistent once created.
- Implementing any of this now — this file is the design capture the user asked for, all five open
  questions are now resolved (see "Confirmed decisions"), but nothing has been built; next step (if
  and when they want it built) is turning this into a real, phased implementation plan the same way
  `SESSION-2026-09-14-ADM-LAYOUT-HANDOFF.md` and `SESSION-2026-09-14-METERS-VBAP-PLAN.md` did for
  their own work — including sizing the real migration cost to existing configs (`decision #4`:
  this replaces, not extends, today's static exact-sized `input_grid` model).
