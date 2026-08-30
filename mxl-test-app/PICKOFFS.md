# Pickoff points, grid points, and the generated control surface

Reference for exactly what mxl-test-app generates in terms of signal flow (pickoff points) and
control (the `amixer` WebSocket protocol, `ws.rs`) — for a track (input strip), a bus (output
strip), and the pickoff-point patch bay's own grid objects. Written against real console reference
material (`~/DEV/yam bus.png`, a Yamaha "CH to MIX" send-chain diagram; `~/DEV/Vista grid.png` and
`~/DEV/Vista grid patch.png`, a Studer DSP-router/crosspoint reference) rather than invented from
scratch — see each section for which document informed which part.

Two vocabularies stay deliberately separate, per the plan at
`~/.claude/plans/snug-painting-elephant.md`:

- **Pickoff point**: a position in a track's or bus's own signal chain (`mixer.rs`, `dsp.rs`) —
  owned by that Track/Bus object, presented on its own channel strip.
- **Grid point**: a source or destination in the pickoff-point patch bay's crosspoint (`patch.rs`)
  — the input grid, the output grid, `track-in`, `bus-in`. A pickoff point (`track-out`, `bus-out`)
  can also *be* a grid source, but the grid itself is a separate object, not part of the Track/Bus
  struct.

## 1. Track (input strip) structure

```
input-patch (track-in, grid destination)
        |
        v
      gain  ---------------------------------------> PreFader pickoff
        |
   [filter]  (FullChannel template only, dsp.rs -- structural placeholder, no signal effect yet)
        |
     [eq]    (FullChannel template only)
        |
   [dyn1]    (FullChannel template only)
        |
   [dyn2]    (FullChannel template only)
        |
  [phase]    (FullChannel template only)
        |
  [delay]    (FullChannel template only)
        |
      fader
        |
   mute/solo ------------------------------------> PostFader pickoff == track-out (grid source)
        |
      sends ---> one or more buses, each at that Send's own pickoff (Pre/PostFader)/level/on
```

- `[bracketed]` stages exist only if the track's `ChannelTemplate` (`config.rs`) is `FullChannel` —
  `Option<Stage>` on `Track`, `None` (not present, not skipped) for `Simple`.
- **`PreFader`** taps right after gain, before the (possibly absent) processing stages and the
  fader/mute/solo — see `mixer::PickoffPoint`'s own docs for why this app's chain only has two
  distinguishable taps (no separate PRE_FILTER/PRE_DYN1/PRE_DYN2 taps the way `yam bus.png` shows a
  real console offering, since the stages between them don't yet do anything audible to tap
  differently around).
- **`PostFader`** taps after fader + mute/solo — this *is* the `track-out:<id>` grid source value.
- A track's contribution to a bus only ever happens through its own `sends` (`mixer::Send`) —
  never automatic, always an explicit send entry.

## 2. Bus (output strip) structure

```
sends (from any track)  +  bus-in (grid destination, summing)
        |
        v
   [filter] [eq] [dyn1] [dyn2] [phase] [delay]   (FullChannel template only, same as tracks)
        |
      fader
        |
      mute  --------------------------------------> bus-out pickoff (grid source)
        |
  bus's own real MXL flow (always-on, unconditional write every period)
```

- A bus has no `gain` and no `solo` (never did, and buses never gained processing-stage
  entry points in `dsp.rs` — beyond the same `Option<Stage>` fields tracks got, both driven by the
  same `ChannelTemplate`).
- `bus-out` is the *only* bus pickoff point — there's no separate pre/post-fader distinction for
  buses (a bus has no upstream "gain" stage the way a track does, and its own `Send`-style
  processing-stage taps aren't modeled — everything a bus does happens after summing, in one
  chain).

## 3. Grid points (the pickoff-point patch bay, `patch.rs`)

| Grid point | Kind | Wire id | Present when |
|---|---|---|---|
| Input grid entry | source | `input:<entry_id>` | Always (config-seeded, `Config.input_grid`) or discovered (Milestone 3, `nmos/discovery.rs`, id `registry:<sender_id>`) or IS-05-synthesized (`nmos/server.rs`, id `recv:<track_id>`) |
| Track direct-out | source | `track-out:<track_id>` | Every track, always (`Track.direct_out_prev`) |
| Bus output | source | `bus-out:<bus_id>` | Every bus, always (`Bus.output_prev`) |
| Track input | destination, exclusive | `track-in:<track_id>` (implicit — addressed by the track's own `input-patch` param, not a wire id of its own) | Every track, always |
| Bus input | destination, summing | `bus-in:<bus_id>` (implicit — addressed by the bus's own `input-patch` param) | Every bus, always |
| Output grid entry | destination, exclusive | `output:<entry_id>` (implicit — addressed by that entry's own `patch` param) | Only if `Config.output_grid` configures it (Milestone 2) |

Self-loop rule: a track cannot patch its own `track-out:<id>` into its own `track-in:<id>`
(rejected). A bus patching its own `bus-out:<id>` into its own `bus-in:<id>` is allowed (a benign
one-period-delayed loop, not a same-period cycle — see `engine.rs`'s pipeline docs).

Evaluation order (why nothing here ever needs cycle detection): `track-in`/`bus-in` always resolve
a `track-out`/`bus-out` source from the *previous* period; `output:` (the pipeline's terminal
stage) always resolves *this* period's values. See `engine.rs`'s module doc for the exact 6-step
per-period order.

## 4. Generated WebSocket control surface (`amixer/{mixerId}/...`, `ws.rs`)

### Mixer-level (no id)

| Path | Direction | Value |
|---|---|---|
| `input-grid` | push only | `[{"id","label","channels"}, ...]` |
| `output-grid` | push only | `[{"id","label","channels"}, ...]` |

### `channel/<track_id>/...` (every track)

| Param | Value shape | Notes |
|---|---|---|
| `gain` | number (dB) | |
| `fader` | number (dB) | |
| `mute` | bool | |
| `solo` | bool | |
| `peakmeter` | `[number\|null, ...]` | push only (`meter_hz`), one entry per track channel |
| `sends` | `[{"bus_id","on","level_db","pickoff":"pre_fader"\|"post_fader"}, ...]` | full-array replace |
| `input-patch` | `[{"source","channel"}\|null, ...]` | one entry per track channel, exclusive |
| `filter` | `{"on","hp_hz","lp_hz"}` or `null` | `null`/rejected if template is `Simple` |
| `eq` | `{"on","bands":[{"freq_hz","gain_db","q"}, ...]}` or `null` | same |
| `dyn1`, `dyn2` | `{"on","threshold_db","ratio","attack_ms","release_ms","makeup_db"}` or `null` | same |
| `phase` | `{"invert"}` or `null` | same |
| `delay` | `{"on","delay_ms"}` or `null` | same |

### `sum/<bus_id>/...` (every bus)

| Param | Value shape | Notes |
|---|---|---|
| `fader` | number (dB) | |
| `mute` | bool | |
| `peakmeter` | `[number\|null, ...]` | push only |
| `input-patch` | `[[{"source","channel"}, ...], ...]` | one array per bus channel, summing |
| `filter`, `eq`, `dyn1`, `dyn2`, `phase`, `delay` | same shapes as tracks | `null`/rejected if template is `Simple` |

No `gain`, no `solo` — buses never had either.

### `output/<output_id>/...` (only if `Config.output_grid` configures entries — `output_id` is a
string, not numeric)

| Param | Value shape |
|---|---|
| `patch` | `[{"source","channel"}\|null, ...]`, one entry per output-grid entry's channel, exclusive |

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
