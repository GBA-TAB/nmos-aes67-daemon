# Automated sequential audio-path check

## Status: implemented and working, same pass

Built the same session this was designed, not left as a plan:

- The prerequisite broadcast fix (`input-patch`/`bus-in`/`master-in`/output `patch` now ride the
  periodic tick, same as `adm-object`) is real and live in `ws.rs`.
- `src/bin/audio-path-check.rs` exists, builds clean (`cargo build --release --bin
  audio-path-check`), and is genuinely useful on its first real run against the live instance --
  it immediately caught two real bugs in its own first implementation:
  1. **Path-count explosion**: an 8-channel patch names the same (source, dest) pair once per
     channel; without deduplicating on edge insert, an 8ch-in-8ch-out chain produced dozens of
     identical duplicate paths. Fixed (`add_edge` now dedupes).
  2. **Wrong meter on a send-derived hop**: `sum/{id}/input-meter` only ever reflects a bus's own
     *direct* bus-in patch (PICKOFFS.md), not what tracks send to it -- a send and a bus-in patch
     are two independent contributors to `bus-out`, not sequential stages. The first version routed
     send edges through `BusIn` anyway, so a bus fed *only* by sends (the normal case) always
     reported a false "silent" at that hop even while genuinely passing real signal. Fixed (sends
     now edge straight to `BusOut`; `BusIn` is reached only by an actual direct patch entry).
  3. Also fixed while verifying: "no complete path discovered at all" was treated as a silent
     non-failure (exit 0) -- now exits 1 like any other broken/incomplete routing state, since an
     unpatched chain is just as much "not working" as a patched-but-silent one.
- Verified against the live instance both ways: a real, fully-patched chain reports one clean
  `PATH OK` with all 7 hops passing; deliberately unpatching the output grid entry correctly
  reports "no complete paths" and exits 1; both states were re-confirmed after fixing the bugs above.
- Also surfaced (not fixed, flagged here): **output-grid patches aren't covered by
  `persistence.rs`'s capture/apply at all** -- a track's own `input_patch`/`sends` survive a
  restart (already existed), but `output:{id}/patch` doesn't, discovered directly because this tool
  now makes that patch visible where it wasn't before. A natural small follow-up, same shape as
  everything else `capture`/`apply_snapshot` already handles.
- `--isolate` mode (below) is still just design, not built this pass.

---

Original design follows.

A structured, repeatable tool that walks the *entire* routing graph — input grid → track-in patch
→ track send → bus → master → output grid patch — and reports signal presence or absence at every
single hop, one path at a time, so a break anywhere in the chain is localized immediately instead
of just "is there sound at the very end" (which is all last night's ad-hoc probe scripts did).

## Why a passive check needs one small backend addition first

The WS protocol has no `GET` op (confirmed this session, tracing the live demo) — a client only
ever learns a value from the periodic broadcast tick or a PUT's own echo. Meters (`peakmeter`/
`input-meter`) and `sends` already ride that tick. **The patch arrays that actually define the
graph — a track's `input-patch`, a bus's `input-patch` (bus-in), a master's `input-patch`
(master-in), and an output-grid entry's `patch` — do not.** A tool can watch signal levels change
together over time and *guess* the topology, but that's fragile and slow; it should just be able to
ask.

This needs the exact same fix already applied to `adm-object` this session: add those four params
to `run_meter_broadcaster`'s per-resource loop in `ws.rs`, so a freshly-connected client (this tool,
or anything else) can discover the real graph directly instead of inferring it. Small, mechanical,
same justification already used once — do this first, the checker is straightforward after.

## What the tool does

1. **Connect and listen** for one broadcast cycle (a couple seconds at the existing `meter_hz`) to
   build a complete snapshot: `channel-list`/`sum-list`/`master-list`, `input-grid`/`output-grid`,
   every resource's `input-patch`/`sends`, and every pickoff meter.
