//! Postgres-слой (та же БД, что у Next). Зона разрезана по ответственности
//! (линза 08): здесь остались пулы, мелкие чтения и общие типы, всё остальное —
//! в подмодулях.
//!
//! | подмодуль | ответственность |
//! |---|---|
//! | `versions` | чтение истории версий одним запросом + разворот строки в канон |
//! | `write` | запись версии: строки, шаги, счётчик, мета — одна транзакция по смыслу |
//! | `carryover` | перенос надстроек Postgres при проекции пуша (тихая потеря по построению) |
//! | `mirror` | настройки и статус зеркала — единственные данные, едущие НАРУЖУ |
use sqlx::postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;

mod carryover;
mod mirror;
mod versions;
mod write;

pub use carryover::CarryOver;
pub use carryover::current_marks;
pub use mirror::{load_mirror, record_mirror_result};
pub use versions::{StepRow, load_bundle_data, ser_step_from_row};
pub use write::{
    add_version, add_version_rows, bump_current_version, insert_step_rows, insert_version_row, update_meta,
};

// step_level enum БД — прямой bind (без text→enum каста).
#[derive(sqlx::Type)]
#[sqlx(type_name = "step_level", rename_all = "lowercase")]
pub enum Level {
    Required,
    Recommended,
    Optional,
}
impl Level {
    /// Мягкий парс уровня: неизвестное/пустое → Required (как TS-дефолт).
    pub fn parse(s: &str) -> Level {
        match s.trim() {
            "recommended" => Level::Recommended,
            "optional" => Level::Optional,
            _ => Level::Required,
        }
    }
    /// Строка для канона list.json — та же, что отдаёт `level::text` из БД.
    pub fn as_str(&self) -> &'static str {
        match self {
            Level::Required => "required",
            Level::Recommended => "recommended",
            Level::Optional => "optional",
        }
    }
}

// LocaleText jsonb: {"en": s} для непустого, иначе {} (порт project.ts L()).
fn loc_val(s: &str) -> serde_json::Value {
    let t = s.trim();
    if t.is_empty() { serde_json::json!({}) } else { serde_json::json!({ "en": t }) }
}

/// LocaleText (jsonb) → строка: берём 'en', иначе первое значение.
pub(crate) fn loc(v: &serde_json::Value) -> String {
    if let Some(o) = v.as_object() {
        if let Some(s) = o.get("en").and_then(|x| x.as_str()) {
            return s.to_string();
        }
        for val in o.values() {
            if let Some(s) = val.as_str() {
                return s.to_string();
            }
        }
    }
    String::new()
}

// Подключение к той же Postgres, что у Next. Runtime-запросы (без compile-time
// проверки), чтобы сборка не требовала живой БД. Параметры — из config::Config
// (env читается один раз на старте).
// acquire_timeout — быстрый отказ вместо зависания, если пул исчерпан;
// test_before_acquire — не отдаём мёртвое соединение после рестарта БД.
pub async fn connect(url: &str, max: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .test_before_acquire(true)
        .connect(url)
        .await
}

/// Мини-пул ТОЛЬКО под advisory-локи репо (git::repo::set_lock_pool): RepoGuard
/// держит соединение на всё время git-операции, из общего пула это выедало по
/// соединению на push (аудит 2026-07-20, P1-5). acquire_timeout выше обычного —
/// очередь тяжёлых git-операций легитимна, быстрый отказ тут вреден.
pub async fn connect_lock_pool(url: &str) -> Result<PgPool, sqlx::Error> {
    let max = crate::config::lock_pool_max();
    tracing::info!(max, "lock pool for repo advisory locks");
    PgPoolOptions::new()
        .max_connections(max)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .test_before_acquire(true)
        .connect(url)
        .await
}

/// Резолв списка по owner handle + slug → (template_id, current_version).
pub async fn resolve_list(
    pool: &PgPool,
    owner: &str,
    slug: &str,
) -> Result<Option<(Uuid, i32)>, sqlx::Error> {
    let row: Option<(Uuid, i32)> = sqlx::query_as(
        "select t.id, t.current_version \
         from templates t join users u on u.id = t.owner_id \
         where u.handle = $1 and t.slug = $2 limit 1",
    )
    .bind(owner)
    .bind(slug)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// list_kind списка (валидное значение или None) — для канона веточных записей
/// (Ф2a): провод kind не несёт, тип — свойство templates.
pub async fn load_list_kind(pool: &PgPool, id: Uuid) -> Result<Option<String>, sqlx::Error> {
    let k: Option<Option<String>> = sqlx::query_scalar("select list_kind from templates where id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(k.flatten().filter(|k| crate::git::serialize::is_valid_kind(k)))
}

/// Число опубликованных публичных списков — быстрый self-check связи с БД.
pub async fn published_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    let (n,): (i64,) = sqlx::query_as(
        "select count(*) from templates where status = 'published' and visibility = 'public' and moderation = 'active'",
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}
