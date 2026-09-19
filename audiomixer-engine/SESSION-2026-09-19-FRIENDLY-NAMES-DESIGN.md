# Friendly names: auto-derived from what's patched, live-updating, both directions

Design-only, no code changes in this pass — written for a fresh session, spanning this repo
(`audiomixer-engine`, the wire protocol and the actual name derivation) and the sibling
`audiomixer-controller` repo (the UI: a naming-mode toggle, and the same toggle reused in the XY
patch-grid popup). Requested alongside a grid-naming question this session already answered in
full (see that answer's own summary, not repeated here): grid size is config-time-only, no runtime
resize path exists, deliberately (`SESSION-2026-09-17-GRID-CRUD-AVAILABILITY-DESIGN.md`).

The user's own framing, verbatim, across two messages: *"[for grid entries and the XY patcher]
this should be added, and updated if patch changes, and sent to ws clients on init or change,
names are based on MXL flow name and audio id in the flow. Audio track names follow the logic,
with an option to name, that is similar to what is done in configuration mode, it would be good to
update it live if a track name is generated from its patched resource. for masters, it is a
friendly name, that could if possible be reported to MXL output/Tx on which the master is
connected to via output grid, as the logic is mostly ... flow name is device-content for its name
[with] an override for the case multiple masters are packed into an mxl flow."*

## Foundational gap, found while scoping this: two things don't exist yet that everything below needs

**1. `subscribed_sender_id` is tracked but never sent to a WS client.** `patch.rs`'s
`InputGridEntry.subscribed_sender_id` (`Mutex<Option<String>>`, set by `nmos/server.rs::
receiver_patch` on IS-05 activation) exists purely as internal bookkeeping today — grepped every
reference in `ws.rs` and found none; `InputGrid::list_json` (`patch.rs:250-256`) emits only `id`/
`label`/`channels`/`channel_labels`. A client (including `audiomixer-controller`) cannot see what's
patched into a grid slot at all right now, only the slot's own static identity. This has to be
exposed before "auto-name from what's patched" can mean anything.

**2. `Track.label`/`Bus.label`/`MasterTrack.label` are plain `String`, not `Mutex<String>`.**
(`mixer.rs:140,271,331`.) Unlike `InputGridEntry.label`/`OutputGridEntry.label` (already
`Mutex<String>`, already live-renamable via `PUT .../label` — `ws.rs:173-180,188-199`), a track/
bus/master's name is fixed at construction and has no rename path *at all* today, manual or
auto. This is a real, separate foundational change (make the field a `Mutex`, add a `PUT
.../label` arm mirroring the grid-entry ones already there) that both the manual-override half and
the auto-derived half of this feature need underneath them.

## Naming source: "MXL flow name and audio id in the flow"

Resolving what's patched into an input-grid entry to a friendly name means, concretely:

1. Read `subscribed_sender_id` (once exposed, per the gap above).
2. Resolve it to a `flow_id` — `registration.rs::resolve_sender_flow_id` (`:131-148`) already does
   exactly this query (`GET /x-nmos/query/v1.3/senders/{sender_id}` → `.flow_id`), reused verbatim,
   not rebuilt.
3. **New**: a second registry query, `GET /x-nmos/query/v1.3/flows/{flow_id}`, for that Flow
   resource's own `label` — this is "the MXL flow name." Confirmed live tonight against a real
   instance: `mxl-signal-gen`'s own flows report real, meaningful labels this way (e.g. `"MXL Audio
   Flow, 8 ch, 48 kHz"` for its Test Tones flow) — nothing new needs to exist on the *writer* side
   for this half, it's a read the registry already has an answer for.
4. **"Audio id in the flow"** — the specific channel within that flow this particular grid
   entry/track is actually listening to. Two real sources, in priority order, *both already
   possible without inventing anything*:
   - If the Flow resource's own `channels[]` array (real IS-04 schema field) carries real
     per-channel labels — which *this app's own* output flows already do when a layout is set,
     `nmos/resources.rs::channels_json`, emitting real speaker-role labels instead of generic
     "Channel N" — use that channel's own label when the upstream flow happens to provide one.
   - Otherwise, fall back to the bare channel index (matching this app's own "Grid In 09" /
     `channel_labels` numbering convention for the *local* side of the naming, applied to remote
     flows too: `"<flow label> ch3"` or similar).
   - **Explicitly out of scope**: resolving an ADM object's own name (e.g. "Dialogue") for a
     specific audio object within a multi-object flow. That needs the sending app's ADM object
     metadata to actually travel over the wire (ST 2110-41 or equivalent), which this codebase's
     own earlier ADM design work deliberately scoped out (`SESSION-2026-09-14-ADM-LAYOUT-HANDOFF.md`
     and later: "live wire transport of ADM metadata is explicitly out of scope"). If that ever
     lands, it's a *third*, richer naming source slotting in above the two here — not a blocker for
     this feature as scoped now.

## Naming mode: Auto / Manual, mirroring `SendPanMode` exactly

The user's own "similar to what is done in configuration mode" reference, and "update it live ...
if generated from its patched resource" (implying it *stops* updating once a manual name is set)
both point at the same existing pattern this codebase already has for exactly this shape of
problem: `SendPanMode` (`mixer.rs:37`, `Auto`/`Adm`/`Route`) — an explicit per-object mode selector,
not an implicit "is the value still the default" check. Recommend the same shape here: each
nameable object (input-grid entry, track, bus, master) gets a `name_mode: Mutex<NameMode>` where
`NameMode` is `Auto | Manual`, alongside the existing `label: Mutex<String>`:

- **`Auto`** (the default for anything with a live patch, matching `SendPanMode::Auto`'s own
  "sensible default, no explicit user action needed" role): `label` is *computed*, re-derived and
  re-published every time the underlying patch changes (see "when this fires" below) — a client
  never PUTs `label` directly while in this mode; it's read-only from the wire's point of view,
  same as `chain`/`peakmeter` already are.
- **`Manual`**: `label` is exactly what it is today for grid entries — a plain client-settable
  string via `PUT .../label`, never touched by patch changes. Setting `label` via PUT while in
  `Auto` mode is exactly the "stops auto-following" transition the user asked for: the PUT handler
  should flip `name_mode` to `Manual` *as part of* handling that PUT, not require a separate
  mode-switch call — one user action ("I typed a name"), one state transition, matching how a
  person actually thinks about it rather than a two-step "first switch to manual, then type."
- A new `PUT .../name-mode` (`"auto"|"manual"`) lets a client switch *back* to `Auto` explicitly
  (re-deriving from the current patch immediately on the switch) — the one transition a plain
  label-PUT can't express on its own.

**When Auto re-derivation fires:** on IS-05 receiver activation/deactivation (`receiver_patch`,
already the choke point for "this entry's subscription changed" — no new trigger needed there),
and, for a track/master's own name (derived from *its* `input-patch`, not a grid entry's), on any
`PUT .../input-patch` that actually changes the resolved source (`apply_track_param`/
`apply_master_param`'s existing `input-patch` arms). Each of those call sites already publishes
something on change; add the re-derived-name publish alongside, not as a new poll/timer.

**Wire shape**: sent as an ordinary field on the resource's own existing JSON shape (`label` is
already in `tracks_list_json`/`buses_list_json`/`masters_list_json`/`InputGrid::list_json`) plus a
new sibling `name_mode` field next to it, and both already ride `init` for free once they're part
of those existing `*_json` builders — no separate mechanism needed for "sent on init or change",
that's just the existing list-republish/PUT-echo machinery this app already has for every other
field, reused.

