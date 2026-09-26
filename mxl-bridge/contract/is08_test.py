#!/usr/bin/env python3
"""IS-08 channel mapping on mxl-bridge's packed flows, checked bit for bit.

Uses soak.py's load for known audio everywhere: two pattern writers feed the tx streams, the relay
loops tx n back into rx Sink n, so every rx stream n carries writer gen_for(n)'s pattern (at a
constant per-stream offset). Then, through the bridge's IS-08 API:

  rx-pack      packed-rx:is08test <- 8 channels from 8 different Sinks, channel order reversed:
               every packed channel must carry exactly its source Sink's channel (pattern check)
  rx-remap     swap two packed channels live: the swap must show
  rx-unmap     clear the mapping: the packed-rx output (and its flow) must disappear
  tx-scatter   a pattern writer feeds packed-tx:is08tx; its channels scattered, permuted, onto
               source-stream:13: every channel of tx stream 13 on the wire must be the right one
  tx-unmap     clear it: tx stream 13 goes silent
  validation   sink-stream -> source-stream (not a packing pair) must be rejected

usage: is08_test.py [--writer-pod sig-gen-audio-0]
"""
import argparse, json, os, struct, subprocess, sys, time, urllib.error, urllib.request

import audiotest as A
import soak as S

MAP = f"{A.NODE}/x-nmos/channelmapping/v1.0"
RX_NAME, TX_NAME = "is08test", "is08tx"
RX_SINKS = [0, 3, 5, 6, 9, 12, 14, 15]   # packed channel k <- Sink RX_SINKS[k] ...
RX_CH = [7, 6, 5, 4, 3, 2, 1, 0]         # ... channel RX_CH[k]
TX_STREAM, TX_SEED = 13, 9013
TX_PERM = [2, 0, 3, 1, 7, 5, 6, 4]       # tx stream 13 channel k <- packed-tx channel TX_PERM[k]

results = []


def check(name, ok, detail):
    results.append((name, ok))
    print(f"  {'PASS' if ok else 'FAIL'}  {name:12s} {detail}", flush=True)


def activate(action):
    body = json.dumps({"activation": {"mode": "activate_immediate"}, "action": action}).encode()
    req = urllib.request.Request(f"{MAP}/map/activations/", data=body, method="POST", headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"null")


def outputs():
    return json.load(urllib.request.urlopen(f"{MAP}/outputs/", timeout=10))


def dump(s, flow, channels, seconds=1):
    r = subprocess.run(["kubectl", "-n", A.NS, "exec", s.writer_pod, "--", s.binary, "audiotest", "dump", s.writer_cfg, flow, str(channels), str(seconds)],
                       capture_output=True, timeout=60)
    out = r.stdout
    while out and not out.startswith(b"{"):
        out = out.partition(b"\n")[2]
    head, _, body = out.partition(b"\n")
    hdr = json.loads(head)
    n = hdr["frames"] * channels
    vals = struct.unpack(f"<{n}i", body[: n * 4])
    return hdr["first_index"], [vals[k:k + channels] for k in range(0, n, channels)]


def find_channel(seed, ch, samples, first_index, window):
    """Pattern index offset (flow index - pattern index) of one channel, checked over its run."""
    live = [(j, v) for j, v in enumerate(samples) if v]
    if len(live) < 100:
        return None
    j0, v0 = live[0]
    for p in range(first_index + j0, first_index + j0 - window, -1):
        if A.pattern(seed, ch, p) == v0 and all(A.pattern(seed, ch, p + (j - j0)) == v for j, v in live[1:200]):
            off = first_index + j0 - p
            bad = sum(A.pattern(seed, ch, first_index + j - off) != v for j, v in live)
            return off, bad
    return None


