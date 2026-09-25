#!/usr/bin/env python3
"""NMOS control test matrix for mxl-bridge, driven through reMOS (the path an operator takes).

Every step is a route change made with reMOS's /api/routes (same ConnectionService as its router
pages), then checked where it matters - on the wire, in the daemon, in the bridge's MXL flows:

  tx-connect     bridge tx N  <- Test Tones (MXL)      -> a 1 kHz tone on tx N's multicast
  tx-reroute     bridge tx N  <- audiomixer Grid Out   -> the tone is gone, other audio instead
  tx-disconnect  bridge tx N  x                        -> silence on the wire
  rx-connect     daemon rx M  <- external 2110 sender  -> the Sink carries that SDP and receives
  rx-disconnect  daemon rx M  x                        -> the Sink is parked and stops receiving
  tx-survives-restart  a reMOS-made tx route is back after a bridge restart (persisted activation)

Uses unused streams by default (tx 4, rx 4) and restores what it changed.

usage: nmos_control.py [--tx 4] [--rx 4] [--rx-sender <NMOS sender id>] [--skip-restart]
env: REMOS (http://localhost:5195), ORCH (http://localhost:8088), plus audiotest.py's.
"""
import argparse, json, math, os, sys, time, urllib.request

import audiotest as A

REMOS = os.environ.get("REMOS", "http://localhost:5195")
ORCH = os.environ.get("ORCH", "http://localhost:8088")
DAEMON_NODE = os.environ.get("DAEMON_NODE", "http://172.30.3.77:3212")
TEST_TONES = "4a7e0ff1-3d87-58c3-9f13-7081598b4c34"
GRID_OUT = "2e427a7f-0f12-598c-9f0a-722d80ff5084"
ERT = "940f6786-e184-5e77-8646-1dddacd87a15"


def remos(method, path, body=None):
    req = urllib.request.Request(REMOS + path, method=method, data=json.dumps(body).encode() if body is not None else None,
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"null")


def connect(sender, receiver):
    st, body = remos("POST", "/api/routes", {"senderId": sender, "receiverId": receiver})
    if st != 200:
        raise RuntimeError(f"reMOS connect failed: HTTP {st} {body}")


def disconnect(receiver):
    st, body = remos("DELETE", f"/api/routes/{receiver}")
    if st not in (200, 204):
        raise RuntimeError(f"reMOS disconnect failed: HTTP {st} {body}")


def wire_audio(group, seconds=1.0):
    """(rms dBFS, dominant frequency Hz) of channel 0 on a stream's multicast."""
    pk = A.capture(group, seconds)
    ch0 = [f[0] for _, _, p, _ in pk for f in A.l24(p, 8)]
    if not ch0:
        return None, None, 0
    rms = math.sqrt(sum(v * v for v in ch0) / len(ch0)) / 8388608.0
    db = 20 * math.log10(rms) if rms > 0 else -200.0
    crossings = sum(1 for a, b in zip(ch0, ch0[1:]) if (a < 0) != (b < 0))
    freq = crossings / 2 / (len(ch0) / A.RATE)
    return round(db, 1), round(freq), len(pk)


def bridge_up():
    try:
        with urllib.request.urlopen(f"{A.NODE}/x-nmos/node/v1.3/", timeout=2):
            return True
    except Exception:
        return False


def bridge_rx_for_tx(n):
    return A.receiver_for_tx(n, A.http("GET", f"{A.NODE}/x-nmos/node/v1.3/receivers/"))


def daemon_rx(m, sinks):
    lo = sinks[m]["map"][0] + 1
    label = f"{sinks[m]['map'][0] + 1}-{sinks[m]['map'][-1] + 1}"
    for r in A.http("GET", f"{DAEMON_NODE}/x-nmos/node/v1.3/receivers/"):
        if r["label"].endswith(f" {label}"):
            return r["id"]
    raise SystemExit(f"no daemon NMOS Receiver for rx {m} (channels {label}, first {lo})")


def sink(m):
    s = next(x for x in A.http("GET", f"{A.DAEMON}/api/sinks")["sinks"] if x["id"] == m)
    st = A.http("GET", f"{A.DAEMON}/api/sink/status/{m}")["sink_flags"]
    group = next((l.split()[2].split("/")[0] for l in s["sdp"].splitlines() if l.startswith("c=IN IP4")), None)
    return s, group, st["receiving_rtp_packet"]


results = []


def quietly(fn, *args):
    try:
        fn(*args)
    except Exception:
        pass


def sender_on_wire(sender):
    """The sender's SDP multicast carries packets (registered 'active' senders can be silent)."""
    if not sender.get("manifest_href"):
        return None, 0
    sdp = urllib.request.urlopen(sender["manifest_href"], timeout=5).read().decode()
    grp = next((l.split()[2].split("/")[0] for l in sdp.splitlines() if l.startswith("c=IN IP4")), None)
    return grp, (len(A.capture(grp, 1)) if grp else 0)


