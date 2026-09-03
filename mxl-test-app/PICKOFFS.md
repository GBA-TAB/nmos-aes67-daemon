# Pickoff points, grid points, and the generated control surface

Reference for exactly what mxl-test-app generates in terms of signal flow (pickoff points) and
control (the `amixer` WebSocket protocol, `ws.rs`) — for a track (input strip), a bus (output
strip), and the pickoff-point patch bay's own grid objects. Written against real console reference
material (`~/DEV/yam bus.png`, a Yamaha "CH to MIX" send-chain diagram; `~/DEV/Vista grid.png` and
`~/DEV/Vista grid patch.png`, a Studer DSP-router/crosspoint reference) rather than invented from
scratch — see each section for which document informed which part.

Two vocabularies stay deliberately separate, per the plan at
`~/.claude/plans/snug-painting-elephant.md`:

- **Pickoff point**: a position in a track's, bus's, or master's own signal chain (`mixer.rs`,
  `dsp.rs`) — owned by that Track/Bus/MasterTrack object, presented on its own channel strip (a
  bus has no strip of its own — see §2).
- **Grid point**: a source or destination in the pickoff-point patch bay's crosspoint (`patch.rs`)
  — the input grid, the output grid, `track-in`, `bus-in`, `master-in`. A pickoff point
  (`track-out`, `bus-out`, `master-out`) can also *be* a grid source, but the grid itself is a
  separate object, not part of the Track/Bus/MasterTrack struct.

**The grid is the only NMOS-facing boundary** (bus/master-decorrelation pass, see §5): an
`input:<id>` entry is the sole thing that gets an IS-05 Receiver, an `output:<id>` entry is the
sole thing that gets an IS-04 Source+Flow+Sender. `Track`/`Bus`/`MasterTrack` are never themselves
NMOS-visible — a bus's or master's own signal only reaches the outside world if/when someone
explicitly patches it into an output-grid entry.

## 1. Track (input strip) structure

```
input-patch (track-in, grid destination)
        |
        v
      gain  ---------------------------------------> PreFader pickoff
        |
   [chain]   (ordered, typed processing slots, dsp.rs -- real DSP, applied in order -- zero or
        |     more of filter/eq/dynamics/phase/delay, in whatever order and count this track
        |     was built with; see §1a)
        |
      fader
        |
   mute/solo ------------------------------------> PostFader pickoff == track-out (grid source)
        |
      sends ---> one or more buses, each at that Send's own pickoff (Pre/PostFader)/level/on
```

- **`PreFader`** taps right after gain, before `chain` and the fader/mute/solo — see
  `mixer::PickoffPoint`'s own docs for why this app's chain only has two distinguishable taps (no
  separate per-slot taps the way `yam bus.png` shows a real console offering — a real, audible
  effect now runs at each slot, but this app still doesn't expose a tap between individual slots).
- **`PostFader`** taps after fader + mute/solo — this *is* the `track-out:<id>` grid source value.
- A track's contribution to a bus only ever happens through its own `sends` (`mixer::Send`) —
  never automatic, always an explicit send entry.

### 1a. `chain`: ordered, typed processing slots

`Track.chain`/`MasterTrack.chain` (`mixer.rs`) is a `Vec<dsp::ProcessingStage>` — zero or more
slots, each one of `filter`/`eq`/`dynamics`/`phase`/`delay` (`dsp::StageKind`), in whatever order
and count this resource was *built* with. Multiple slots of the same kind (e.g. two `dynamics`
stages) are legitimately allowed — nothing caps it, unlike the old fixed `dyn1`/`dyn2` fields this
replaced. **Order and membership are fixed at construction time** (CREATE or startup `Config`) —
there's no live reorder; delete and recreate a track/master to change its chain (cheap, since
runtime `CREATE`/`DELETE` already exists — see §4's own "Runtime topology" section).

A `TrackConfig`/`MasterTrackConfig`'s `chain` field is the authoritative, ordered source (a list of
`{"kind", "params"}` entries — `params` are that kind's own default field shape, e.g. `{"hp_hz":100}`
for a filter, omitted/`null` keeps that kind's own inaudible default). The older binary `template`
(`ChannelTemplate`, `Simple`/`FullChannel`) still exists as **deploy-time sugar only** — expanded
into the exact legacy fixed six-slot chain (filter → eq → dynamics → dynamics → phase → delay) when
`chain` itself is empty; an explicit non-empty `chain` always wins. Kept specifically so
`docker-entrypoint.sh`'s `CHANNEL_TEMPLATE` env var (hence every existing container/Kubernetes
deployment) and any hand-authored `config.json` using `"template":"full_channel"` keep building the
exact same chain they always did, with zero changes required — see §5's own iteration-history entry
for the full rationale.

