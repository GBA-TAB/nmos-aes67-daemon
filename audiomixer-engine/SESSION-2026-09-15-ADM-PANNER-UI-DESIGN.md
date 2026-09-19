# ADM object panner UI — conceptual design (not implemented)

Design-only, per user request. No code changes in this pass. Written against the real backend
model (`mixer::mix_into_scaled_with_object_pan`, `mixer::vbap_bed_gains`, `adm::AdmObjectMetadata`,
`ws.rs`'s `channel/{id}/adm-object` param — all already implemented and live, see engine.rs:453-490)
so the UI concept matches what the panning math can actually do, not an idealized version of it.

## What the backend actually computes (the constraint the UI has to be honest about)

- An object's position is **one single spherical point**: `{azimuth_deg, elevation_deg, distance}`
  (`adm::AdmPosition`), stored once on the track's `adm_object` slot. There is no separate value
  per destination.
- Panning is **not** real 3D triangulated VBAP. It's a documented two-ring approximation
  (`mixer.rs`'s own doc comment on `vbap_bed_gains`): a flat *bed ring* (L/R/C/Ls/Rs at real
  BS.2051 azimuths, all elevation 0) and, for `Surround5_1_4` only, a second flat *height ring*
  (Ltf/Rtf/Ltb/Rtb at elevation 30°), blended between the two rings by how close `elevation_deg` is
  to the height ring's own angle. `M`/`Lfe` are never panned to.
- **The same one position re-panned fresh per destination bus, every period** — the exact same
  `(azimuth, elevation)` produces different real gains depending on which bus a given send targets,
  because `vbap_bed_gains` is called with *that send's own bus layout* (`Surround5_1` /
  `Surround7_1` / `Surround5_1_4` — only those three have real ring data; anything else silently
  falls back to plain downmix, no panning at all). A track can send the same object to several
  buses at once; each gets its own correctly-computed gain pattern for free, with zero extra UI
  state needed per destination.
- `distance` and `width`/`height`/`depth` (extent) are captured and round-trip through Serial ADM
  export, but **nothing in the current mix path reads them** — they don't affect gains today.
- `gain_db` is a plain object-level trim, applied via the `scale` parameter alongside the VBAP gain
  — not part of the spatial math.

This is why the UI shouldn't be a literal Avid Cube or an L-ISA-style free 3D scene: both of those
visually promise true 3D triangulation (any point in a volume resolves to a physically consistent
speaker blend), which this backend doesn't do. Overbuilding the visual metaphor would just teach
operators to expect precision that isn't there. The honest equivalent of "cube panner" for a
two-ring system is closer to a **planetarium-style azimuth ring + a separate elevation control** —
which is also considerably cheaper to build reliably in a Blazor canvas than a real orbit-camera 3D
view.

## Proposed UI: ring pad + elevation rail, not a free 3D cube

```
┌─ ADM Object Pan — "Dialogue" ─────────────────────────────┐
│  Preview against: [ 5.1.4 Bed ▾ ]                          │
│                                                              │
│              Ltf         C          Rtf                     │
│                 \        |        /       (height ring,     │
│                  \       |       /         only shown for   │
│         Ls ───────●──────┼──────●─────── Rs  a layout that  │
│                     \     |     /            has one)       │
│                      \    |    /                             │
│                       \   |   /                              │
│                  Ltb ───●─┼─●─── Rtb                         │
│                                                                │
│              [drag the object dot anywhere on the ring plane]│
│                     ●  <- object, glow = current gain sum    │
│                                                                │
│  Azimuth   [ -110° ]◀━━━━━━●━━━━▶   Elevation [ 30° ]◀━●━▶   │
│  Distance  [  2.4m ]◀━━●━━━━━━━▶    Gain      [ -3.0 dB ]    │
│                                                                │
│  Per-speaker gain:  L ▁ R ▂ C ▁ Ls █ Rs ▁ Ltf ▂ Rtf ▁ Ltb ▓  │
└────────────────────────────────────────────────────────────┘
```

