//! Contract tests: the node's real HTTP surface, driven in-process, checked against the OFFICIAL
//! schemas vendored under `contract/` (IS-04 v1.3.3 resources; AMWA BCP-007-03 v1.0 MXL transport
//! parameters, constraints and domain definition) plus BCP-007-03's MUST rules that a schema
//! cannot express. The same `contract/` files are meant to be checked by every MXL node we build
//! (the macOS driver included), so they all follow the spec rather than each other.
//!
//! No MXL media here (the test state has no libmxl): activations that would open a flow are
//! exercised live, against the real daemon; these tests cover documents and request validation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::{server, NmosState};
use crate::config::test_config;
use crate::daemon_client::{test_sink, test_source};

fn contract_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("contract")
}

/// Resolves the schemas' relative `$ref`s ("resource_core.json", "mxl_uuid.json") from disk.
struct FileRetriever;

impl jsonschema::Retrieve for FileRetriever {
    fn retrieve(&self, uri: &jsonschema::Uri<&str>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let raw = std::fs::read(uri.path().as_str())?;
        Ok(serde_json::from_slice(&raw)?)
    }
}

fn validator(rel: &str) -> jsonschema::Validator {
    let path = contract_dir().join(rel);
    let mut schema: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    // Draft-04 base URI, so relative `$ref`s resolve next to the file.
    schema["id"] = json!(format!("file://{}", path.display()));
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft4)
        .with_retriever(FileRetriever)
        .build(&schema)
        .unwrap_or_else(|e| panic!("schema {rel}: {e}"))
}

#[track_caller]
fn assert_valid(rel: &str, doc: &Value) {
    let v = validator(rel);
    let errors: Vec<String> = v.iter_errors(doc).map(|e| format!("{} at {}", e, e.instance_path)).collect();
    assert!(errors.is_empty(), "{rel} rejects {doc:#}:\n{}", errors.join("\n"));
}

async fn state() -> Arc<NmosState> {
    let s = Arc::new(NmosState::new(test_config(), PathBuf::from("/nonexistent"), crate::mxl_domain::test_domain()));
    s.apply_sink_added_or_changed(&test_sink(3, "DKL OPT2 Audio", (16..32).collect())).await;
    s.apply_source_added_or_changed(&test_source(0, "ALSA Source 0", (0..8).collect())).await;
    s
}