def check(name, ok, detail):
    results.append((name, ok, detail))
    print(f"  {'PASS' if ok else 'FAIL'}  {name:22s} {detail}", flush=True)
    return ok


def wait(pred, timeout=10.0, step=0.5):
    end = time.time() + timeout
    while time.time() < end:
        v = pred()
        if v:
            return v
        time.sleep(step)
    return pred()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tx", type=int, default=4)
    ap.add_argument("--rx", type=int, default=4)
    ap.add_argument("--rx-sender", default=ERT)
    ap.add_argument("--skip-restart", action="store_true")
    a = ap.parse_args()
    group = f"239.55.5.{5 + a.tx}"
    rxid = bridge_rx_for_tx(a.tx)
    print(f"== tx {a.tx}: bridge MXL Receiver {rxid[:8]}, multicast {group}")

    try:
        connect(TEST_TONES, rxid)
        time.sleep(1.5)
        db, hz, n = wire_audio(group)
        check("tx-connect", n > 0 and db > -40 and abs(hz - 1000) < 50, f"Test Tones -> wire: {db} dBFS, {hz} Hz, {n} packets")

        connect(GRID_OUT, rxid)
        time.sleep(1.5)
        db2, hz2, n2 = wire_audio(group)
        check("tx-reroute", n2 > 0 and not (abs(hz2 - 1000) < 50 and abs(db2 - db) < 1), f"Grid Out -> wire: {db2} dBFS, {hz2} Hz (tone gone)")

        if not a.skip_restart:
            connect(TEST_TONES, rxid)
            time.sleep(1)
            # The orchestrator watches the rollout (up to 30 s) before answering.
            req = urllib.request.Request(f"{ORCH}/api/instances/mxl-bridge-1/restart", method="POST")
            with urllib.request.urlopen(req, timeout=90) as r:
                st = json.load(r)
            wait(lambda: bridge_up(), timeout=30)
            time.sleep(3)
            db3, hz3, n3 = wire_audio(group)
            check("tx-survives-restart", n3 > 0 and abs(hz3 - 1000) < 50, f"after bridge restart ({st.get('rollout', {}).get('outcome')}): {db3} dBFS, {hz3} Hz")

        disconnect(rxid)
        time.sleep(1.5)
        db4, hz4, n4 = wire_audio(group)
        check("tx-disconnect", n4 > 0 and db4 < -100, f"-> wire: {db4} dBFS ({n4} packets, stream keeps running)")
    finally:
        quietly(disconnect, rxid)

    sinks = {s["id"]: s for s in A.http("GET", f"{A.DAEMON}/api/sinks")["sinks"]}
    drx = daemon_rx(a.rx, sinks)
    sender = A.http("GET", f"http://172.30.3.201/x-nmos/query/v1.3/senders/{a.rx_sender}")
    print(f"== rx {a.rx}: daemon NMOS Receiver {drx[:8]} <- {sender['label']!r}")
    grp0, live = sender_on_wire(sender)
    if not live:
        print(f"  SKIP  rx-connect/disconnect  {sender['label']!r} sends nothing on {grp0} - give a transmitting NMOS 2110 sender with --rx-sender")
        failed = [r for r in results if not r[1]]
        print(f"== {len(results) - len(failed)}/{len(results)} PASS (rx skipped)")
        sys.exit(1 if failed else 0)
    try:
        connect(a.rx_sender, drx)
        # Joining the group and locking onto the stream can take several seconds.
        s, grp, receiving = wait(lambda: (lambda r: r if r[2] else None)(sink(a.rx)), timeout=20) or sink(a.rx)
        check("rx-connect", receiving and grp != "239.255.255.1", f"Sink {a.rx} on {grp}, receiving={receiving}")
        if receiving:
            # The audio itself: wire vs the bridge's MXL flow for this stream, bit for bit.
            r = A.test_rx(a.rx, 3, A.pod_bin())
            ok = r["verdict"].startswith("PASS")
            silent = "silent" in r["verdict"]
            check("rx-audio", ok or silent, (f"bit-exact, {r.get('compared')} frames, wire -> MXL {r.get('latency_ms_median')} ms" if ok
                                               else "sender is silent - connection verified, content not comparable" if silent else r["verdict"]))

        disconnect(drx)
        s2, grp2, receiving2 = wait(lambda: (lambda r: r if not r[2] else None)(sink(a.rx)), timeout=10) or sink(a.rx)
        parked = grp2 == "239.255.255.1"
        check("rx-disconnect", parked and not receiving2, f"Sink {a.rx} on {grp2} (parked={parked}), receiving={receiving2}")
    finally:
        quietly(disconnect, drx)

    failed = [r for r in results if not r[1]]
    print(f"== {len(results) - len(failed)}/{len(results)} PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
