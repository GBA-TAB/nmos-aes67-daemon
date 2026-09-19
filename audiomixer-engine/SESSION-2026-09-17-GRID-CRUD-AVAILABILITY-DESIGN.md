# Grid entry CRUD model, availability, and restart/reconnection

This documents the current, real behavior of `input_grid`/`output_grid` (patch.rs's pickoff-point
patch bay) against three questions raised in review: what "CRUD" actually means against a
fixed-size grid, what "availability" means for an entry, and what happens to a grid entry's
subscription across a mixer instance restart — including reconnecting to a *different* MXL node.
No code changes accompany this document; it's a record of current behavior plus explicitly-flagged
open decisions for future work, requested after "plan panpot design" landed
(SESSION-2026-09-16-PAN-OBJECT-MATRIX-DESIGN.md's own addendum) surfaced the same question for the
grid.

## 1. The scope boundary: grid *size* is fixed, deliberately

The number of `input_grid`/`output_grid` slots, and each slot's own channel count, is set once at
process startup from static config (`cfg.input_grid`/`cfg.output_grid`, `main.rs:206-265` for input,
`:272-330` for output) and never changes for the life of the process — this is the same convention
a physical patchbay follows: you don't add new XLR ports while the show is running, you change what
each existing port is plugged into. **Creating brand-new NMOS Node/Device/Source/Flow/Sender/
Receiver resources at runtime — i.e. growing or shrinking the grid itself — is out of scope and not
planned.** If more I/O capacity is needed, that's a config change (or, in the Kubernetes target
deployment, a different replica/config) and a restart, not a runtime CRUD operation. Section 5
below records exactly what would have to change to support this, as a boundary marker, not a TODO.

Everything below is about what's already dynamic — and what could reasonably become dynamic —
*within* that fixed set of slots.

## 2. What's already dynamic today ("CRUD against a fixed grid")

### Input grid: already fully CRUD-able on subscription, via two independent mechanisms

**IS-05 receiver activation** (`nmos/server.rs::receiver_patch`, `:366-401`) — any NMOS controller
(or a plain `curl`, as this session's own manual repatches have done repeatedly) can PATCH an
entry's Receiver's `staged`/`active` params to point it at any compatible Sender's flow, entirely
independent of the entry's own static identity (`id`/`label`/`channels`, which never change). This
*is* the "reference to subscription is dynamic" property — it already exists, is exercised in
production, and needs no new work. Deactivating (`master_enable: false` or no `sender_id`) clears
`reader`/`subscribed_sender_id` back to empty (`server.rs:375-378`).

**Registry auto-discovery** (`nmos/discovery.rs`) — a second, separate mechanism: every 5s
(`discovery.rs:30,49-56`) the app polls the NMOS Query API for other MXL-transport Senders and
**creates or removes whole `InputGridEntry` objects** for them (`apply_diff`, `discovery.rs:117-
158`) — not just filling a reader into an existing slot. This sounds like it contradicts §1, but it
doesn't: every discovered entry is namespaced under `"registry:<sender_id>"` (`discovery.rs:31`,
documented at `:4-8` as deliberately collision-proof against config-authored or IS-05-activated
entries) and gets its own freshly-minted stable Receiver id from the same `ids::
instance_input_receiver_id` helper the static path uses. So the *config-declared* grid is still
fixed size, but the *live* grid the app actually exposes can be larger, auto-growing/shrinking as
other MXL apps come and go on the network. This already is a dynamic grid, just driven by registry
presence rather than a CREATE command a user issues directly.

### Output grid: the "subscription" concept doesn't really apply the same way