def rx_channel_ok(s, frames, first_index, mapping):
    """mapping: packed channel -> (sink, channel). Each packed channel must be its Sink's channel."""
    bad = []
    for k, (sink, ch) in enumerate(mapping):
        seed = S.SEEDS[s.gen_for(sink)]
        r = find_channel(seed, ch, [f[k] for f in frames], first_index, A.RATE)
        if r is None or r[1]:
            bad.append((k, sink, ch, r))
    return bad


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--writer-pod", default="sig-gen-audio-0")
    a = ap.parse_args()
    S.KEEP_TX = {1, TX_STREAM}  # tx 13 is driven through IS-08 here, not by a soak writer
    s = S.Soak(os.path.expanduser("~/is08-test"), time.time() + 3600, a.writer_pod)
    tx_gen = None
    try:
        s.setup()
        time.sleep(20)  # past the setup transient: re-pointed Sinks re-anchor their rx indices once
        rx_flow = A.mxl_id(A.bridge_name(f"packedrx-{RX_NAME}"), "flow")
        tx_flow = A.mxl_id(A.bridge_name(f"packedtx-{TX_NAME}"), "flow")
        out_rx = f"packed-rx:{RX_NAME}"

        print("== rx: packing 8 Sinks' channels into one MXL flow")
        st, body = activate({out_rx: {str(k): {"input": f"sink-stream:{RX_SINKS[k]}", "channel_index": RX_CH[k]} for k in range(8)}})
        time.sleep(2)
        first, frames = dump(s, rx_flow, 8)
        bad = rx_channel_ok(s, frames, first, list(zip(RX_SINKS, RX_CH)))
        check("rx-pack", st == 200 and not bad, f"HTTP {st}, {len(frames)} frames x 8 ch, each packed channel = its Sink's channel" if not bad else f"HTTP {st} {body}, wrong: {bad}")

        st, body = activate({out_rx: {"0": {"input": f"sink-stream:{RX_SINKS[1]}", "channel_index": RX_CH[1]},
                                       "1": {"input": f"sink-stream:{RX_SINKS[0]}", "channel_index": RX_CH[0]}}})
        time.sleep(2)
        first, frames = dump(s, rx_flow, 8)
        swapped = [(RX_SINKS[1], RX_CH[1]), (RX_SINKS[0], RX_CH[0])] + list(zip(RX_SINKS, RX_CH))[2:]
        bad = rx_channel_ok(s, frames, first, swapped)
        check("rx-remap", st == 200 and not bad, "channels 0 and 1 swapped live, all 8 verified" if not bad else f"HTTP {st} {body}, wrong: {bad}")

        st, body = activate({out_rx: {str(k): {"input": None, "channel_index": None} for k in range(8)}})
        time.sleep(1)
        gone = out_rx not in outputs()
        check("rx-unmap", st == 200 and gone, f"output {out_rx} {'removed' if gone else 'still present'}")

        print(f"== tx: scattering a packed MXL flow onto tx stream {TX_STREAM}")
        tx_gen = subprocess.Popen(["kubectl", "-n", A.NS, "exec", s.writer_pod, "--", s.binary, "audiotest", "gen", s.writer_cfg, tx_flow, "8", "600",
                                   str(TX_SEED), "48"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        next(l for l in tx_gen.stdout if l.startswith("{"))
        out_tx = f"source-stream:{TX_STREAM}"
        st, body = activate({out_tx: {str(k): {"input": f"packed-tx:{TX_NAME}", "channel_index": TX_PERM[k]} for k in range(8)}})
        time.sleep(5)  # the new packed-tx reader settles its read delay first
        pk = A.capture(f"239.55.5.{5 + TX_STREAM}", 2)
        frames = [(ts + k, f, arr) for ts, _, p, arr in pk for k, f in enumerate(A.l24(p, 8))]
        bad_ch = []
        if frames:
            # one offset (RTP time - pattern index) for all channels; find it on channel 0
            rtp0, f0, arr0 = next(((t, f, arr) for t, f, arr in frames if f[0]), (None, None, None))
            est = arr0 * A.RATE // 10**9 if arr0 else 0  # MXL index when that frame arrived
            p0 = next((p for p in range(est, est - A.RATE // 2, -1)
                       if A.pattern(TX_SEED, TX_PERM[0], p) == f0[0] and A.pattern(TX_SEED, TX_PERM[1], p) == f0[1]), None) if f0 else None
            if p0 is None:
                # diagnose: is there audio at all, and which pattern channel (if any) is where?
                nz = sum(1 for _, f, _ in frames if any(f))
                diag = []
                for k in range(8):
                    v = next((f[k] for _, f, _ in frames if f[k]), None)
                    hit = None
                    if v is not None:
                        for c in range(8):
                            q = next((p for p in range(est + A.RATE, est - 4 * A.RATE, -1) if A.pattern(TX_SEED, c, p) == v), None)
                            if q is not None:
                                hit = (c, round((est - q) / 48, 1))
                                break
                    diag.append((k, v, hit))
                bad_ch = [f"pattern not found: {nz}/{len(frames)} frames non-silent; per channel (value, (pattern ch, ms behind)): {diag}"]
            else:
                for k in range(8):
                    wrong = sum(A.pattern(TX_SEED, TX_PERM[k], p0 + (((t - rtp0) + (1 << 31)) & 0xFFFFFFFF) - (1 << 31)) != f[k] for t, f, _ in frames)
                    if wrong:
                        bad_ch.append((k, TX_PERM[k], wrong))
        check("tx-scatter", st == 200 and frames and not bad_ch,
              f"HTTP {st}, {len(frames)} frames x 8 ch on the wire, channel k = packed channel {TX_PERM}" if not bad_ch else f"HTTP {st} {body}, wrong: {bad_ch}")

        st, body = activate({out_tx: {str(k): {"input": None, "channel_index": None} for k in range(8)}})
        time.sleep(2)
        db, _, n = A.capture(f"239.55.5.{5 + TX_STREAM}", 1), None, None
        silent = all(not any(f) for _, _, p, _ in db for f in A.l24(p, 8))
        check("tx-unmap", st == 200 and db and silent, f"tx stream {TX_STREAM}: {'silent' if silent else 'still carries audio'} ({len(db)} packets)")

        st, body = activate({out_tx: {"0": {"input": "sink-stream:0", "channel_index": 0}}})
        check("validation", st in (400, 409, 422) and "not routable" in json.dumps(body), f"sink-stream -> source-stream: HTTP {st} {json.dumps(body)[:80]}")
    finally:
        activate({f"packed-rx:{RX_NAME}": {str(k): {"input": None, "channel_index": None} for k in range(8)}})
        activate({f"source-stream:{TX_STREAM}": {str(k): {"input": None, "channel_index": None} for k in range(8)}})
        if tx_gen:
            tx_gen.terminate()
        s.teardown()
    failed = [n for n, ok in results if not ok]
    print(f"== {len(results) - len(failed)}/{len(results)} PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
