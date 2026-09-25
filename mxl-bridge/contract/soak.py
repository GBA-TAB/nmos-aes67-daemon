#!/usr/bin/env python3
"""Soak test: all 16 tx and 16 rx streams of the bridge under load for hours, checked bit for bit.

Load (everything stays on this host; nothing new goes on the network):
  tx  two pattern writers in the bridge pod (`audiotest gen`, seed A with 10 ms blocks, seed B with
      1 ms blocks) feed bridge tx n (even n <- A, odd n <- B). tx 1 keeps whatever it is routed to.
  rx  `audiotest relay` re-sends tx n's RTP to 239.56.5.(5+n) with TTL 0 and multicast loopback, so
      the host's own RAVENNA driver receives it; daemon Sink n is pointed there. Round trip:
      pattern -> MXL -> bridge tx -> ALSA -> driver -> RTP -> relay -> driver -> ALSA -> bridge rx -> MXL.

Checks:
  every --probe-secs, alternating on a rotating stream:
    tx probe  2 s of stream n on the wire == the pattern at one constant offset (latency: wire
              arrival vs the pattern's MXL index);
    rx probe  2 s of the bridge's rx MXL flow n == the pattern (full round trip; latency = MXL index
              of the rx flow - the pattern index it carries).
  every 60 s: pod restarts, bridge CPU, bridge WARN/ERROR log lines, the 16 Sinks' flags, PTP
  status, kernel log, relay and writer liveness. Writers, relay and routes are re-established if
  anything restarts, and each such event is logged.

Output: <out>/soak.jsonl (one record per probe/sample), <out>/soak.log (readable), and
<out>/summary.json at the end. Everything changed is restored on exit (also on SIGTERM/SIGINT).

usage: soak.py --until 08:30 [--out ~/soak-<date>] [--probe-secs 150]
"""
import argparse, datetime, json, os, signal, subprocess, sys, threading, time, urllib.request, uuid

import audiotest as A

RELAY_BASE = "239.56.5.5"
RELAY_NAME = "mxl-soak-relay"
SEEDS = {"A": 7001, "B": 7002}
BLOCKS = {"A": 480, "B": 48}
STREAMS = 16
KEEP_TX = {1}  # tx streams whose route the soak leaves alone (content not checked)


def now():
    return datetime.datetime.now().strftime("%H:%M:%S")