async fn call(s: &Arc<NmosState>, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty))
        .unwrap();
    let res = server::router(s.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn get(s: &Arc<NmosState>, uri: &str) -> Value {
    let (st, v) = call(s, "GET", uri, None).await;
    assert_eq!(st, StatusCode::OK, "GET {uri}");
    v
}

async fn ids(s: &Arc<NmosState>) -> (String, String, String) {
    let sender = get(s, "/x-nmos/node/v1.3/senders/").await[0].clone();
    let receiver = get(s, "/x-nmos/node/v1.3/receivers/").await[0].clone();
    (
        sender["id"].as_str().unwrap().into(),
        sender["flow_id"].as_str().unwrap().into(),
        receiver["id"].as_str().unwrap().into(),
    )
}

const IS04: &str = "is-04-v1.3/schemas";
const BCP: &str = "bcp-007-03/schemas";

#[tokio::test]
async fn is04_resources_validate_and_follow_bcp_007_03() {
    let s = state().await;
    assert_valid(&format!("{IS04}/node.json"), &get(&s, "/x-nmos/node/v1.3/self").await);
    for d in get(&s, "/x-nmos/node/v1.3/devices/").await.as_array().unwrap() {
        assert_valid(&format!("{IS04}/device.json"), d);
    }
    for src in get(&s, "/x-nmos/node/v1.3/sources/").await.as_array().unwrap() {
        assert_valid(&format!("{IS04}/source.json"), src);
    }
    for f in get(&s, "/x-nmos/node/v1.3/flows/").await.as_array().unwrap() {
        assert_valid(&format!("{IS04}/flow.json"), f);
        assert_eq!(f["media_type"], "audio/float32");
    }
    let senders = get(&s, "/x-nmos/node/v1.3/senders/").await;
    let receivers = get(&s, "/x-nmos/node/v1.3/receivers/").await;
    assert!(!senders.as_array().unwrap().is_empty() && !receivers.as_array().unwrap().is_empty());
    for tx in senders.as_array().unwrap() {
        assert_valid(&format!("{IS04}/sender.json"), tx);
        assert_eq!(tx["transport"], "urn:x-nmos:transport:mxl");
        assert!(tx["manifest_href"].is_null(), "BCP-007-03: manifest_href MUST be null");
        assert_eq!(tx["interface_bindings"], json!([]), "BCP-007-03: empty interface_bindings");
    }
    for rx in receivers.as_array().unwrap() {
        assert_valid(&format!("{IS04}/receiver.json"), rx);
        assert_eq!(rx["transport"], "urn:x-nmos:transport:mxl");
        assert_eq!(rx["interface_bindings"], json!([]));
        assert!(!rx["caps"]["media_types"].as_array().unwrap().is_empty());
        let cs = &rx["caps"]["constraint_sets"][0];
        assert_eq!(cs["urn:x-nmos:cap:format:channel_count"]["enum"], json!([8]), "BCP-004-01 caps");
    }
}

#[tokio::test]
async fn is05_documents_validate_against_the_mxl_schemas() {
    let s = state().await;
    let (sender, flow, receiver) = ids(&s).await;
    for v in ["v1.2", "v1.1"] {
        let base = format!("/x-nmos/connection/{v}/single");
        for ep in ["staged", "active"] {
            let doc = get(&s, &format!("{base}/senders/{sender}/{ep}")).await;
            let tp = doc["transport_params"].as_array().unwrap();
            assert_eq!(tp.len(), 1, "exactly one parameter set");
            assert_valid(&format!("{BCP}/sender_transport_params_mxl.json"), &tp[0]);
            assert_eq!(tp[0]["mxl_domain_id"], s.domain.id.to_string());
            assert_eq!(tp[0]["mxl_flow_id"], flow);

            let doc = get(&s, &format!("{base}/receivers/{receiver}/{ep}")).await;
            let tp = doc["transport_params"].as_array().unwrap();
            assert_eq!(tp.len(), 1);
            assert_valid(&format!("{BCP}/receiver_transport_params_mxl.json"), &tp[0]);
            assert_eq!(tp[0], json!({"mxl_domain_id": null, "mxl_flow_id": null}), "unconnected = null");
        }
        for (kind, id) in [("senders", &sender), ("receivers", &receiver)] {
            let c = get(&s, &format!("{base}/{kind}/{id}/constraints")).await;
            let c = c.as_array().unwrap();
            assert_eq!(c.len(), 1, "exactly one constraint set");
            assert_valid(&format!("{BCP}/constraints-schema-mxl.json"), &c[0]);
            assert!(!c[0].to_string().contains("\"auto\""), "auto is never listed in constraints");
        }
        let (st, _) = call(&s, "GET", &format!("{base}/senders/{sender}/transportfile"), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "BCP-007-03: /transportfile MUST 404");
    }
}

#[tokio::test]
async fn is05_rejects_what_cannot_apply_here() {
    let s = state().await;
    let (sender, _flow, receiver) = ids(&s).await;
    let base = "/x-nmos/connection/v1.2/single";
    let other = "e37296f4-a397-414d-b096-121d95fd08a2";
    let cases = [
        (format!("{base}/senders/{sender}/staged"), json!({"transport_params": [{"mxl_flow_id": other}]})),
        (format!("{base}/senders/{sender}/staged"), json!({"transport_params": [{"mxl_domain_id": other}]})),
        (format!("{base}/receivers/{receiver}/staged"), json!({"transport_params": [{"mxl_flow_id": "auto"}]})),
        (format!("{base}/receivers/{receiver}/staged"), json!({"transport_params": [{"mxl_domain_id": other, "mxl_flow_id": other}]})),
        (format!("{base}/receivers/{receiver}/staged"), json!({"transport_params": [{"mxl_flow_id": other}, {"mxl_flow_id": other}]})),
        (format!("{base}/receivers/{receiver}/staged"), json!({"sender_id": sender, "transport_file": {"data": "v=0", "type": "application/sdp"}})),
        (format!("{base}/receivers/{receiver}/staged"), json!({"master_enable": true})),
    ];
    for (uri, body) in cases {
        let (st, err) = call(&s, "PATCH", &uri, Some(body.clone())).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "PATCH {uri} {body} -> {err}");
    }
    // Staging values that ARE acceptable (no activation: nothing to open).
    let (st, doc) = call(&s, "PATCH", &format!("{base}/senders/{sender}/staged"), Some(json!({"transport_params": [{"mxl_domain_id": "auto", "mxl_flow_id": "auto"}]}))).await;
    assert_eq!(st, StatusCode::OK, "{doc}");
    let (st, _) = call(&s, "PATCH", &format!("{base}/receivers/{receiver}/staged"), Some(json!({"master_enable": false, "sender_id": null, "transport_file": {"data": null, "type": null}}))).await;
    assert_eq!(st, StatusCode::OK);
}

