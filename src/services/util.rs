//! Общие хелперы доменных сервисов: маппинг ошибок, uuid, LocaleText/refs → jsonb.
use tonic::Status;
use uuid::Uuid;

use crate::pb_domain::{LocaleText, StepRef};

pub fn internal<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

pub fn parse_id(s: &str) -> Result<Uuid, Status> {
    Uuid::parse_str(s).map_err(|_| Status::invalid_argument("bad uuid"))
}

pub fn loc_map(v: &serde_json::Value) -> LocaleText {
    let mut m = std::collections::HashMap::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if let Some(s) = val.as_str() {
                m.insert(k.clone(), s.to_string());
            }
        }
    }
    LocaleText { v: m }
}

pub fn loc_json(l: &Option<LocaleText>) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    if let Some(lt) = l {
        for (k, v) in &lt.v {
            m.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
    }
    serde_json::Value::Object(m)
}

pub fn refs_json(refs: &[StepRef]) -> serde_json::Value {
    serde_json::Value::Array(
        refs.iter()
            .map(|r| {
                let mut m = serde_json::Map::new();
                m.insert("label".into(), loc_json(&r.label));
                if !r.url.is_empty() {
                    m.insert("url".into(), serde_json::Value::String(r.url.clone()));
                }
                serde_json::Value::Object(m)
            })
            .collect(),
    )
}