class Soak:
    def __init__(self, out, until):
        self.out, self.until = out, until
        os.makedirs(out, exist_ok=True)
        self.jl = open(os.path.join(out, "soak.jsonl"), "a")
        self.log = open(os.path.join(out, "soak.log"), "a")
        self.gens = {}  # name -> (Popen, flow, first_index)
        self.binary = None
        self.pod_uid = None
        self.cpu_prev = None
        self.stats = {"tx_probes": 0, "tx_fail": 0, "rx_probes": 0, "rx_fail": 0, "events": 0,
                      "tx_latency_ms": [], "rx_roundtrip_ms": [], "failures": []}
        self.saved = None
        self.stopping = False

    # ---- output
    def rec(self, kind, **kw):
        kw.update(t=time.time(), kind=kind)
        self.jl.write(json.dumps(kw) + "\n")
        self.jl.flush()

    def say(self, msg):
        line = f"{now()} {msg}"
        print(line, flush=True)
        self.log.write(line + "\n")
        self.log.flush()

    def event(self, what, **kw):
        self.stats["events"] += 1
        self.rec("event", what=what, **kw)
        self.say(f"EVENT {what} {json.dumps(kw) if kw else ''}")

    # ---- cluster/daemon helpers
    def kubectl(self, *args, timeout=30):
        return subprocess.run(["kubectl", "-n", A.NS, *args], capture_output=True, text=True, timeout=timeout)

    def pod_info(self):
        p = json.loads(self.kubectl("get", "pods", "-o", "json").stdout)
        return {i["metadata"]["name"]: (i["metadata"]["uid"], sum(c.get("restartCount", 0) for c in i["status"].get("containerStatuses", [])),
                                         i["status"].get("phase")) for i in p["items"]}

    def bridge_receivers(self):
        return A.http("GET", f"{A.NODE}/x-nmos/node/v1.3/receivers/")

    def rx_flow(self, n, sinks):
        label = sinks[n]["name"]
        sender = next(s for s in A.http("GET", f"{A.NODE}/x-nmos/node/v1.3/senders/") if s["label"] == label)
        return sender

    # ---- load
    def start_gen(self, name):
        flow = str(uuid.uuid4())
        secs = max(60, int(self.until - time.time()) + 900)
        p = subprocess.Popen(["kubectl", "-n", A.NS, "exec", A.POD, "--", self.binary, "audiotest", "gen", A.CONFIG, flow, "8",
                              str(secs), str(SEEDS[name]), str(BLOCKS[name])],
                             stdout=subprocess.PIPE, stderr=open(os.path.join(self.out, f"gen-{name}.err"), "a"), text=True)
        first = json.loads(p.stdout.readline())
        threading.Thread(target=lambda: p.stdout.read(), daemon=True).start()
        self.gens[name] = (p, flow, first["first_index"])
        self.say(f"writer {name}: flow {flow[:8]}, seed {SEEDS[name]}, block {BLOCKS[name]}")

    def gen_for(self, n):
        return "A" if n % 2 == 0 else "B"

    def connect_tx(self):
        recv = self.bridge_receivers()
        for n in range(STREAMS):
            if n in KEEP_TX:
                continue
            rid = A.receiver_for_tx(n, recv)
            flow = self.gens[self.gen_for(n)][1]
            A.http("PATCH", f"{A.NODE}/x-nmos/connection/v1.2/single/receivers/{rid}/staged",
                   {"master_enable": True, "activation": {"mode": "activate_immediate"},
                    "transport_params": [{"mxl_flow_id": flow, "mxl_domain_id": "auto"}]})

    def start_relay(self):
        subprocess.run(["docker", "rm", "-f", RELAY_NAME], capture_output=True)
        args = ["docker", "run", "-d", "--name", RELAY_NAME, "--user", "0", "--net=host", "--cap-add", "NET_RAW", "--cap-add", "NET_ADMIN"]
        args += ["-v", f"{A.HOST_BIN}:/t/mxl-bridge:ro", "--entrypoint", "/t/mxl-bridge", "mxl-bridge:latest",
                 "audiotest", "relay", A.IFACE, "239.55.5.5", RELAY_BASE, str(STREAMS), "0"]
        subprocess.run(args, check=True, capture_output=True)
        self.say("relay started")

    def relay_running(self):
        r = subprocess.run(["docker", "inspect", "-f", "{{.State.Running}}", RELAY_NAME], capture_output=True, text=True)
        return r.stdout.strip() == "true"

    def point_sinks(self):
        base = [int(x) for x in RELAY_BASE.split(".")]
        for n in range(STREAMS):
            s = dict(self.saved["sinks"][n])
            src_sdp = A.http("GET", f"{A.DAEMON}/api/source/sdp/{n}")
            src_sdp = src_sdp.decode() if isinstance(src_sdp, bytes) else src_sdp
            tx_group = next(l.split()[2].split("/")[0] for l in src_sdp.splitlines() if l.startswith("c=IN IP4"))
            relay_group = f"{base[0]}.{base[1]}.{base[2]}.{base[3] + n}"
            body = {k: v for k, v in s.items() if k != "id"}
            body["sdp"] = src_sdp.replace(tx_group, relay_group)
            body["use_sdp"] = True
            A.http("PUT", f"{A.DAEMON}/api/sink/{n}", body)

    def activate_rx_senders(self):
        sinks = {s["id"]: s for s in A.http("GET", f"{A.DAEMON}/api/sinks")["sinks"]}
        for n in range(STREAMS):
            sid = self.rx_flow(n, sinks)["id"]
            A.http("PATCH", f"{A.NODE}/x-nmos/connection/v1.2/single/senders/{sid}/staged",
                   {"master_enable": True, "activation": {"mode": "activate_immediate"}})

    def setup(self):
        sinks = {s["id"]: s for s in A.http("GET", f"{A.DAEMON}/api/sinks")["sinks"]}
        senders = {}
        for n in range(STREAMS):
            sid = self.rx_flow(n, sinks)["id"]
            senders[sid] = A.http("GET", f"{A.NODE}/x-nmos/connection/v1.2/single/senders/{sid}/active")["master_enable"]
        recv = self.bridge_receivers()
        tx_active = {}
        for n in range(STREAMS):
            rid = A.receiver_for_tx(n, recv)
            tx_active[rid] = A.http("GET", f"{A.NODE}/x-nmos/connection/v1.2/single/receivers/{rid}/active")
        self.saved = {"sinks": sinks, "rx_senders": senders, "tx_receivers": tx_active}
        with open(os.path.join(self.out, "saved-state.json"), "w") as f:
            json.dump(self.saved, f, indent=1, default=str)
        self.binary = A.pod_bin()
        self.pod_uid = self.pod_info()[A.POD][0]
        for g in SEEDS:
            self.start_gen(g)
        self.connect_tx()
        self.start_relay()
        self.point_sinks()
        self.activate_rx_senders()
        self.say(f"load up: 16 tx (writers A/B, tx {sorted(KEEP_TX)} kept), 16 rx via relay; until {datetime.datetime.fromtimestamp(self.until):%H:%M}")

    def teardown(self):
        self.say("teardown: restoring routes, Sinks, senders; stopping writers and relay")
        for name, (p, _, _) in self.gens.items():
            p.terminate()
        subprocess.run(["docker", "rm", "-f", RELAY_NAME], capture_output=True)
        if not self.saved:
            return
        for rid, act in self.saved["tx_receivers"].items():
            try:
                if not act.get("master_enable"):
                    A.http("PATCH", f"{A.NODE}/x-nmos/connection/v1.2/single/receivers/{rid}/staged",
                           {"master_enable": False, "activation": {"mode": "activate_immediate"}})
            except Exception as e:
                self.say(f"teardown: tx receiver {rid[:8]}: {e}")
        for n, s in self.saved["sinks"].items():
            try:
                A.http("PUT", f"{A.DAEMON}/api/sink/{n}", {k: v for k, v in s.items() if k != "id"})
            except Exception as e:
                self.say(f"teardown: sink {n}: {e}")
        for sid, en in self.saved["rx_senders"].items():
            try:
                A.http("PATCH", f"{A.NODE}/x-nmos/connection/v1.2/single/senders/{sid}/staged",
                       {"master_enable": en, "activation": {"mode": "activate_immediate"}})
            except Exception as e:
                self.say(f"teardown: sender {sid[:8]}: {e}")

    # ---- repair
    def ensure_load(self):
        info = self.pod_info()
        uid = info.get(A.POD, (None,))[0]
        if uid != self.pod_uid:
            self.event("bridge pod replaced", old=self.pod_uid, new=uid)
            for _ in range(60):
                try:
                    urllib.request.urlopen(f"{A.NODE}/x-nmos/node/v1.3/", timeout=2)
                    break
                except Exception:
                    time.sleep(2)
            self.pod_uid = uid
            self.binary = A.pod_bin()
            for g in SEEDS:
                self.start_gen(g)
            self.connect_tx()
            self.activate_rx_senders()
        for g, (p, _, _) in list(self.gens.items()):
            if p.poll() is not None:
                self.event("writer exited", writer=g, code=p.returncode)
                self.start_gen(g)
                self.connect_tx()
        if not self.relay_running():
            logs = subprocess.run(["docker", "logs", "--tail", "5", RELAY_NAME], capture_output=True, text=True)
            self.event("relay not running", log=(logs.stdout + logs.stderr)[-500:])
            self.start_relay()
        return info

    # ---- probes
    @staticmethod
    def find_index(frame, seed, lo, hi):
        """The pattern index in [lo, hi] whose 8 channels equal `frame`, searching from hi down."""
        for p in range(hi, lo - 1, -1):
            if A.pattern(seed, 0, p) == frame[0] and all(A.pattern(seed, c, p) == frame[c] for c in range(1, 8)):
                return p
        return None

    def probe_tx(self, n):
        g = self.gen_for(n)
        seed = SEEDS[g]
        pk = A.capture(f"239.55.5.{5 + n}", 2)
        drops = getattr(A.capture, "drops", None)
        if not pk:
            return {"verdict": "FAIL: no packets"}
        frames = [(ts, k, f, tai) for ts, _, p, tai in pk for k, f in enumerate(A.l24(p, 8))]
        live = [x for x in frames if any(x[2])]
        if not live:
            return {"verdict": "FAIL: silent"}
        rtp0, k0, f0, tai0 = live[0]
        est = tai0 * A.RATE // 1_000_000_000
        p0 = self.find_index(f0, seed, est - A.RATE // 4, est)
        if p0 is None:
            return {"verdict": "FAIL: pattern not found near the arrival time"}
        bad = 0
        first_bad = None
        lat = []
        for i, (rtp, k, f, tai) in enumerate(live):
            idx = p0 + ((((rtp + k) - (rtp0 + k0)) + (1 << 31)) & 0xFFFFFFFF) - (1 << 31)
            if any(A.pattern(seed, c, idx) != f[c] for c in range(8)):
                bad += 1
                first_bad = first_bad if first_bad is not None else i
            if k == 47 and i % 97 == 0:
                lat.append(tai / 1e9 - idx / A.RATE)
        lost = A.seq_gaps(pk)
        lat.sort()
        med = round(lat[len(lat) // 2] * 1000, 2) if lat else None
        ok = bad == 0 and (lost == 0 or (drops and drops >= lost))
        return {"verdict": "PASS" if ok else f"FAIL: {bad} bad frames (first at {first_bad}), {lost} lost packets",
                "frames": len(live), "lost": lost, "capture_drops": drops, "latency_ms": med,
                "latency_ms_max": round(lat[-1] * 1000, 2) if lat else None}

    def probe_rx(self, n):
        g = self.gen_for(n)
        seed = SEEDS[g]
        sinks = {s["id"]: s for s in A.http("GET", f"{A.DAEMON}/api/sinks")["sinks"]}
        flow = self.rx_flow(n, sinks)["flow_id"]
        r = subprocess.run(["kubectl", "-n", A.NS, "exec", A.POD, "--", self.binary, "audiotest", "dump", A.CONFIG, flow, "8", "2"],
                           capture_output=True, timeout=60)
        head, _, body = r.stdout.partition(b"\n")
        hdr = json.loads(head)
        nbytes = hdr["frames"] * 8 * 4
        import struct
        vals = struct.unpack(f"<{nbytes // 4}i", body[:nbytes])
        frames = [vals[k:k + 8] for k in range(0, len(vals), 8)]
        first = next((i for i, f in enumerate(frames) if any(f)), None)
        if first is None:
            return {"verdict": "FAIL: rx flow silent", "frames": len(frames)}
        idx_first = hdr["first_index"] + first
        p0 = self.find_index(frames[first], seed, idx_first - A.RATE, idx_first)
        if p0 is None:
            return {"verdict": "FAIL: pattern not found in rx flow"}
        bad, first_bad = 0, None
        for j, f in enumerate(frames[first:]):
            if any(A.pattern(seed, c, p0 + j) != f[c] for c in range(8)):
                bad += 1
                first_bad = first_bad if first_bad is not None else j
        rt = round((idx_first - p0) / A.RATE * 1000, 2)
        return {"verdict": "PASS" if bad == 0 else f"FAIL: {bad} bad frames of {len(frames) - first} (first at {first_bad})",
                "frames": len(frames) - first, "silent_lead": first, "roundtrip_ms": rt}

    # ---- periodic sample
    def sample(self, info):
        out = {"pods": {k: v[1] for k, v in info.items()}}
        # bridge process CPU (pid 1 in its container)
        r = self.kubectl("exec", A.POD, "--", "sh", "-c", "cut -d' ' -f14,15 /proc/1/stat; cut -d' ' -f1 /proc/uptime")
        try:
            ticks, up = r.stdout.split("\n")[:2]
            t = sum(int(x) for x in ticks.split())
            u = float(up)
            if self.cpu_prev:
                out["bridge_cpu_pct"] = round((t - self.cpu_prev[0]) / 100 / (u - self.cpu_prev[1]) * 100, 1)
            self.cpu_prev = (t, u)
        except Exception:
            pass
        logs = self.kubectl("logs", A.POD, "--since=61s").stdout.splitlines()
        warn = [l for l in logs if " WARN " in l or " ERROR " in l]
        out["bridge_warn"] = len(warn)
        if warn:
            out["bridge_warn_sample"] = [l[-220:] for l in warn[:3]]
        bad = {}
        for n in range(STREAMS):
            f = A.http("GET", f"{A.DAEMON}/api/sink/status/{n}")["sink_flags"]
            issues = [k for k in ("rtp_seq_id_error", "rtp_ssrc_error", "rtp_payload_type_error", "rtp_sac_error", "some_muted", "all_muted", "muted") if f.get(k)]
            if not f.get("receiving_rtp_packet"):
                issues.append("not_receiving")
            if issues:
                bad[n] = issues
        out["sink_issues"] = bad
        try:
            ptp = A.http("GET", f"{A.DAEMON}/api/ptp/status")
            out["ptp"] = ptp.get("status")
        except Exception as e:
            out["ptp"] = f"error {e}"
        k = subprocess.run(["journalctl", "-k", "--since", "-61s", "--no-pager", "-q"], capture_output=True, text=True).stdout.splitlines()
        noisy = ("AddRTPStream", "RemoveRTPStream", "docker0", "veth", "entered", "br-")
        kern = [l for l in k if not any(x in l for x in noisy)]
        out["kernel_lines"] = len(kern)
        if kern:
            out["kernel_sample"] = [l[-200:] for l in kern[:3]]
        out["load"] = open("/proc/loadavg").read().split()[:3]
        self.rec("sample", **out)
        flag = []
        if bad:
            flag.append(f"sinks {bad}")
        if warn:
            flag.append(f"{len(warn)} bridge WARN/ERROR")
        if kern:
            flag.append(f"{len(kern)} kernel lines: {kern[0][-120:]}")
        if out.get("ptp") not in ("locked",):
            flag.append(f"ptp {out.get('ptp')}")
        self.say(f"sample cpu {out.get('bridge_cpu_pct')}% load {' '.join(out['load'])}" + (" | " + "; ".join(flag) if flag else " | ok"))
        return out

    def run(self, probe_secs):
        self.setup()
        time.sleep(10)
        probe_i = 0
        next_probe = time.time() + 20
        next_sample = time.time()
        prev_restarts = None
        while time.time() < self.until and not self.stopping:
            try:
                if time.time() >= next_sample:
                    info = self.ensure_load()
                    restarts = {k: v[1] for k, v in info.items()}
                    if prev_restarts is not None:
                        for k, v in restarts.items():
                            if v > prev_restarts.get(k, v):
                                self.event("pod restarted", pod=k, restarts=v)
                    prev_restarts = restarts
                    self.sample(info)
                    next_sample += 60
                if time.time() >= next_probe:
                    candidates = [n for n in range(STREAMS) if n not in KEEP_TX]
                    n = candidates[(probe_i // 2) % len(candidates)]
                    kind = "tx" if probe_i % 2 == 0 else "rx"
                    try:
                        res = self.probe_tx(n) if kind == "tx" else self.probe_rx(n)
                    except Exception as e:
                        res = {"verdict": f"FAIL: probe error {e!r}"}
                    self.stats[f"{kind}_probes"] += 1
                    if not res["verdict"].startswith("PASS"):
                        self.stats[f"{kind}_fail"] += 1
                        self.stats["failures"].append({"t": now(), "kind": kind, "stream": n, **res})
                    if res.get("latency_ms") is not None:
                        self.stats["tx_latency_ms"].append(res["latency_ms"])
                    if res.get("roundtrip_ms") is not None:
                        self.stats["rx_roundtrip_ms"].append(res["roundtrip_ms"])
                    self.rec(f"probe_{kind}", stream=n, **res)
                    self.say(f"probe {kind} {n:2d} writer {self.gen_for(n)}: {res['verdict']} " +
                             " ".join(f"{k}={v}" for k, v in res.items() if k != "verdict"))
                    probe_i += 1
                    next_probe += probe_secs
            except Exception as e:
                self.event("loop error", error=repr(e))
            time.sleep(1)

    def summary(self):
        def dist(v):
            if not v:
                return None
            v = sorted(v)
            return {"n": len(v), "min": v[0], "median": v[len(v) // 2], "p99": v[min(len(v) - 1, int(len(v) * 0.99))], "max": v[-1]}
        s = {k: v for k, v in self.stats.items() if not k.endswith("_ms")}
        s["tx_latency_ms"] = dist(self.stats["tx_latency_ms"])
        s["rx_roundtrip_ms"] = dist(self.stats["rx_roundtrip_ms"])
        with open(os.path.join(self.out, "summary.json"), "w") as f:
            json.dump(s, f, indent=1)
        self.say(f"SUMMARY tx {s['tx_probes'] - s['tx_fail']}/{s['tx_probes']} PASS, rx {s['rx_probes'] - s['rx_fail']}/{s['rx_probes']} PASS, "
                 f"{s['events']} events; tx latency {s['tx_latency_ms']}; rx round trip {s['rx_roundtrip_ms']}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--until", required=True, help="HH:MM (next occurrence) or seconds from now (e.g. 600s)")
    ap.add_argument("--out", default=os.path.expanduser(f"~/soak-{datetime.date.today():%Y%m%d}"))
    ap.add_argument("--probe-secs", type=float, default=150)
    a = ap.parse_args()
    if a.until.endswith("s"):
        until = time.time() + float(a.until[:-1])
    else:
        h, m = map(int, a.until.split(":"))
        t = datetime.datetime.now().replace(hour=h, minute=m, second=0, microsecond=0)
        if t <= datetime.datetime.now():
            t += datetime.timedelta(days=1)
        until = t.timestamp()
    s = Soak(a.out, until)

    def stop(*_):
        s.stopping = True
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        s.run(a.probe_secs)
    finally:
        s.teardown()
        s.summary()
    sys.exit(1 if s.stats["tx_fail"] or s.stats["rx_fail"] else 0)


if __name__ == "__main__":
    main()
