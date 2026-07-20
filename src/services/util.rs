//! Общие хелперы доменных сервисов: маппинг ошибок, uuid, LocaleText/refs → jsonb.
use tonic::Status;
use uuid::Uuid;

use crate::pb_domain::{LocaleText, StepRef};

/// Внутренняя ошибка: детали — ТОЛЬКО в лог (SQL/пути/git не текут клиенту),
/// наружу generic INTERNAL. Аудит 2026-07-20, P1-6.
pub fn internal<E: std::fmt::Display>(e: E) -> Status {
    tracing::error!(error = %e, "internal error");
    Status::internal("internal error")
}

/// Ошибка Postgres → осмысленный gRPC-код (таксономия вместо тотального
/// internal): 23505 unique → ALREADY_EXISTS, 23503 fk → FAILED_PRECONDITION,
/// 22P02/22007 bad cast (enum/uuid/дата) → INVALID_ARGUMENT. Остальное —
/// internal (лог с деталями, клиенту generic).
pub fn db_status(e: sqlx::Error) -> Status {
    if let sqlx::Error::Database(db) = &e
        && let Some(code) = db.code()
    {
        match code.as_ref() {
            "23505" => return Status::already_exists("already exists"),
            "23503" => return Status::failed_precondition("referenced row missing"),
            "22P02" | "22007" => return Status::invalid_argument("invalid value"),
            _ => {}
        }
    }
    internal(e)
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