- **Ring pad (primary, canvas-based)**: a top-down plan view. Speaker dots are placed at their
  *real* `role_angle()` azimuths (verified BS.2051 values already in `mixer.rs`), labeled with
  their role short name. The object is a single draggable dot; dragging it around the ring changes
  `azimuth_deg` live. A dashed inner ring represents the height ring's own angle so both are visible
  at once without needing a separate 3D projection — elevation is communicated by the object dot's
  own size/brightness shifting toward the inner ring as `elevation_deg` approaches 30°, not by fake
  depth/perspective.
- **Elevation** is a separate horizontal slider, not a drag axis on the pad — deliberately, since
  the backend's own elevation handling is a linear blend between exactly two flat rings, not a
  continuous 3D coordinate; a slider communicates "blend amount between two known rings" more
  honestly than implying free vertical movement in space.
- **Distance** is a slider/number field only, visually reflected as the object dot's opacity/size
  (closer = larger/brighter) purely as a *reminder it's captured*, not because it changes the mix —
  worth a small "no effect on level yet" hint in the UI copy so it doesn't read as broken.
- **Destination-bus selector** ("Preview against: ▾") lists every `Surround5_1`/`Surround7_1`/
  `Surround5_1_4` bus this track currently sends to (via its own `sends` list) — switching it only
  changes what layout the ring/gain-bar preview renders against; it does **not** create a
  per-destination position, since (per above) there is only ever one real position. If the track
  has no VBAP-capable send yet, the pad still works (freely settable) but shows a plain "not
  currently panning anything — no send targets a VBAP-capable bus" notice, so it's clear the
  position is being captured but has nowhere to act yet.
- **Per-speaker gain readout**: small bar-per-role strip under the pad, one bar per role in the
  previewed layout, height/brightness driven by that role's real computed gain. This needs a
  client-side mirror of `vbap_bed_gains`'s math for live drag feedback (cheap: it's closed-form,
  no simulation) — the authoritative mix always happens server-side regardless; this is preview
  only, same "client renders a fast approximation, server is truth" relationship the meter/fader
  canvases already have with their own dB curves.

## Data flow (reuses the existing param, no new backend surface)

- On open, `GET`/read the current broadcast value of `channel/{id}/adm-object` (already exists,
  `ws.rs:430`) to seed the pad.
- Dragging the pad updates local component state continuously (60fps redraw, same pattern
  `AudioFader.razor`'s canvas drag already uses) and sends `PUT channel/{id}/adm-object` with the
  full `{name, gain_db, position:{azimuth, elevation, distance}, width, height, depth}` shape,
  **debounced** the same way `AudioFader` already debounces its own drag PUTs
  (`DebounceIntervalMs`) rather than flooding a PUT per animation frame.
- The panel re-syncs from the next broadcast the same way every other live-editable param in this
  app already does (`MixerStateService.MixerStateChanged` → `Refresh()` → `StateHasChanged()`) —
  no special-casing needed.

## Where it lives on the strip

A new conditionally-rendered block on `TrackStrip.razor`, same shape as `ProcessingChain`/`Sends` —
but only rendered when `track.adm_object` is non-null (most tracks are plain beds and would never
show it). Given how much screen space a ring pad needs, the block itself is a small summary button
("ADM PAN — Az -110° El 30°") that opens a `Modal` (same `PatchGrid`/`chain-slot-modal-wide` popup
pattern already established for the other space-hungry editors), not an inline canvas squeezed into
the narrow vertical strip.

## Explicitly out of scope for a first pass

- A real free-form 3D/orbit-camera view (would overstate the backend's actual two-ring precision —
  see the honesty argument above).
- Width/height/depth (extent) actually affecting the mix — needs its own VBAP extension (spreading
  gain across neighboring speakers) not built yet; the UI would just be capturing values nothing
  consumes.
- Per-destination-bus independent positions (the backend model is one position, many destinations,
  by design — see above).
- Trajectory automation / keyframed movement.
- Multi-object simultaneous overview (e.g. seeing every ADM object on one shared stage plot at
  once) — a real, useful feature for a later pass, but a materially different UI (a stage view
  showing N objects) from this one (a single object's own pan control).
