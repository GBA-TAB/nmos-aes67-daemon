#!/usr/bin/env python3
"""End-to-end audio test of mxl-bridge: bit-exactness and latency, measured on the wire.

tx (MXL -> bridge -> ALSA -> RAVENNA -> ST 2110-30):
  a deterministic pattern is written into a fresh MXL flow (`mxl-bridge audiotest gen` in the bridge
  pod), the bridge's MXL Receiver for the stream is connected to it (IS-05, mxl_flow_id), and the
  stream's RTP is captured on the media NIC (`audiotest capture`, AF_PACKET - the card's own
  transmissions are visible there). Every captured sample must equal pattern(seed, ch, index) for
  one constant offset between RTP time and MXL index: that offset is the latency.

rx (ST 2110-30 -> RAVENNA -> ALSA -> bridge -> MXL):
  an external sender (e.g. a Mac's RAVENNA stream) feeds a bridge rx stream; its RTP is captured on
  the NIC while the bridge's MXL flow for that stream is dumped (`audiotest dump`). The two must match
  sample for sample at one constant offset (MXL index - RTP time): bit-exact, and that offset is the
  latency. Needs non-silent, non-periodic audio (noise or music; a pure tone is ambiguous).

Both directions align by absolute time: RTP timestamps (mediaclk:direct=0) and MXL indices are the
same PTP/TAI sample count (RTP modulo 2^32).

usage:
  audiotest.py tx [--streams 4-15] [--seconds 4]
  audiotest.py rx --stream N [--seconds 4]
env: BRIDGE_POD (mxl-bridge-1-0), BRIDGE_NODE (http://172.30.3.214:3213), DAEMON (http://localhost:8080),
     IFACE (enp4s0), BRIDGE_BIN (host-built mxl-bridge to copy into the pod; default: the pod's own)
"""
import argparse, json, os, struct, subprocess, sys, tempfile, time, urllib.request, uuid

POD = os.environ.get("BRIDGE_POD", "mxl-bridge-1-0")
NS = os.environ.get("NAMESPACE", "mxl-orchestrator")
NODE = os.environ.get("BRIDGE_NODE", "http://172.30.3.214:3213")
DAEMON = os.environ.get("DAEMON", "http://localhost:8080")
IFACE = os.environ.get("IFACE", "enp4s0")
HOST_BIN = os.environ.get("BRIDGE_BIN", os.path.expanduser("~/DEV/nmos/aes67-linux-daemon/mxl-bridge/target/release/mxl-bridge"))
CONFIG = "/config/config.json"
RATE = 48000

M64 = (1 << 64) - 1


def splitmix64(z):
    z = (z + 0x9E3779B97F4A7C15) & M64
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M64
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M64
    return z ^ (z >> 31)


def pattern(seed, ch, index):
    """Same as audiotest.rs::pattern (pinned values checked below)."""
    return (splitmix64(seed ^ (ch << 48) ^ index) >> 43) - (1 << 20)


assert pattern(1, 0, 0) == 139589 and pattern(1, 7, 123456789) == -175737, "pattern drifted from audiotest.rs"


