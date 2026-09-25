#!/usr/bin/env python3
"""Live AMWA BCP-007-03 ("NMOS With MXL") conformance check of a running node.

Reads the node's IS-04 Node API and IS-05 Connection API (v1.2) over HTTP - read-only, GETs only -
and validates every MXL Sender/Receiver (transport urn:x-nmos:transport:mxl) and what belongs to it
against the schemas vendored next to this script (BCP-007-03 + IS-04 v1.3), plus the MUST rules a
schema cannot express. Same yardstick as mxl-bridge's contract tests, for any implementation.

usage: check_node.py http://<node>:<port> [more nodes ...]      (needs python3-jsonschema)
exit status: 0 = all conformant, 1 = findings
"""
import json
import os
import sys
import urllib.error
import urllib.request

import jsonschema

HERE = os.path.dirname(os.path.abspath(__file__))
MXL = "urn:x-nmos:transport:mxl"
# What MXL itself stores (flow_def.json media_type): audio is always float32, video v210.
MXL_MEDIA = {"urn:x-nmos:format:audio": ("audio/float32", 32), "urn:x-nmos:format:video": ("video/v210", None)}


def schema(rel):
    path = os.path.join(HERE, rel)
    with open(path) as f:
        s = json.load(f)
    resolver = jsonschema.RefResolver("file://" + path, s)
    return lambda doc: sorted(f"{e.message} at /{'/'.join(map(str, e.absolute_path))}"
                              for e in jsonschema.Draft4Validator(s, resolver=resolver).iter_errors(doc))


IS04 = {k: schema(f"is-04-v1.3/schemas/{k}.json") for k in ("sender", "receiver", "flow", "source")}
TX_PARAMS = schema("bcp-007-03/schemas/sender_transport_params_mxl.json")
RX_PARAMS = schema("bcp-007-03/schemas/receiver_transport_params_mxl.json")
CONSTRAINTS = schema("bcp-007-03/schemas/constraints-schema-mxl.json")


def get(url):
    try:
        with urllib.request.urlopen(url, timeout=5) as r:
            body = r.read()
            try:
                return r.status, json.loads(body)
            except ValueError:
                return r.status, None  # e.g. a text/plain transport file
    except urllib.error.HTTPError as e:
        return e.code, None
    except (urllib.error.URLError, OSError) as e:
        return 0, None


def check(base):
    base = base.rstrip("/")
    node = f"{base}/x-nmos/node/v1.3"
    conn = f"{base}/x-nmos/connection/v1.2/single"
    findings = []

    def need(ok, what):
        if not ok:
            findings.append(what)

    _, flows = get(f"{node}/flows/")
    _, sources = get(f"{node}/sources/")
    flows = {f["id"]: f for f in flows or []}
    sources = {s["id"]: s for s in sources or []}
    _, senders = get(f"{node}/senders/")
    _, receivers = get(f"{node}/receivers/")

    for tx in [s for s in senders or [] if s.get("transport") == MXL]:
        n = f"Sender '{tx['label']}'"
        findings += [f"{n}: IS-04 {m}" for m in IS04["sender"](tx)]
        need(tx.get("manifest_href") is None, f"{n}: manifest_href MUST be null (is {tx.get('manifest_href')!r})")
        need(tx.get("interface_bindings") == [], f"{n}: interface_bindings MUST be [] (is {tx.get('interface_bindings')})")
        flow = flows.get(tx.get("flow_id"))
        if flow:
            findings += [f"{n}: Flow IS-04 {m}" for m in IS04["flow"](flow)]
            want = MXL_MEDIA.get(flow.get("format"))
            if want and flow.get("media_type") != want[0]:
                findings.append(f"{n}: Flow media_type {flow.get('media_type')} - MXL {flow['format'].split(':')[-1]} flows are {want[0]}")
            if want and want[1] and flow.get("bit_depth") != want[1]:
                findings.append(f"{n}: Flow bit_depth {flow.get('bit_depth')} - MXL audio is {want[1]}-bit float")
            src = sources.get(flow.get("source_id"))
            if src:
                findings += [f"{n}: Source IS-04 {m}" for m in IS04["source"](src)]
        st, _ = get(f"{conn}/senders/{tx['id']}/transportfile")
        need(st == 404, f"{n}: /transportfile MUST return 404 (got {st})")
        for ep in ("active", "staged"):
            st, doc = get(f"{conn}/senders/{tx['id']}/{ep}")
            tp = (doc or {}).get("transport_params")
            if not isinstance(tp, list) or len(tp) != 1:
                findings.append(f"{n}: /{ep} MUST carry exactly one transport parameter set (got {tp})")
                continue
            findings += [f"{n}: /{ep} {m}" for m in TX_PARAMS(tp[0])]
            need("mxl_domain_id" in tp[0] and "mxl_flow_id" in tp[0], f"{n}: /{ep} MUST contain mxl_domain_id and mxl_flow_id (has {sorted(tp[0])})")
        st, c = get(f"{conn}/senders/{tx['id']}/constraints")
        if not isinstance(c, list) or len(c) != 1:
            findings.append(f"{n}: /constraints MUST be one constraint set (got {c})")
        else:
            findings += [f"{n}: /constraints {m}" for m in CONSTRAINTS(c[0])]

    for rx in [r for r in receivers or [] if r.get("transport") == MXL]:
        n = f"Receiver '{rx['label']}'"
        findings += [f"{n}: IS-04 {m}" for m in IS04["receiver"](rx)]
        need(rx.get("interface_bindings") == [], f"{n}: interface_bindings MUST be [] (is {rx.get('interface_bindings')})")
        need(bool(rx.get("caps", {}).get("media_types")), f"{n}: caps.media_types MUST list at least one media type")
        need("constraint_sets" in rx.get("caps", {}), f"{n}: MUST declare BCP-004-01 caps.constraint_sets")
        for ep in ("active", "staged"):
            st, doc = get(f"{conn}/receivers/{rx['id']}/{ep}")
            tp = (doc or {}).get("transport_params")
            if not isinstance(tp, list) or len(tp) != 1:
                findings.append(f"{n}: /{ep} MUST carry exactly one transport parameter set (got {tp})")
                continue
            findings += [f"{n}: /{ep} {m}" for m in RX_PARAMS(tp[0])]
            need("mxl_domain_id" in tp[0] and "mxl_flow_id" in tp[0], f"{n}: /{ep} MUST contain mxl_domain_id and mxl_flow_id (has {sorted(tp[0])})")
        st, c = get(f"{conn}/receivers/{rx['id']}/constraints")
        if not isinstance(c, list) or len(c) != 1:
            findings.append(f"{n}: /constraints MUST be one constraint set (got {c})")
        else:
            findings += [f"{n}: /constraints {m}" for m in CONSTRAINTS(c[0])]

    count = len([s for s in senders or [] if s.get("transport") == MXL]) + len([r for r in receivers or [] if r.get("transport") == MXL])
    return count, findings


def main():
    bad = 0
    for base in sys.argv[1:]:
        count, findings = check(base)
        unique = sorted(set(findings))
        print(f"== {base}: {count} MXL senders/receivers, {len(unique)} distinct findings")
        for f in unique:
            print(f"   - {f}")
        bad += bool(unique)
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
