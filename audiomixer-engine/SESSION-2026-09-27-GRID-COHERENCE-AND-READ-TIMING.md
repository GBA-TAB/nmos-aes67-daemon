# 2026-09-27: registry/grid coherence, input read timing, output patch persistence

Four faults on caspar, all found live after the MXL naming switch
(`mxl-<host>-<domain>-<app>-<resource>`, GBA-TAB/mxl `docs/Naming.md`). Commits on
`mxl-bridge`: `315d06f`, `7ccd3ff`, `d23ccd2`.

## 1. Registry showed ~100-136 input channels, the engine 32

**Symptom.** The registry listed the audiomixer with receivers `gridin01-08` up to
`gridin97-104`, and more over time. The engine's own Node API had 4 (its configured 32 channels).

**Cause.** Registry discovery (`nmos/discovery.rs`) adds one input per MXL sender it can open. It
read only the Query API's first page. nmos-cpp returns 10 results per page by default, and its
default page is the newest one. With more than 10 senders or flows, each 5 s poll saw a different
subset:
- inputs were removed and re-added continuously;
- each re-add reserved a new channel range, so the numbers climbed;
- a removed input's receiver was never deleted from the registry.

**Fix.**
- `get_all`: walk every page, starting at `paging.since=0:0` and following `Link: rel="next"`.
  nmos-cpp caps `paging.limit` at 100.
- `InputGrid::reserve_channel_range_for(key, channels)`: a discovered sender keeps its range for
  the life of the process. New senders are added in label order, so a restart numbers the same set
  the same way.
- A discovery change triggers a registration pass (`notify_changed`). Each pass deletes receivers
  that left the grid (`registered_receivers`).
- At startup the engine first deletes its own node from the registry, which removes everything a
  previous run left under the same, name-derived node id.

**Verified.** Registry and Node API agreed (17 receivers, 136 channels) over 5 samples in 75 s.

**Side effect to know.** For a moment after startup the node is absent from the registry. reMOS's
route restore can fail during that moment ("Connection API URL not found for receiver"), then
succeeds on its next attempt.

## 2. Grid inputs on the same flow: only one played

**Symptom.** Test Tones was routed to grid inputs 01-32 (and discovered once more as its own
input). Only one of the five inputs had signal; which one changed after each restart.

**Cause.** Each period, the engine reads 96 samples per input (2 ms) with a 4 ms timeout, following
the writer's head. Test Tones (GStreamer `audiotestsrc`) commits 1024 samples every 21 ms, so reads
right at the head time out. After a failed read the engine re-anchored the input at the head, then
backed it off for 500 ms. By the retry, that anchor was 500 ms stale. libmxl only lets a reader lag
half the flow's buffer (about 200 ms here), so every retry failed "too late" and re-anchored stale
again, forever. An input played only if its first read happened to succeed.

**Fix (`flow.rs`).**
- `resync_to_head` re-anchors at the next read, after the back-off, not before it.
- Each input keeps a lag behind the writer's head. A read that finds its samples not yet written
  (`Timeout`, `OutOfRangeTooEarly`) grows it: 192, 384, 768, ... up to 0.5 s. Test Tones settled
  at 768 samples (16 ms); 1 ms writers (mxl-bridge) keep the minimum.
- Read failures log the full error chain and the current lag.
- Logging defaults to `info` when `RUST_LOG` is unset. Before, only errors were printed, so these
  failures were invisible.

**Verified.** All five Test Tones inputs carried the tone in every meter sample (75/75, 3 rounds).
Measured separately: Test Tones' head advances in 1024-sample steps, 25-46 ms behind the media
clock; a bridge rx flow advances every 48-96 samples, within 1 ms.

## 3. Mixer output to the bridge silent after a restart

**Symptom.** Tracks and masters had signal, the output grid meters were empty, and the bridge tx
streams reading Grid Out were silent.

**Cause.** `state.json` saved tracks, buses and masters but not the output grid's patch (what
feeds each Grid Out channel). Every restart left all outputs unpatched.

**Fix.** `persistence::capture` writes `"outputs"` (entry id -> patch). `apply_snapshot` restores
it after tracks, buses and masters. Round-trip test `output_patch_round_trips`.

**Verified.** Master 0 -> Grid Out 01-08 and 09-16 ch 1-2 survived a restart without re-applying.
bridge tx00 carried 8 channels and tx01 2 channels on the wire.

## Open

- **IS-05 connections are not persisted.** After a restart every input is empty until reMOS
  restores its routes; without reMOS they stay empty.
- **Signal generator block size.** 1024-sample blocks cost about 16 ms of read lag in every reader.
  48 or 96 samples per buffer in mxl-signal-gen would bring that to the minimum.
- **Discovered inputs are named by channel range** (`gridin33-40`), which follows discovery order.
  Stable within a process and across restarts with the same sender set, but a changed set
  renumbers them.