An output-grid entry's Sender is **always on** — it writes a real MXL flow every period
unconditionally, silence when unpatched (`server.rs:312-313`: *"an output-grid entry's Sender is
always on … `master_enable` is accepted but has no effect"*). There's no equivalent of "activate
this entry to start receiving" the way an input Receiver has, because nothing here is a receiver —
it's a broadcaster. The `receiver_id` an output entry's `sender_patch` records (`server.rs:312-316`)
is purely informational bookkeeping about which external NMOS Receiver last told this app "I'm
listening to you" — it has zero effect on the app's own behavior, and *whether* an external
receiver actually subscribes is that receiver's own IS-05 decision, made against this app's Sender,
not something this app can or needs to drive from its own side.

What an output entry's "content" already *does* let you change live, right now, with zero gaps:
**which internal signal feeds it** — that's `patch.rs`'s own output-grid patch
(`SourceRef::TrackOut`/`BusOut`/`MasterOut` → this output entry), fully live-PUTtable today and the
exact thing `PatchBay.razor`'s "Grid Out" destination category already edits. So for output, "the
grid's own content is dynamic" is already true in the one sense that actually matters operationally
(what you hear out of that port).

### What's genuinely NOT dynamic on either side: the entry's own `label`

`InputGridEntry.label`/`OutputGridEntry.label` are plain `String` fields (`patch.rs:115-116`, not
even `Mutex`-wrapped) — resolved exactly once at construction, either from config
(`entry.label.clone()`) or auto-generated from the running channel-offset counter
(`format!("Grid In {:02}-{:02}", …)`, `main.rs:223-224`/`:304-305`) — and then frozen for the life
of the process. **This is the one real, currently-missing piece of "dynamic content" the earlier
review flagged** ("dynamic in the naming of its content"). Concretely: there is no PUT path anywhere
in `ws.rs` that touches a grid entry's label at all today.

**Recommendation, not yet built**: add a `label` PUT to the existing per-entry `input`/`output`
WS namespace (`ws.rs` already dispatches `"input"`/`"output"` kinds by entry id for the peakmeter/
patch params — a `"label"` param would slot in the same way), backed by wrapping `label` in a
`Mutex<String>` (matching every other live-mutable field on these structs) and re-publishing it on
the existing per-entry broadcast tick. Small, well-scoped, no NMOS-side changes needed since the
Node API's own JSON builders (`resources.rs`) already read the struct fresh per request — a relabel
would show up in `GET /receivers`/`GET /senders` on the very next poll with no extra plumbing.

## 3. Availability model

**Input**: already a real three-state model, just not surfaced as one unified enum anywhere:
- `reader: Mutex<Option<FlowReader>>` — has a live open flow reader or not.
- `subscribed_sender_id: Mutex<Option<String>>` — which sender it was last told to read, purely
  informational (`patch.rs:147-148`).
- `fault: Mutex<Option<String>>` — set by the engine's own input-read step on a read failure,
  cleared on the next successful read (`patch.rs:150-151`); folded into the IS-12/registration
  `active` computation as `reader.is_some() && fault.is_none()`.

Together these already answer "is this input available right now": no reader = never activated or
deactivated; reader present + no fault = live; reader present + fault set = subscribed but
currently failing to read. This is a solid existing model — nothing to add here, just worth writing
down as the answer to "what does availability mean."

**Output**: no availability state at all, by design — an output entry's `FlowWriter` is created
once at startup and never becomes unavailable short of a process crash; it always has *something*
to write (silence if unpatched). There is no "is this output available" question to answer beyond
"is the process up," which the existing `/healthz` endpoint already covers at the process level.

## 4. Restart / reconnection behavior — including to a *different* MXL node

Traced directly from this session's own repeated manual recovery after restarts (this is not
theoretical — the exact sequence below has been executed by hand more than once tonight):

- **IS-05 active/staged state is never persisted and never auto-restored.** `subscribed_sender_id`/
  `reader` reset to empty on every process start; every entry comes back "awaiting IS-05
  activation" (`main.rs`'s own startup log line). `state_path` persistence (`persistence.rs`)
  covers track/bus/master *values* and dynamically-created *topology* — it has no `"input_grid"`/
  `"output_grid"` section at all (confirmed by direct inspection of `capture()`/`apply_snapshot()`
  in `persistence.rs`), so a grid entry's subscription is entirely outside what gets saved.
