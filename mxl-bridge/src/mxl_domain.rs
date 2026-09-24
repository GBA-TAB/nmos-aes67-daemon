//! MXL Domain identity per AMWA BCP-007-03 v1.0 ("NMOS With MXL", §MXL Domain definition):
//! every MXL Domain holds a `domain_def.json` in its host directory, and its `id` is what IS-05
//! `mxl_domain_id` refers to. Schema: `contract/bcp-007-03/schemas/mxl_domain_definition.json`.
//!
//! The MXL SDK itself does not create this file. If the configured domain has none, mxl-bridge
//! writes one (random UUID), so the domain has an identity controllers can use; an existing file
//! (written by an orchestrator or another media function) is always respected, never rewritten.
//! Creation is race-safe against other processes doing the same: write a temp file, then
//! `hard_link` it into place, which fails if someone else won - and then their file is read.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context};

pub const DEFINITION_FILE: &str = "domain_def.json";

#[derive(Debug, Clone, PartialEq)]
pub struct DomainDef {
    pub id: uuid::Uuid,
    pub label: String,
    pub description: String,
    pub tags: BTreeMap<String, Vec<String>>,
}

/// `mxl_uuid.json`: lowercase canonical UUID, version nibble 1-f, RFC 4122/9562 variant.
pub fn is_mxl_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, c) in b.iter().enumerate() {
        let ok = match i {
            8 | 13 | 18 | 23 => *c == b'-',
            14 => matches!(c, b'1'..=b'9' | b'a'..=b'f'),
            19 => matches!(c, b'8' | b'9' | b'a' | b'b'),
            _ => matches!(c, b'0'..=b'9' | b'a'..=b'f'),
        };
        if !ok {
            return false;
        }
    }
    true
}

impl DomainDef {
    pub fn parse(v: &serde_json::Value) -> anyhow::Result<Self> {
        let obj = v.as_object().context("domain definition must be a JSON object")?;
        let id = obj.get("id").and_then(|x| x.as_str()).context("domain definition needs a string `id`")?;
        if !is_mxl_uuid(id) {
            bail!("domain id '{id}' is not a lowercase canonical UUID");
        }
        let text = |k: &str| -> anyhow::Result<String> {
            obj.get(k).and_then(|x| x.as_str()).map(str::to_string).with_context(|| format!("domain definition needs a string `{k}`"))
        };
        let mut tags = BTreeMap::new();
        for (k, vals) in obj.get("tags").and_then(|x| x.as_object()).context("domain definition needs a `tags` object")? {
            let vals = vals.as_array().with_context(|| format!("tag '{k}' must be an array of strings"))?;
            let vals = vals
                .iter()
                .map(|s| s.as_str().map(str::to_string).with_context(|| format!("tag '{k}' must be an array of strings")))
                .collect::<anyhow::Result<Vec<_>>>()?;
            tags.insert(k.clone(), vals);
        }
        Ok(DomainDef { id: uuid::Uuid::parse_str(id)?, label: text("label")?, description: text("description")?, tags })
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id.to_string(),
            "label": self.label,
            "description": self.description,
            "tags": self.tags,
        })
    }
}

/// Reads `<domain_dir>/domain_def.json`, creating it first if it does not exist.
pub fn load_or_create(domain_dir: &Path, label: &str) -> anyhow::Result<DomainDef> {
    let path = domain_dir.join(DEFINITION_FILE);
    if !path.exists() {
        std::fs::create_dir_all(domain_dir).with_context(|| format!("creating MXL domain {}", domain_dir.display()))?;
        let def = DomainDef {
            id: uuid::Uuid::new_v4(),
            label: label.to_string(),
            description: format!("MXL domain at {} (definition created by mxl-bridge)", domain_dir.display()),
            tags: BTreeMap::new(),
        };
        let tmp = domain_dir.join(format!(".{DEFINITION_FILE}.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(&def.to_json())?)?;
        let linked = std::fs::hard_link(&tmp, &path);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => tracing::info!(path = %path.display(), id = %def.id, "created MXL domain definition"),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {} // another process won: read theirs
            Err(e) => return Err(e).with_context(|| format!("writing {}", path.display())),
        }
    }
    let raw = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let v: serde_json::Value = serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
    DomainDef::parse(&v).with_context(|| format!("invalid {}", path.display()))
}

/// Fixed identity for unit tests that build an `NmosState` without touching the filesystem.
#[cfg(test)]
pub fn test_domain() -> DomainDef {
    DomainDef {
        id: uuid::Uuid::parse_str("5f0a4c1e-9d3b-4c47-8f5e-2a7c61b0d3a9").unwrap(),
        label: "test domain".into(),
        description: String::new(),
        tags: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_rule_matches_the_bcp_schema() {
        assert!(is_mxl_uuid("5f0a4c1e-9d3b-4c47-8f5e-2a7c61b0d3a9"));
        assert!(!is_mxl_uuid("5F0A4C1E-9D3B-4C47-8F5E-2A7C61B0D3A9"), "uppercase");
        assert!(!is_mxl_uuid("5f0a4c1e-9d3b-0c47-8f5e-2a7c61b0d3a9"), "version nibble 0");
        assert!(!is_mxl_uuid("5f0a4c1e-9d3b-4c47-cf5e-2a7c61b0d3a9"), "variant");
        assert!(!is_mxl_uuid("auto"));
    }

    #[test]
    fn creates_once_then_keeps_the_same_identity() {
        let dir = std::env::temp_dir().join(format!("mxl-domain-test-{}", uuid::Uuid::new_v4()));
        let a = load_or_create(&dir, "lab").unwrap();
        let b = load_or_create(&dir, "other label").unwrap();
        assert_eq!(a, b, "an existing definition is read, never rewritten");
        assert!(is_mxl_uuid(&a.id.to_string()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_definitions_missing_required_fields() {
        let ok = serde_json::json!({"id": "5f0a4c1e-9d3b-4c47-8f5e-2a7c61b0d3a9", "label": "a", "description": "", "tags": {}});
        assert!(DomainDef::parse(&ok).is_ok());
        for k in ["id", "label", "description", "tags"] {
            let mut v = ok.clone();
            v.as_object_mut().unwrap().remove(k);
            assert!(DomainDef::parse(&v).is_err(), "missing {k} must be rejected");
        }
    }
}