## Masters: the reverse direction — pushing the name *out* to the flow it feeds

This is the genuinely new half, not just "the same thing read backwards." A master's own friendly
name, instead of (or in addition to) being *derived from* an input, gets *pushed into* the label of
the MXL flow its output-grid entry writes — "flow name is device-content" per the user's own
phrase, matching the convention this app's own `nmos/resources.rs` output-flow labels and
`mxl-signal-gen`'s own flow labels (`"MXL Audio Flow, 8 ch, 48 kHz"`-style: what wrote it +
what it carries) already follow loosely.

**The 1:1 case (simple, and the common one):** exactly one master's own `master-out` feeds a given
output-grid entry across its whole channel range — resolvable today by inspecting that entry's own
per-channel `patch()` array (`patch.rs`) and confirming every channel resolves to the same
`SourceRef::MasterOut { master_id, .. }`. In this case, the master's friendly name can simply
*become* that output-grid entry's own `label` (already `Mutex<String>`, already live-renamable,
already flows into that entry's own registered NMOS Flow resource's `label` via the existing
registration path — no new plumbing needed for the "push it into the real Flow resource" half,
only for the "trigger it from the master's own name changing" half).

**The N:1 case the user explicitly flagged** — multiple masters' outputs packed into different
channel ranges of one shared output-grid entry/flow (already representable today: an
`OutputGridEntry`'s own `channels` count is fully independent of any single master's, per-channel
crosspoint routing already lets different channels of one entry resolve to different masters'
`master-out`) — has no single obviously-correct name. Recommend: detect this case (the per-channel
resolution above finds more than one distinct `master_id` feeding the entry) and simply **don't
auto-push** a name in this case — leave the output-grid entry's own `label` exactly as manually set
today, and surface *why* auto-naming isn't happening for it (a readonly informational field or a
log line, not silent) rather than guessing whose name should win or concatenating multiple names
into something nobody asked for. This is a real, deliberate scope boundary, not a gap to fill
later — the user's own "could be different, too, with an override" already anticipates this exact
outcome.