### 1b. Automatic per-track/master latency compensation

Every stage kind is sample-synchronous today — Filter/EQ are direct-form biquads, Dynamics is a
feed-forward envelope follower, Phase is a sign flip: none of them buffer or look ahead, so they add
**zero** inherent processing latency (`dsp::ProcessingStage::latency_samples`, currently `0` for
every kind, including Delay — see below). That means two tracks with different chains, or no chain
at all, are already sample-aligned at their `track-out`/`master-out` pickoff points, with nothing
extra needed.

`mixer::LatencyCompensation` + `mixer::compute_compensation` exist anyway, as a forward-looking hook:
`engine.rs` sums each track's/master's own `chain.iter().map(|s| s.latency_samples()).sum()`, finds
the system-wide max across every track/master, and gives each one an automatic, invisible delay
line making up the difference — applied after the chain and fader, on the signal every downstream
consumer (sends, patches) actually receives. The currently-active amount is published read-only as
`channel/<id>/compensation-delay-ms` / `master/<id>/compensation-delay-ms` (no PUT exists for it).
**Deliberately unrelated to `dsp::DelayStage`**: that's a user-controlled creative effect an operator
dials in on purpose (confirmed with the user before building this) — compensating it away would
defeat its entire purpose, so it reports `0` for `latency_samples()` too, same as every other kind.
This mechanism is a no-op today (system-wide max latency is always `0`); it's ready for whenever a
future stage's own algorithm has real inherent latency (a lookahead limiter, a linear-phase EQ
mode, …) to report its own sample count and have every other track/master automatically stay
aligned with it, without needing to build the compensation machinery at that point too.

## 2. Bus (pure summer) structure

```
sends (from any track)  +  bus-in (grid destination, summing)
        |
        v
  bus-out pickoff (grid source)
```

- A bus is deliberately *not* a controllable channel strip — no fader, no mute, no processing
  chain, no gain, no solo, and owns no real MXL flow or NMOS presence of its own. `bus-out` is
  simply the raw sum; it's `MasterTrack` (§2b) that carries everything a bus used to (fader/DSP),
  fed via `master-in`. See the plan at `~/.claude/plans/snug-painting-elephant.md` §1/§3 for why —
  in short: patch `bus-out:<id>` into an output-grid entry (or a master) if a real destination is
  ever wanted, rather than every bus permanently owning a flow whether or not anything's listening.
- `bus-out` is the *only* bus pickoff point.

## 2b. Master track (controllable output strip) structure

```
master-in (grid destination, summing — from bus-out, track-out, master-out, or input-grid)
        |
        v
   [chain]  (ordered, typed processing slots -- same as tracks, see §1a)
        |
      fader
        |
      mute  --------------------------------------> master-out pickoff (grid source)
```

- A master has no `gain` and no `solo` (same as a bus never did).
- `master-out` is the *only* master pickoff point — same reasoning as `bus-out`.
- On a small mixer, one master is auto-paired 1:1 with each bus (`BusConfig.auto_master`,
  config.rs) — `master-in` is pre-patched to that bus's own `bus-out`, channel-for-channel,
  reproducing the fused bus/master behavior this app had before the decorrelation pass. On a bigger
  system, master count is fully decorrelated from bus count (more buses than masters, more masters
  than buses, one master fed by several buses, or masters cascaded into each other — all patched
  explicitly via `master-in`).
- A master owns no MXL flow or NMOS presence of its own either (§1) — patch `master-out:<id>` into
  an output-grid entry to make a specific master's signal externally visible.

## 3. Grid points (the pickoff-point patch bay, `patch.rs`)