2. **Build the graph** from that snapshot: for each input-grid entry, which track(s) patch from it
   (`track-in`); for each track, which bus(es) it sends to (`sends`); for each bus, which master(s)
   read it (`master-in`, including the `auto_master` 1:1 case); for each master, which output-grid
   entry patches from it (`output` patch). A track/bus/master with no real connection on either side
   is simply not part of any discovered path — not an error, just not tested.
3. **Enumerate every discovered end-to-end path** (input → ... → output) — there can be more than
   one hop count per path (a track's post-fader send is one hop from a bus's own direct `bus-in`
   patch, both legitimate), and more than one path can share a middle segment (three tracks into one
   bus) — each gets checked independently.
4. **Walk each path sequentially, hop by hop**, checking that hop's own meter is above a silence
   threshold (default -50 dBFS, configurable) on at least one channel. Report every hop, not just
   the failing one — a working chain is worth seeing in full, not just a pass/fail bit.
5. **Report** in the same shape this session's own manual traces already used, since that format
   proved itself useful live tonight:

   ```
   Path: input:in-gen -> channel:50 (Dialogue) -> sum:1 (Monitor Bus) -> master:1 -> output:out-monitor
     [PASS] input:in-gen/peakmeter        -20.0 dBFS
     [PASS] channel:50/input-meter        -20.0 dBFS
     [PASS] channel:50/peakmeter          -20.0 dBFS
     [PASS] sum:1/peakmeter               -20.0 dBFS
     [PASS] master:1/peakmeter            -20.0 dBFS
     [PASS] output:out-monitor/peakmeter  -20.0 dBFS
     => PATH OK (6/6 hops confirmed)

   Path: input:in-cop1 -> channel:2 (COP1 Track) -> sum:1 (Monitor Bus) -> master:1 -> output:out-monitor
     [FAIL] input:in-cop1/peakmeter       silent
     [SKIP] channel:2/input-meter         (upstream already silent)
     ...
     => PATH BROKEN at hop 1 (input:in-cop1 has no signal)
   ```
6. **Exit code** 0 only if every discovered path passes every hop — scriptable/CI-usable, not just
   a human-read report.

## Isolation: passive by default, an opt-in stricter mode

The default mode above is purely observational — it never changes any live state, safe to run
against a production console at any time. Its one real limitation: if two tracks send into the
*same* bus simultaneously, a broken track's send is masked by the other track's real signal (the
bus itself still shows non-silent, even though one of its two contributors is actually dead).

An opt-in `--isolate` mode closes that gap: before checking a given path, temporarily mute every
*other* track/send touching the same bus (`mute`/`sends[].on`, already live WS params), check the
path in isolation, then restore every value it changed — logged clearly, and restored even if the
check itself errors partway through. Flagged as opt-in and clearly louder in its own output (it
audibly mutes things, if only for a second) because it touches live state on what could be a real
console someone else is listening to, unlike the default mode.

## Where it lives

A new standalone binary, `src/bin/audio-path-check.rs`, in this same `mxl-test-app` crate — it only
needs to speak the plain JSON WS protocol (`tokio-tungstenite`, one new dependency; `serde_json` is
already in `Cargo.toml`), not any of `mxl-test-app`'s own internal types, so it doesn't need a
`[lib]` target added to the crate. Run as `cargo run --release --bin audio-path-check -- --host
172.30.3.77 --port 3214 [--threshold-db -50] [--isolate]`.

## Explicitly out of scope for a first pass

- The `--isolate` mode's actual implementation (documented above, not built this pass — the
  observational default is the safe, immediately useful piece).
- Verifying *content* (a specific tone/pattern arriving correctly) — only presence/absence of
  signal above the silence threshold, same granularity every meter in this app already reports at.
- A continuous/watch mode (re-running on an interval) — this is a one-shot check tool; wrapping it
  in a loop is a trivial shell-level concern if wanted later, not this tool's own job.
