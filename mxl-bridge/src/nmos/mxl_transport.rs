//! IS-05 transport parameters for MXL, per AMWA BCP-007-03 v1.0 §IS-05:
//! - `mxl_domain_id` and `mxl_flow_id` are present in `active`, `staged` and `constraints`, as
//!   exactly ONE parameter set (and one constraint set);
//! - an undetermined value is `null`; `"auto"` may be staged and resolves to this node's own value,
//!   but is never listed in constraints; a Receiver's `mxl_flow_id` never accepts `"auto"`;
//! - a request whose values are invalid, or cannot be applied here (another MXL Domain, another
//!   Flow than the Sender writes), is rejected (400).
//! Schemas: `contract/bcp-007-03/schemas/{sender,receiver}_transport_params_mxl.json`,
//! `constraints-schema-mxl.json`.

use serde_json::{json, Value};

use crate::mxl_domain::is_mxl_uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Sender,
    Receiver,
}

/// One parameter as staged by a controller: absent (keep), `null`, `"auto"` or a UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Param {
    Null,
    Auto,
    Id(String),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Staged {
    pub domain: Option<Param>,
    pub flow: Option<Param>,
}

/// `[{"mxl_domain_id": .., "mxl_flow_id": ..}]` for `active` / `staged`.
pub fn params(domain_id: Option<&str>, flow_id: Option<&str>) -> Value {
    json!([{ "mxl_domain_id": domain_id, "mxl_flow_id": flow_id }])
}

/// One constraint set. The Sender writes exactly one Flow in exactly one Domain, so both are
/// enumerated; a Receiver can read any Flow of its Domain (`{}` = unconstrained).
pub fn constraints(role: Role, domain_id: &str, sender_flow_id: Option<&str>) -> Value {
    let flow = match (role, sender_flow_id) {
        (Role::Sender, Some(f)) => json!({ "enum": [f] }),
        _ => json!({}),
    };
    json!([{ "mxl_domain_id": { "enum": [domain_id] }, "mxl_flow_id": flow }])
}

fn param(role: Role, name: &str, v: &Value) -> Result<Param, String> {
    match v {
        Value::Null => Ok(Param::Null),
        Value::String(s) if s == "auto" => {
            if role == Role::Receiver && name == "mxl_flow_id" {
                Err("a Receiver's mxl_flow_id does not accept \"auto\"".into())
            } else {
                Ok(Param::Auto)
            }
        }
        Value::String(s) if is_mxl_uuid(s) => Ok(Param::Id(s.clone())),
        other => Err(format!("{name}: {other} is neither null, \"auto\" nor a lowercase MXL UUID")),
    }
}

/// Parses and checks `transport_params` of a PATCH body against this node.
/// `own_flow_id`: the Flow a Sender writes (Receivers pass `None`).
pub fn parse_staged(role: Role, body: &Value, own_domain_id: &str, own_flow_id: Option<&str>) -> Result<Staged, String> {
    let Some(tp) = body.get("transport_params") else { return Ok(Staged::default()) };
    let arr = tp.as_array().ok_or("transport_params must be an array")?;
    let [set] = arr.as_slice() else {
        return Err(format!("MXL uses exactly one set of transport parameters, got {}", arr.len()));
    };
    let obj = set.as_object().ok_or("transport_params[0] must be an object")?;
    let mut staged = Staged::default();
    for (k, v) in obj {
        match k.as_str() {
            "mxl_domain_id" => staged.domain = Some(param(role, k, v)?),
            "mxl_flow_id" => staged.flow = Some(param(role, k, v)?),
            k if k.starts_with("ext_") => {}
            k => return Err(format!("unknown MXL transport parameter '{k}'")),
        }
    }
    if let Some(Param::Id(d)) = &staged.domain {
        if d != own_domain_id {
            return Err(format!("MXL Domain {d} is not available on this node (only {own_domain_id})"));
        }
    }
    if let (Role::Sender, Some(Param::Id(f)), Some(own)) = (role, &staged.flow, own_flow_id) {
        if f != own {
            return Err(format!("this Sender writes MXL Flow {own}, not {f}"));
        }
    }
    Ok(staged)
}

/// BCP-007-03: a Receiver request MUST NOT include a transport file; omitted or both fields null
/// is accepted.
pub fn check_no_transport_file(body: &Value) -> Result<(), String> {
    match body.get("transport_file") {
        None | Some(Value::Null) => Ok(()),
        Some(tf) if tf.get("data").is_none_or(Value::is_null) && tf.get("type").is_none_or(Value::is_null) => Ok(()),
        Some(_) => Err("MXL Receivers take no transport_file (use transport_params.mxl_flow_id)".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOM: &str = "5f0a4c1e-9d3b-4c47-8f5e-2a7c61b0d3a9";
    const FLOW: &str = "cd27d5f7-ed49-5112-a4e7-80588a30f3fe";
    const OTHER: &str = "e37296f4-a397-514d-b096-121d95fd08a2";

    fn body(p: Value) -> Value {
        json!({ "transport_params": [p] })
    }

    #[test]
    fn sender_accepts_null_auto_and_its_own_ids_only() {
        let s = parse_staged(Role::Sender, &body(json!({"mxl_domain_id": "auto", "mxl_flow_id": FLOW})), DOM, Some(FLOW)).unwrap();
        assert_eq!((s.domain, s.flow), (Some(Param::Auto), Some(Param::Id(FLOW.into()))));
        assert!(parse_staged(Role::Sender, &body(json!({"mxl_domain_id": null, "mxl_flow_id": "auto"})), DOM, Some(FLOW)).is_ok());
        assert!(parse_staged(Role::Sender, &body(json!({"mxl_flow_id": OTHER})), DOM, Some(FLOW)).is_err());
        assert!(parse_staged(Role::Sender, &body(json!({"mxl_domain_id": OTHER})), DOM, Some(FLOW)).is_err());
    }

    #[test]
    fn receiver_flow_id_never_auto() {
        assert!(parse_staged(Role::Receiver, &body(json!({"mxl_flow_id": "auto"})), DOM, None).is_err());
        let s = parse_staged(Role::Receiver, &body(json!({"mxl_domain_id": "auto", "mxl_flow_id": OTHER})), DOM, None).unwrap();
        assert_eq!(s.flow, Some(Param::Id(OTHER.into())));
    }

    #[test]
    fn exactly_one_set_and_no_foreign_parameters() {
        assert!(parse_staged(Role::Receiver, &json!({"transport_params": []}), DOM, None).is_err());
        assert!(parse_staged(Role::Receiver, &json!({"transport_params": [{}, {}]}), DOM, None).is_err());
        assert!(parse_staged(Role::Receiver, &body(json!({"destination_ip": "239.1.1.1"})), DOM, None).is_err());
        assert!(parse_staged(Role::Receiver, &body(json!({"mxl_flow_id": "CD27D5F7-ED49-5112-A4E7-80588A30F3FE"})), DOM, None).is_err());
        assert!(parse_staged(Role::Receiver, &body(json!({"ext_vendor": 1})), DOM, None).is_ok());
        assert_eq!(parse_staged(Role::Receiver, &json!({"master_enable": true}), DOM, None).unwrap(), Staged::default());
    }

    #[test]
    fn transport_file_rules() {
        assert!(check_no_transport_file(&json!({})).is_ok());
        assert!(check_no_transport_file(&json!({"transport_file": {"data": null, "type": null}})).is_ok());
        assert!(check_no_transport_file(&json!({"transport_file": {"data": "v=0", "type": "application/sdp"}})).is_err());
    }
}