| Grid point | Kind | Wire id | Present when |
|---|---|---|---|
| Input grid entry | source | `input:<entry_id>` | Always (config-seeded, `Config.input_grid`, or `INPUT_GRID_COUNT`-generated) or discovered (`nmos/discovery.rs`, id `registry:<sender_id>`) |
| Track direct-out | source | `track-out:<track_id>` | Every track, always (`Track.direct_out_prev`) |
| Bus output | source | `bus-out:<bus_id>` | Every bus, always (`Bus.output_prev`) |
| Master output | source | `master-out:<master_id>` | Every master, always (`MasterTrack.output_prev`) |
| Track input | destination, exclusive | `track-in:<track_id>` (implicit — addressed by the track's own `input-patch` param, not a wire id of its own) | Every track, always |
| Bus input | destination, summing | `bus-in:<bus_id>` (implicit — addressed by the bus's own `input-patch` param) | Every bus, always |
| Master input | destination, summing | `master-in:<master_id>` (implicit — addressed by the master's own `input-patch` param) | Every master, always |
| Output grid entry | destination, exclusive | `output:<entry_id>` (implicit — addressed by that entry's own `patch` param) | Config-seeded (`Config.output_grid`) or `OUTPUT_GRID_COUNT`-generated |

Self-loop rule: a track cannot patch its own `track-out:<id>` into its own `track-in:<id>`
(rejected — the one exclusive-destination case this applies to). A bus patching its own
`bus-out:<id>` into its own `bus-in:<id>`, or a master patching its own `master-out:<id>` into its
own `master-in:<id>`, is allowed (a benign one-period-delayed loop, not a same-period cycle — see
`engine.rs`'s pipeline docs) — both are summing destinations, where a same-resource self-feed
behaves like patching a real console insert-return into its own insert-send, which real patchbays
don't prevent either.

Evaluation order (why nothing here ever needs cycle detection): `track-in`/`bus-in`/`master-in`
always resolve a `track-out`/`bus-out`/`master-out` source from the *previous* period when that
source is itself a track/bus/master processed later in the same period's pipeline (concretely:
`master-in` may read *this* period's `track-out`/`bus-out` since tracks/buses finish earlier, but
always the *previous* period's `master-out`, even for another master processed earlier in the same
loop); `output:` (the pipeline's terminal stage) always resolves *this* period's values for all
three. See `engine.rs`'s module doc for the exact per-period order.

## 4. Generated WebSocket control surface (`amixer/{mixerId}/...`, `ws.rs`)

### Mixer-level (no id)

| Path | Direction | Value |
|---|---|---|
| `input-grid` | push only | `[{"id","label","channels"}, ...]` |
| `output-grid` | push only | `[{"id","label","channels"}, ...]` |
| `channel-list` | push only | `[{"id","label","channels"}, ...]`, sorted by id — every track that currently exists (see the `CREATE`/`DELETE` section below) |
| `sum-list` | push only | same shape, every bus |
| `master-list` | push only | same shape, every master |

### `input/<entry_id>/...` (per input-grid entry — `entry_id` is a string)

| Param | Value shape | Notes |
|---|---|---|
| `peakmeter` | `[number\|null, ...]` | push only (`meter_hz`) — `input:<id>`'s own pickoff meter, one entry per that entry's channel. No PUT-able param exists here over the WS protocol — an entry's own reader is opened/closed via its NMOS Receiver (IS-05 `PATCH .../receivers/<id>/staged`, `nmos/server.rs::receiver_patch`), not a WS param; routing an active entry to a track/bus/master is a separate step, over the ordinary `input-patch` params below. |

### `channel/<track_id>/...` (every track)

| Param | Value shape | Notes |
|---|---|---|
| `gain` | number (dB) | |
| `fader` | number (dB) | |
| `mute` | bool | |
| `solo` | bool | |
| `peakmeter` | `[number\|null, ...]` | push only (`meter_hz`), one entry per track channel — post-fader, `track-out:<id>`'s own pickoff meter |
| `input-meter` | `[number\|null, ...]` | push only — pre-gain, `track-in:<id>`'s own pickoff meter (what this track's own input-patch actually delivered this period, independent of gain/fader/mute) |
| `sends` | `[{"bus_id","on","level_db","pickoff":"pre_fader"\|"post_fader"}, ...]` | full-array replace |
| `input-patch` | `[{"source","channel"}\|null, ...]` | one entry per track channel, exclusive |
| `stage/<index>` | that slot's own kind-shaped value (below) | `index` into this track's own `chain` (§1a); out-of-range index rejected (warn, no-op), never a crash |
| `chain` | `[{"index","kind","params"}, ...]` | push only (`meter_hz`) — the full ordered chain; `params` is that slot's own kind-shaped value, same as `stage/<index>`'s own PUT/echo shape. The only way to discover what a track's chain even *is* (its shape isn't queryable any other way) |
| `compensation-delay-ms` | number (ms) | push only — see §1b; always `0` today, no PUT exists |

`stage/<index>`'s own value shape depends on that slot's `kind` (`chain[index]`'s own `kind`):
`filter` → `{"on","hp_hz","lp_hz"}`; `eq` → `{"on","bands":[{"freq_hz","gain_db","q"}, ...]}`;
`dynamics` → `{"on","threshold_db","ratio","attack_ms","release_ms","makeup_db"}`; `phase` →
`{"invert"}`; `delay` → `{"on","delay_ms"}`.

### `sum/<bus_id>/...` (every bus — a pure summer, see §2)

| Param | Value shape | Notes |
|---|---|---|
| `peakmeter` | `[number\|null, ...]` | push only — `bus-out:<id>`'s own pickoff meter, the raw sum (no fader exists to distinguish a separate "post-fader" reading from) |
| `input-meter` | `[number\|null, ...]` | push only — this bus's `bus-in:<id>` patch's own contribution *only*, measured before it's summed with tracks' own `sends` (see `engine.rs` step 4); not the same signal `peakmeter` reports |
| `input-patch` | `[[{"source","channel"}, ...], ...]` | one array per bus channel, summing |

No `gain`, no `solo`, no `fader`, no `mute`, no DSP stages — a bus carries none of those (see §2);
everything that used to live here moved to `master/<master_id>/...`, below.

### `master/<master_id>/...` (every master track — see §2b)

| Param | Value shape | Notes |
|---|---|---|
| `fader` | number (dB) | |
| `mute` | bool | |
| `peakmeter` | `[number\|null, ...]` | push only — post-fader, `master-out:<id>`'s own pickoff meter |
| `input-meter` | `[number\|null, ...]` | push only — `master-in:<id>`'s own pickoff meter (this master's only input mechanism, no separate "sends"-style second contributor the way a bus has) |
| `input-patch` | `[[{"source","channel"}, ...], ...]` | one array per master channel, summing |
| `stage/<index>`, `chain` | same shapes as tracks | see §1a — a master's `chain` has the same rules as a track's |
| `compensation-delay-ms` | number (ms) | push only — see §1b; always `0` today, no PUT exists |

No `gain`, no `solo` — a master never had either (same as a bus never did).

### `output/<output_id>/...` (config-seeded, `Config.output_grid`, or `OUTPUT_GRID_COUNT`-generated
— `output_id` is a string, not numeric — see §1: the *only* thing that gets a real NMOS
Source+Flow+Sender)

| Param | Value shape | Notes |
|---|---|---|
| `patch` | `[{"source","channel"}\|null, ...]` | one entry per output-grid entry's channel, exclusive |
| `peakmeter` | `[number\|null, ...]` | push only (`meter_hz`) — `output:<id>`'s own pickoff meter |

### Runtime topology: `CREATE`/`DELETE` (tracks/buses/masters only — never the grid)

New `op` values alongside `WATCH`/`PUT`, for changing the mixer's *processing scale* live while it's
running — deliberately decorrelated from the input/output grid's own sizing (`INPUT_GRID_COUNT`/
`OUTPUT_GRID_COUNT`, still config/env-only, still fixed for the process's lifetime — see §1). Full
design rationale: `~/.claude/plans/snug-painting-elephant.md`; see §5's own bullet below for a
summary.

| op | Path | Value | Notes |
|---|---|---|---|
| `CREATE` | `amixer/{mixerId}/channel`, `/sum`, or `/master` | `TrackConfig`\|`BusConfig`\|`MasterTrackConfig`-shaped | id is client-supplied, inside the payload — reuses the *exact* JSON shape `config.json`'s own `tracks[]`/`buses[]`/`masters[]` arrays already use, not a third schema |
| `DELETE` | `amixer/{mixerId}/channel/{id}`, `/sum/{id}`, or `/master/{id}` | none | — |

Both silent-on-failure, server-log only — same convention as every existing `PUT`, no new ack/error
envelope (this protocol has no request/response correlation id to hang one off of). Both re-publish
`channel-list`/`sum-list`/`master-list` immediately on success, on top of those lists' own regular
`meter_hz` tick — success is fast to observe without a new mechanism.

`CREATE` rejects only an id already in use or a payload that fails to deserialize; a track's `sends`
referencing an incompatible-channel or nonexistent bus *warns*, matching `main.rs`'s own existing
startup-time behavior rather than introducing a stricter runtime-only rule. `DELETE` actively scrubs
every other resource's dangling reference to the deleted id (`patch.rs`'s `scrub_*_references`) —
not required for crash-safety (a reference to a permanently-gone id already resolves to silence
forever) but required because `DELETE` makes **id reuse** a new, live event: without scrubbing, a
stale `Send`/patch entry left over from before the delete could silently "reconnect" to an unrelated
new resource later `CREATE`d with the same client-chosen id.

## 5. Iteration history

- **Milestone 1** (`patch.rs` introduced): `track-in`/`bus-in` grid destinations, `input:`/
  `track-out:` grid sources, `input-grid` listing. Replaced the old whole-track raw-flow_id
  `source` param.
- **Milestone 2**: output grid (`output:` destinations, `output-grid` listing), `bus-out:` grid
  source added.
- **Milestone 3**: `nmos/discovery.rs` — registry-driven input grid entries (`registry:<sender_id>`
  ids) alongside config-seeded and IS-05-synthesized ones.
- **Sends rework**: `bus_assign: HashSet<u32>` (a plain per-track set of bus ids, no per-send
  parameters) replaced by `Track.sends: Vec<Send>` — real pickoff point (`PreFader`/`PostFader`),
  on/off, and variable `level_db` per send, informed by `~/DEV/yam bus.png`/`~/DEV/Vista grid.png`.
  A plain bus assignment is now just a `Send` left at its `level_db: 0.0` default.
- **Processing chain**: `dsp.rs` — `filter`/`eq`/`dyn1`/`dyn2`/`phase`/`delay`, `Option<Stage>` on
  both `Track` and `Bus`, gated by the new `ChannelTemplate` (`Simple`/`FullChannel`) — structural
  placeholders (no signal effect yet), confirmed with the user before building rather than assumed.
- **Live-state persistence**: `persistence.rs` — see §6 below.
- **Pickoff metering completeness**: before this pass, only `track-out`/`bus-out` had a real meter
  (`Track.meter_db`/`Bus.meter_db`, reused from before the patch bay existed at all). The other 4 of
  6 pickoff kinds had none — driven by the patch-grid view's own design needing a live signal
  reading on every source/destination it presents, not just the two that happened to have one
  already. Added: `InputGridEntry.meter_db` (`input:<id>`, peaked in `engine.rs` step 1 where each
  entry is read), `Track.input_meter_db` (`track-in:<id>`, peaked in step 3 right after
  `resolve_track_in`, before gain), `Bus.input_meter_db` (`bus-in:<id>`, the one structural change —
  `resolve_bus_in` now resolves into its own scratch buffer in step 4, peaked there, *then* mixed
  into the shared sends accumulator, since once summed the two contributions aren't separable), and
  `OutputGridEntry.meter_db` (`output:<id>`, peaked in step 6 right after `resolve_output`). All four
  ride the same `meter_hz` broadcaster tick as the existing meters (`channel/<id>/input-meter`,
  `sum/<id>/input-meter`, `input/<id>/peakmeter`, `output/<id>/peakmeter` — see §4).
- **Bus/master decorrelation** (`~/.claude/plans/snug-painting-elephant.md`): split the old fused
  `Bus` (summer + controllable strip + its own MXL flow + NMOS Sender) into a pure-summer `Bus`
  (§2) and a new `MasterTrack` (§2b, everything `Bus` lost) — decorrelates bus count from
  "controllable master strip" count (more buses than masters, more masters than buses, a master fed
  by several buses, masters cascaded into each other). New `master-in`/`master-out` grid kinds
  (§3), new `master/<id>/...` WS surface (§4). Alongside this, the input/output grid became the
  *only* NMOS-facing surface (§1): `Track`'s old per-track Receiver and the ephemeral
  `"recv:<track_id>"` input-grid-entry-synthesis-on-activation mechanism are both gone, replaced by
  every input-grid entry carrying its own stable Receiver from the start (`INPUT_GRID_COUNT`/
  config-sized, decorrelated from track count), and the output grid gaining real IS-04/IS-05
  mirroring it never had before (`OUTPUT_GRID_COUNT`-sized, decorrelated from bus/master count).
  IS-05 receiver activation (`nmos/server.rs::receiver_patch`) no longer touches any patch as a
  side effect — activating an input and routing it to a track/bus/master are now two fully
  independent steps.
- **Runtime topology `CREATE`/`DELETE`** (`~/.claude/plans/snug-painting-elephant.md`): before this,
  the mixer's own processing scale (track/bus/master count) was fixed at startup from `Config`,
  requiring a full process restart to change — decorrelated in principle from the input/output
  grid's own sizing, but in practice just as static. Added live `CREATE`/`DELETE` (§4) backed by an
  engine rework: `MixerState.tracks`/`buses`/`masters` changed from immutable `Vec<Arc<T>>` to
  `Mutex<HashMap<u32, Arc<T>>>` (the same shape `patch::InputGrid`/`OutputGrid` already used, already
  proven safe for concurrent runtime mutation via `nmos/discovery.rs`'s own insert/remove). The
  real-time engine loop (`engine.rs::run`) splits into two tiers: id→`Arc<T>` snapshots refresh every
  period unconditionally (cheap `Arc` clones, matching the grid's own `.snapshot()` pattern already
  in the loop), while the `f32` sample scratch buffers — the allocations actually worth not paying
  every period — are keyed by id (not position) and only rebuilt when a new `topology_generation`
  counter (bumped on every create/delete) has changed since the scratch was last checked, tolerating
  a changing track/bus/master count without a per-period allocation. `Track`/`Bus`/`MasterTrack`
  gained a `dynamically_created` field so persistence (§6) can tell a `CREATE`d resource apart from
  a config-authored one and reconstruct it faithfully on restart. New `channel-list`/`sum-list`/
  `master-list` broadcasts close a real pre-existing gap: there was previously no way for a client
  to discover what track/bus/master ids exist at all.
- **Ordered, typed processing-chain slots** (§1a; same plan file as the entry above): replaced the
  fixed six named `Option<Stage>` fields + binary `ChannelTemplate` gate with `Track.chain`/
  `MasterTrack.chain: Vec<dsp::ProcessingStage>` — a user now designs a track's/master's own chain
  (which stages, in what order, some kinds repeated or omitted entirely) instead of picking between
  exactly two fixed shapes. Order is fixed at construction time only (no live reorder — delete/
  recreate to change it, consistent with how `template` already worked before this). New
  `dsp::StageKind`/`ProcessingStage` sum type wraps the five existing concrete stage structs
  unchanged; `ws.rs`'s five `apply_*`/`*_json` pairs narrowed from `&Option<T>` to `&T` (presence is
  now index-existence in the chain, not `Option::None`). **Breaking wire-protocol change**: the six
  standalone PUT params (`filter`/`eq`/`dyn1`/`dyn2`/`phase`/`delay`) are gone, replaced by
  `stage/<index>` (one slot, addressed by its position in `chain`) and a new `chain` discovery
  broadcast (§4) — no legacy param aliases, since `dyn1`/`dyn2` become ambiguous the moment a chain
  has any count of dynamics stages other than exactly two. `ChannelTemplate` (`Simple`/`FullChannel`)
  deliberately kept, not removed — now pure deserialize-time sugar in `config.rs` only, expanded
  into the exact legacy fixed chain when a resource's `chain` is left empty, so every existing
  `docker-entrypoint.sh`/`kube-example.yaml` container deployment and hand-authored `config.json`
  using `"template":"full_channel"` keeps building the identical chain with zero changes (pinned by
  a dedicated back-compat regression test, `config.rs::track_config_with_only_template_still_builds_the_legacy_chain`).
- **Real DSP for the processing-chain stages** (`~/.claude/plans/snug-painting-elephant.md`,
  superseding the entry above's "structural placeholders" framing): every `chain` slot now applies a
  real effect — `engine.rs` calls `ProcessingStage::process` once per slot, in chain order, between a
  track's gain and fader (and, for a master, between its input-meter measurement and its fader).
  Filter is a cascaded HP/LP biquad pair (2nd-order Butterworth); EQ is a bank of peaking biquads,
  one per band; Dynamics is a feed-forward peak detector with exponential attack/release and a static
  compressor curve (`ratio` clamped `>= 1.0` — this app still doesn't model a distinct gate/expander
  mode); Phase is an exact sign flip; Delay is a per-channel circular buffer, pre-allocated to a fixed
  2-second cap so a `delay_ms` change never allocates on the audio thread. New `biquad.rs` holds the
  hand-rolled RBJ "Audio EQ Cookbook" coefficient formulas (no DSP crate dependency). Each stage's
  persistent per-channel signal state (filter/EQ biquad registers, the dynamics envelope, the delay
  ring) lives in new private, `Mutex`-wrapped fields alongside that stage's existing parameters —
  sized once at construction from the track's/master's own `channels` (and, for Delay, `sample_rate`),
  which is why `ProcessingStage::default_on`/`StageSlotConfig::build`/`config::build_chain`/
  `Track::new*`/`MasterTrack::new*`/`topology::build_track`/`build_master` all gained `channels`/
  `sample_rate` parameters (pure plumbing, no behavior change to any of them beyond that). At every
  stage's own default parameters the effect is an exact no-op (pinned by dedicated unit tests), so
  this does not change the audible behavior of any existing chain until its parameters are actually
  moved off default. This is the real-time audio thread's only new locking beyond what it already did
  for gain/fader/sends/mute — deliberately not a new risk category (see the plan's Context section).

  **Live-verified end-to-end**, not just unit-tested: a standalone throwaway Rust tool (same `mxl`
  crate/API `flow.rs` itself uses — no `speaker-test`/ALSA involved, this app has no sound-card path
  at all) wrote a real 300 Hz sine into a real MXL flow feeding a track's `input-patch`, and read the
  processed result back from a real MXL flow patched from that track's `track-out`. With a live
  `hp_hz: 2000` filter stage, the tone came out attenuated **34.3 dB**; PUTting `hp_hz` back down to
  its inaudible default (20 Hz) over the running WS connection dropped that to a clean ~0 dB
  passthrough — confirming the engine applies a chain slot's *current* live parameters every period,
  not a value cached at construction or CREATE time. (One early reading in that same session showed a
  spurious, perfectly-alternating every-other-period silence pattern on the output flow; extensive
  follow-up — varying the track, its chain, and the patch — never reproduced it on a freshly-started
  instance, and it tracked a test-only condition: the tool's own tone generator had a fixed, short
  lifetime and was mid-exit-or-already-dead during that one reading, not a defect in this app's
  engine or the new DSP code. Documented here rather than silently dropped, in case it recurs.)
- **Automatic per-track/master latency compensation** (§1b): confirmed every stage kind is already
  sample-synchronous (0 inherent latency), so tracks with different chains — or none at all — are
  already sample-aligned today, no work needed for that alone. Built the compensation mechanism
  anyway, as a forward-looking hook, at the user's explicit request: `dsp::ProcessingStage::
  latency_samples()` (0 for every kind, including Delay — its `delay_ms` is a deliberate effect, not
  latency to compensate for), `mixer::LatencyCompensation`/`compute_compensation`, wired into
  `engine.rs`'s existing topology-generation-triggered rebuild, published read-only as
  `.../compensation-delay-ms`. A no-op today by construction; ready for whenever a future
  non-zero-latency stage (a lookahead limiter, a linear-phase EQ mode, …) needs it.
- **Gatherer** (deferred — design sketch only, see the plan's §15, not implemented): a *separate*
  future app, not a change to this one, for bundling several independently-produced narrow MXL
  flows into one wide flow (SMPTE 2110-30-style stream consolidation) — reads N existing flows,
  writes one new flow it alone owns, needing no new capability in `flow.rs`. Ties into the existing
  `PackedTxName`/`packed_tx_flow_id` convention already in `config.rs`/`ids.rs`: the gatherer would
  be what actually *produces* a well-known `packed-tx:<name>` flow from several sources.

## 6. Persistence and redundancy

`Config` only ever describes the *shape* of a deployment (which tracks/buses exist, their
template, initial sends) — everything mutated afterward via a WS PUT (gain/fader/mute/solo/sends,
DSP stage params, `track-in`/`bus-in` patches) lives only in memory unless `Config.state_path` is
set. `persistence.rs` captures/applies that live state by reusing `ws.rs`'s own per-field JSON
builders/appliers directly (`sends_json`, `apply_filter`, `patch.track_in_json`, ...) rather than a
second, parallel (de)serialization — those already are the tested, authoritative mapping between
live state and JSON.

- **On startup**: if `state_path` is set and a file already exists there, it's loaded and applied
  on top of the config-built tracks/buses, before the engine thread starts. A brand-new deployment
  (no file yet) just proceeds with config defaults, silently.
- **Runtime-`CREATE`d topology**: the saved file also carries a `"topology"` section — enough of
  each `dynamically_created` track/bus/master (id/label/channels/chain/sends/current values) to
  reconstruct it via the same `TrackConfig`/`BusConfig`/`MasterTrackConfig` shape `CREATE`'s own
  payload uses. On startup this is applied *before* the ordinary value-resume step above — a
  resource has to already exist in the live collection for a resumed value to have anywhere to go,
  so reversing that order would silently strand every `CREATE`d id's resumed values with zero
  compiler signal (a dedicated regression test protects this ordering, `persistence.rs`).
  Config-authored tracks/buses/masters are never written here — only ones that didn't come from
  `Config` in the first place.
- **While running**: saved every 5s (bounds staleness if the process is ever killed
  ungracefully) and once more, synchronously, on receiving SIGTERM — which is what Kubernetes sends
  (and waits `terminationGracePeriodSeconds` before SIGKILL) when a `livenessProbe` failure
  triggers a pod replacement, closing the staleness window to ~zero for the case that actually
  matters.
- **`/healthz`**: a minimal liveness/readiness target (`main.rs`) — deliberately just "is the HTTP
  server answering," not deep engine-thread/flow health, for now.
- **Why this needs a StatefulSet, not the Deployment the rest of this app's container-sizing story
  otherwise uses**: state resume requires the *same* replica (stable identity, stable volume) to
  come back after a replacement — a plain Deployment's pods get a new random name each time one is
  replaced, which would leave `STATE_PATH`/`INSTANCE_NAME` pointing nowhere useful. See
  `kube-example.yaml`'s own comments for the full reasoning.
- **What this is not**: BCP-008 (`NcReceiverMonitor`/`NcSenderMonitor`, IS-12) defines *stream
  connection* health — link/connection/sync/essence status for NMOS Receivers/Senders — nothing
  for MXL resources specifically, and nothing at the container/process level. "Should this
  container be replaced" is answered by Kubernetes' own liveness probe, not by anything in the
  NMOS/BCP-008 layer. A future BCP-008-01/02 implementation on this app's own mirrored output-grid
  Senders/input-grid Receivers (the only NMOS-visible resources — see §1; mirroring the sibling
  `aes67-linux-daemon`'s existing precedent) would be a useful, narrower signal — "is this specific
  patch/send connected and clean" — layered on top of, not instead of, the container-health
  mechanism above.
- **What this is not (yet)**: active-active hot failover (two replicas live simultaneously, an
  instant handoff with no resume delay) needs a shared external store (etcd/Redis) with real
  conflict resolution between concurrent writers — a materially bigger architecture than the
  PVC-backed restart-resume model here, and not something this app has taken on.
