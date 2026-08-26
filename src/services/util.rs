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
///
/// ⚠️ ИСЧЕРПАНИЕ ПУЛА — не «мы сломались», а «мы заняты». Мини-пул под локи репо
/// держит соединение на ВСЮ git-операцию, поэтому одновременных записей ровно
/// `SETFORK_LOCK_POOL_MAX` (дефолт 4); пятая ждёт `acquire_timeout` (30 с) и получает
/// отказ. До этой правки он ехал как `Internal`, то есть ядро сообщало о собственной
/// поломке там, где на деле стояла очередь: повтор помогает, ждать имеет смысл, а
/// код говорил обратное. Ровно тот же разбор, что у гейта записи 25.08 (линза 05):
/// ретраибельное состояние обязано иметь ретраибельный код.
///
/// `ResourceExhausted` — то, что gRPC для этого и завёл. `PoolClosed` — другое:
/// пул закрыт при остановке, повтор к этому инстансу бессмыслен, это `Unavailable`.
pub fn db_status(e: sqlx::Error) -> Status {
    match &e {
        sqlx::Error::PoolTimedOut => {
            tracing::warn!("db pool exhausted: too many concurrent operations");
            metrics::counter!("db_pool_exhausted_total").increment(1);
            return Status::resource_exhausted("core is busy, too many concurrent operations - retry");
        }
        sqlx::Error::PoolClosed => {
            tracing::error!("db pool is closed");
            return Status::unavailable("core is shutting down");
        }
        _ => {}
    }
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

#[cfg(test)]
mod pool_status_tests {
    use super::db_status;
    use tonic::Code;

    /// Исчерпание пула — очередь, а не поломка. Разбор тот же, что у гейта записи
    /// (линза 05, 25.08): ретраибельное состояние обязано иметь ретраибельный код,
    /// иначе клиент бросает попытки там, где повтор помог бы.
    #[test]
    fn an_exhausted_pool_means_busy_not_broken() {
        let s = db_status(sqlx::Error::PoolTimedOut);
        assert_eq!(
            s.code(),
            Code::ResourceExhausted,
            "ждать соединения — это «занято»; Internal сказал бы «мы сломались», и повтор \
             выглядел бы бессмысленным"
        );
    }

    /// Закрытый пул — остановка инстанса. Повтор К ЭТОМУ инстансу не поможет, но
    /// поможет к другому: Unavailable, а не Internal.
    #[test]
    fn a_closed_pool_means_shutting_down() {
        assert_eq!(db_status(sqlx::Error::PoolClosed).code(), Code::Unavailable);
    }

    /// Остальные ошибки не задеты: таксономия по SQLSTATE и generic internal на месте.
    #[test]
    fn other_errors_are_untouched() {
        assert_eq!(db_status(sqlx::Error::RowNotFound).code(), Code::Internal, "не отнесено к занятости");
    }
}