- **Concretely**: after any restart, an external NMOS controller (or a human with `curl`, as this
  session's own recovery steps have done) must re-issue the IS-05 PATCH against every input entry
  that needs to be live again. There's no automatic retry-connect-to-last-known-sender.
- **Reconnecting to a *different* MXL node specifically is not a special case — it's the same
  mechanism.** The connection is identified purely by `sender_id`, a real UUID looked up in the
  registry at PATCH time (`server.rs:392-401`, `registration::resolve_sender_flow_id`) — there is no
  hostname/IP/"same instance" assumption baked into the reconnect path. If the upstream sender
  moved to a different physical node, or a different MXL node instance came up entirely, the PATCH
  is identical: supply that sender's own `sender_id`. The only thing that changes across "same node
  restarted" vs "a different node now" is *which* `sender_id` the controller chooses to PATCH with
  — not anything this app does differently.
- **Is the *lack* of auto-reconnect actually a gap?** Genuinely open, not decided here. Three real
  options, not mutually exclusive:
  1. **Persist and replay**: save `subscribed_sender_id` (and enough transport info to re-resolve
     it) in the state file, re-issue the equivalent of an IS-05 PATCH at startup against whatever
     the registry currently says that sender_id resolves to. Risk: if the sender changed shape
     (different channel count, gone entirely), a blind replay could reconnect to something wrong
     or fail loudly — needs the same "validate, don't guess" discipline `main.rs` already applies
     elsewhere.
  2. **Leave it to the orchestrating layer**: this is arguably the *standard* NMOS answer, not a
     gap specific to this app — IS-04/05 itself does not guarantee a Receiver's subscription
     survives that Receiver's own process restart; a real NMOS deployment already expects a
     controller (or BCP-002-01 style "Concepts: Natural Grouping"/reconnection logic) to notice a
     Receiver came back with `active.sender_id: null` and re-patch it. Doing nothing here is
     consistent with how the rest of the NMOS ecosystem already handles this.
  3. **Do nothing, keep current manual/external-driven behavior** — valid if the deployment's own
     orchestration (whatever redeploys/restarts this container in Kubernetes) is expected to own
     reconnection as part of its own health-check/readiness story.

  No recommendation is made here between these three — it depends on what's actually orchestrating
  restarts in the target deployment, which is a decision for whoever owns that layer, not something
  to guess at from inside this app.

## 5. Why dynamic Node/Flow/Source/Sender/Receiver *creation* stays out of scope

Recorded here as the concrete reason §1's boundary is a real engineering decision, not just
caution, based on gaps a prior investigation this session already found by tracing the code:

- `NmosState.output_ids: HashMap<String, OutputIds>` is built once at construction
  (`nmos/mod.rs:56-69`) and documented as read-only-after-startup; every lookup used to be a
  panicking `Index` (fixed this session to be fallible, but the map itself is still never *added
  to* after startup — a fallible lookup on a runtime-created entry would still just always miss).
- No registry **DELETE** exists anywhere (`register_resource` only ever POSTs,
  `registration.rs:12-24`) — even input-grid's own working discovery-driven entry removal
  (`discovery.rs:120`) leaves a stale Receiver in the registry until the Node's own heartbeat
  eventually lapses, rather than actively deleting it.
- Nothing notifies the registration task when the grid changes — `register_all` only re-runs on
  startup, a 404 heartbeat response, or a fault transition (`registration.rs:40,54-58,149-161`).
- An output entry's `FlowWriter::create(...)` currently hard-fails the whole process on error
  (`main.rs:291-320`, `?`-propagated) — fine for a startup-time pass where a bad config should stop
  the app, wrong for a hypothetical runtime CREATE where one bad request shouldn't take the process
  down.
- The `Grid In NN`/`Grid Out NN` auto-label numbering is a running counter local to `main.rs`'s own
  startup loop (`input_grid_channel_offset`/`output_grid_channel_offset`) — a runtime-created entry
  has no natural slot in that numbering scheme without picking a different convention.

None of these are unfixable — discovery.rs is a working reference implementation of most of the
*input*-side mechanics already — but together they're real, multi-part work, not a small addition,
and §1's position is that the value (a physical-patchbay-sized, config-defined I/O footprint) isn't
worth that cost for this app's actual deployment model, where capacity changes come from
redeploying with different config, not a live CRUD call.