#[test]
fn created_domain_definition_matches_the_schema() {
    let dir = std::env::temp_dir().join(format!("mxl-contract-domain-{}", uuid::Uuid::new_v4()));
    let def = crate::mxl_domain::load_or_create(&dir, "lab").unwrap();
    let on_disk: Value = serde_json::from_slice(&std::fs::read(dir.join("domain_def.json")).unwrap()).unwrap();
    assert_valid(&format!("{BCP}/mxl_domain_definition.json"), &on_disk);
    assert_eq!(on_disk["id"], def.id.to_string());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Sanity of the harness itself: the spec's own examples pass the spec's own schemas.
#[test]
fn bcp_examples_pass_their_schemas() {
    let ex = |name: &str| -> Value {
        serde_json::from_slice(&std::fs::read(contract_dir().join("bcp-007-03/examples").join(name)).unwrap()).unwrap()
    };
    assert_valid(&format!("{BCP}/mxl_domain_definition.json"), &ex("domain_def.json"));
    assert_valid(&format!("{BCP}/sender_transport_params_mxl.json"), &ex("sender-transport-parameters-active.json")["transport_params"][0]);
    assert_valid(&format!("{BCP}/receiver_transport_params_mxl.json"), &ex("receiver-transport-parameters-active.json")["transport_params"][0]);
    assert_valid(&format!("{BCP}/constraints-schema-mxl.json"), &ex("transport-parameters-constraints.json")[0]);
    assert_valid(&format!("{IS04}/sender.json"), &ex("mxl-sender.json"));
    assert_valid(&format!("{IS04}/receiver.json"), &ex("mxl-audio-receiver.json"));
    assert_valid(&format!("{IS04}/flow.json"), &ex("mxl-audio-flow.json"));
}

/// The harness must actually reject: via `$ref` into `mxl_uuid.json`, required keys, IS-04 core.
#[test]
fn harness_rejects_invalid_documents() {
    let bad = [
        (format!("{BCP}/sender_transport_params_mxl.json"), json!({"mxl_flow_id": "CD27D5F7-ED49-5112-A4E7-80588A30F3FE"})),
        (format!("{BCP}/receiver_transport_params_mxl.json"), json!({"mxl_flow_id": "auto"})),
        (format!("{BCP}/sender_transport_params_mxl.json"), json!({"destination_ip": "239.1.1.1"})),
        (format!("{BCP}/constraints-schema-mxl.json"), json!({"urn:x-nmos:cap:transport:mxl_domain_id": {"enum": []}})),
        (format!("{BCP}/mxl_domain_definition.json"), json!({"id": "5f0a4c1e-9d3b-4c47-8f5e-2a7c61b0d3a9", "label": "x"})),
        (format!("{IS04}/sender.json"), json!({"id": "not-a-uuid"})),
    ];
    for (schema, doc) in bad {
        assert!(!validator(&schema).is_valid(&doc), "{schema} must reject {doc}");
    }
}
