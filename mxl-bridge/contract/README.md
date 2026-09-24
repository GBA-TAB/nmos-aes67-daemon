# MXL NMOS contract (shared)

Official schemas every MXL node we build is checked against, so implementations agree through the
specification rather than through each other.

| Directory | Source |
|---|---|
| `bcp-007-03/` | AMWA-TV/bcp-007-03 @ `16d66a4` (2026-09-18): `APIs/schemas`, `examples`, LICENSE |
| `is-04-v1.3/` | AMWA-TV/is-04 v1.3.3 (`8e6876d`) `APIs/schemas`, LICENSE (via nmos-testing's cache) |

Consumers: `mxl-bridge/src/nmos/contract_tests.rs` (Rust). The macOS driver can validate its
`nmos_mxl.cpp` output against the same files (e.g. with pboettch/json-schema-validator).