## `audiomixer-controller` side (separate repo, same session's worth of work)

- A per-object naming-mode toggle (Auto/Manual) wherever a track/bus/master/grid-entry's name is
  shown or edited — visually, likely the same "pill" language `Configure.razor`'s own per-mixer-row
  badges already use (`"● Running"`/`"Scheduled (new, unsaved)"` style, `Components/Pages/
  Configure.razor`), reused for "Auto"/"Manual" rather than inventing new visual language.
- The XY patch-grid popup (`Components/Audio/PatchGrid.razor`, `PatchControl.razor`) gets the same
  toggle for its own row/column labels — this is purely a *display* concern once the underlying
  `label`/`name_mode` fields exist on the wire; the patcher doesn't need its own separate naming
  logic, just to read the same fields everything else does.
- **Open decision, deferred to whoever builds the UI half**: does the toggle apply per-object (one
  flip per track) or is there also a page-level "show me grid IDs" / "show me patched names" master
  switch that overrides every individual object's own display preference at once? The user's
  original ask ("an interface button where you can flip the naming") reads more like the latter
  (one global display preference) than per-object — but per-object `name_mode` (Auto/Manual) is a
  *data* concept the backend needs regardless of which display toggle the UI ends up offering; a
  global "grid ID vs. friendly name" display switch in the controller is a thin, independent layer
  on top, not something the engine's own wire protocol needs to know about at all.

## Suggested build order

1. `Track`/`Bus`/`MasterTrack.label` → `Mutex<String>` + `PUT .../label` (the foundational gap) —
   small, mechanical, immediately useful even with zero auto-naming logic (this alone closes "I
   can't rename a track" as a standalone fix).
2. Expose `subscribed_sender_id` over the wire (the other foundational gap) — small, read-only,
   unlocks the controller being able to show *anything* about what's patched into a grid slot, even
   before auto-naming exists.
3. The flow-label + channel-id registry resolution (new function alongside
   `resolve_sender_flow_id`) — self-contained, testable against a live registry independent of the
   mode/PUT machinery around it.
4. `NameMode` (Auto/Manual) + the re-derivation triggers, for input-grid entries first (simplest:
   one trigger point, `receiver_patch`) — proves the pattern before extending it to tracks/masters
   (more trigger points: every `input-patch` PUT that actually changes the source).
5. Tracks/masters' own Auto naming, reusing steps 3-4's machinery against their own `input-patch`.
6. Masters → output-flow label push, 1:1 case only, with the N:1 detect-and-skip guard from day
   one (not a follow-up) — the "silently wrong" failure mode (auto-pushing a name in the N:1 case
   with no guard) is worse than not having the feature yet.
7. `audiomixer-controller` UI: the toggle(s), last — needs 1-4 (at minimum) actually on the wire to
   have anything real to display.