def http(method, url, body=None):
    req = urllib.request.Request(url, method=method, data=json.dumps(body).encode() if body is not None else None,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.load(r) if r.headers.get("content-type", "").startswith("application/json") else r.read()


def pod_bin():
    """The pod's own binary unless a host build is given (lets the tool be tested before an image rebuild)."""
    if os.path.exists(HOST_BIN):
        subprocess.run(["kubectl", "-n", NS, "cp", HOST_BIN, f"{POD}:/tmp/mxl-bridge-audiotest"], check=True)
        return "/tmp/mxl-bridge-audiotest"
    return "/app/mxl-bridge"


def capture(group, seconds):
    """RTP packets for group:5004 on the media NIC: list of (ts, seq, payload)."""
    image = "mxl-bridge:latest"
    args = ["docker", "run", "--rm", "--user", "0", "--net=host", "--cap-add", "NET_RAW", "--cap-add", "NET_ADMIN"]
    if os.path.exists(HOST_BIN):
        args += ["-v", f"{HOST_BIN}:/t/mxl-bridge:ro", "--entrypoint", "/t/mxl-bridge"]
    else:
        args += ["--entrypoint", "/app/mxl-bridge"]
    proc = subprocess.run(args + [image, "audiotest", "capture", IFACE, group, "5004", str(seconds)], capture_output=True, check=True)
    out = proc.stdout
    capture.drops = int(proc.stderr.decode().rsplit("capture_drops=", 1)[-1].split()[0]) if b"capture_drops=" in proc.stderr else None
    pk, i = [], 0
    while i + 16 <= len(out):
        tai, ts, seq, n = struct.unpack_from("<QIHH", out, i)
        pk.append((ts, seq, out[i + 16:i + 16 + n], tai))
        i += 16 + n
    return pk


def l24(payload, channels):
    """Interleaved big-endian 24-bit -> list of frames (lists of ints)."""
    s = [int.from_bytes(payload[j:j + 3], "big", signed=True) for j in range(0, len(payload) - len(payload) % 3, 3)]
    return [s[k:k + channels] for k in range(0, len(s) - len(s) % channels, channels)]


def seq_gaps(pk):
    return sum(((b[1] - a[1]) & 0xFFFF) - 1 for a, b in zip(pk, pk[1:]) if ((b[1] - a[1]) & 0xFFFF) != 1)


def near64(ref, low32):
    """The 64-bit index closest to `ref` whose low 32 bits are `low32`."""
    d = (low32 - ref) & 0xFFFFFFFF
    if d >= 1 << 31:
        d -= 1 << 32
    return ref + d


def receiver_for_tx(n, receivers):
    names = {f"Bridge TX {n + 1}", f"ALSA Source {n}"}
    for r in receivers:
        if r["label"] in names:
            return r["id"]
    raise SystemExit(f"no bridge MXL Receiver for tx {n}")


def test_tx(n, seconds, binary, receivers, seed):
    group = f"239.55.5.{5 + n}"
    rx_id = receiver_for_tx(n, receivers)
    flow = str(uuid.uuid4())
    gen = subprocess.Popen(["kubectl", "-n", NS, "exec", POD, "--", binary, "audiotest", "gen", CONFIG, flow, "8", str(seconds + 6), str(seed)],
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    start = json.loads(gen.stdout.readline())
    staged = f"{NODE}/x-nmos/connection/v1.2/single/receivers/{rx_id}/staged"
    try:
        http("PATCH", staged, {"master_enable": True, "activation": {"mode": "activate_immediate"},
                                "transport_params": [{"mxl_flow_id": flow, "mxl_domain_id": "auto"}]})
        time.sleep(1.5)
        pk = capture(group, seconds)
    finally:
        http("PATCH", staged, {"master_enable": False, "activation": {"mode": "activate_immediate"}})
        gen.terminate()
    return analyse_tx(n, pk, start["first_index"], seed, int((seconds + 6) * RATE))


def analyse_tx(n, pk, first_index, seed, written):
    """Align by content (the RTP clock's epoch may differ from TAI - the lab grandmaster's does),
    check every sample, and time each packet against the TAI clock MXL indices count."""
    res = {"stream": f"tx {n}", "packets": len(pk), "lost": seq_gaps(pk), "capture_drops": capture.drops}
    frames = [(ts + k, f, tai, k) for ts, _, p, tai in pk for k, f in enumerate(l24(p, 8))]
    res["frames"] = len(frames)
    live = [x for x in frames if any(x[1])]
    if not live:
        res["verdict"] = "FAIL: silence only"
        return res
    # 21-bit values repeat inside a multi-second window: keep every index per value and accept the
    # one where all 8 channels agree (a single-channel lookup picks wrong indices by collision).
    where = {}
    for i in range(first_index, first_index + written):
        where.setdefault(pattern(seed, 0, i), []).append(i)
    rtp0, f0 = live[0][0], live[0][1]
    idx0 = next((i for i in where.get(f0[0], []) if all(pattern(seed, c, i) == f0[c] for c in range(8))), None)
    if idx0 is None:
        res["verdict"] = "FAIL: pattern not found (content differs from what was written)"
        return res
    offset = (rtp0 - idx0) & 0xFFFFFFFF          # RTP time - MXL index (includes any clock-epoch offset)
    bad = 0
    for rtp, f, _, _ in live:
        idx = idx0 + (((rtp - rtp0) + (1 << 31)) & 0xFFFFFFFF) - (1 << 31)
        bad += any(pattern(seed, c, idx) != f[c] for c in range(8))
    # Latency: when the packet was on the wire (TAI) vs the MXL time of its last sample.
    lat = []
    for rtp, f, tai, k in live[:: max(1, len(live) // 2000)]:
        idx = idx0 + (((rtp - rtp0) + (1 << 31)) & 0xFFFFFFFF) - (1 << 31)
        last_of_packet = idx + (47 - k)
        lat.append(tai / 1e9 - last_of_packet / RATE)
    lat.sort()
    res.update(checked=len(live), mismatched=bad, rtp_minus_mxl_samples=offset if offset < 1 << 31 else offset - (1 << 32),
               latency_ms_min=round(lat[0] * 1000, 3), latency_ms_median=round(lat[len(lat) // 2] * 1000, 3),
               latency_ms_max=round(lat[-1] * 1000, 3), silent_frames_before=frames.index(live[0]))
    res["verdict"] = verdict(bad == 0, res["lost"], res["capture_drops"])
    return res


def verdict(exact, lost, capture_drops):
    """Gaps the capture socket itself dropped are not the stream's fault."""
    if not exact:
        return "FAIL"
    if lost == 0:
        return "PASS"
    return "PASS (capture dropped packets, stream intact)" if capture_drops and capture_drops >= lost else "FAIL"


def test_rx(n, seconds, binary):
    sinks = {s["id"]: s for s in http("GET", f"{DAEMON}/api/sinks")["sinks"]}
    sdp = sinks[n]["sdp"]
    group = next(l.split()[2].split("/")[0] for l in sdp.splitlines() if l.startswith("c=IN IP4"))
    channels = len(sinks[n]["map"])
    senders = http("GET", f"{NODE}/x-nmos/node/v1.3/senders/")
    flows = {f["id"]: f for f in http("GET", f"{NODE}/x-nmos/node/v1.3/flows/")}
    label = sinks[n]["name"]
    sender = next((s for s in senders if s["label"] == label), None)
    if not sender:
        raise SystemExit(f"no bridge MXL Sender labelled {label!r}")
    flow = sender["flow_id"]
    dump = subprocess.Popen(["kubectl", "-n", NS, "exec", POD, "--", binary, "audiotest", "dump", CONFIG, flow, str(channels), str(seconds + 1)],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    pk = capture(group, seconds + 1)
    raw = dump.communicate()[0]
    head, _, body = raw.partition(b"\n")
    hdr = json.loads(head)
    mxl = struct.unpack(f"<{len(body) // 4}i", body[: len(body) // 4 * 4])
    mframes = [list(mxl[k:k + channels]) for k in range(0, len(mxl), channels)]
    return analyse_rx(n, group, pk, hdr["first_index"], mframes, channels)


def analyse_rx(n, group, pk, first_index, mframes, channels):
    res = {"stream": f"rx {n}", "group": group, "packets": len(pk), "lost": seq_gaps(pk), "capture_drops": capture.drops, "mxl_frames": len(mframes)}
    wire = {}
    for ts, _, p, _ in pk:
        for k, f in enumerate(l24(p, channels)):
            wire[near64(first_index, ts + k)] = f
    if not wire or not any(any(f) for f in wire.values()):
        res["verdict"] = "FAIL: sender is silent (play noise or music on it)"
        return res
    # Offset L = MXL index - RTP time; find it on a stretch of non-silent MXL audio.
    probe = next((k for k in range(len(mframes) - 64) if all(any(mframes[k + j]) for j in range(64))), None)
    if probe is None:
        res["verdict"] = "FAIL: MXL flow is silent - is the stream connected?"
        return res
    target = mframes[probe:probe + 64]
    idx = first_index + probe
    lat = next((L for L in range(0, 2 * RATE) if all(wire.get(idx - L + j) == target[j] for j in range(64))), None)
    if lat is None:
        res["verdict"] = "FAIL: MXL content not found on the wire (not bit-exact, or different stream)"
        return res
    compared = bad = 0
    for k, f in enumerate(mframes):
        w = wire.get(first_index + k - lat)
        if w is not None:
            compared += 1
            bad += w != f
    res.update(latency_samples=lat, latency_ms=round(lat / RATE * 1000, 3), compared=compared, mismatched=bad)
    res["verdict"] = verdict(bad == 0 and compared > RATE, res["lost"], res["capture_drops"])
    return res


def parse_range(s):
    a, _, b = s.partition("-")
    return list(range(int(a), int(b or a) + 1))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("direction", choices=["tx", "rx"])
    ap.add_argument("--streams", default="4-15", help="tx streams to test (default 4-15, the unused ones)")
    ap.add_argument("--stream", type=int, help="rx stream to test")
    ap.add_argument("--seconds", type=float, default=4)
    a = ap.parse_args()
    binary = pod_bin()
    results = []
    if a.direction == "tx":
        receivers = http("GET", f"{NODE}/x-nmos/node/v1.3/receivers/")
        for n in parse_range(a.streams):
            r = test_tx(n, a.seconds, binary, receivers, seed=1000 + n)
            results.append(r)
            print(json.dumps(r), flush=True)
    else:
        if a.stream is None:
            ap.error("rx needs --stream")
        r = test_rx(a.stream, a.seconds, binary)
        results.append(r)
        print(json.dumps(r), flush=True)
    failed = [r for r in results if not r["verdict"].startswith("PASS")]
    print(f"== {len(results) - len(failed)}/{len(results)} PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
