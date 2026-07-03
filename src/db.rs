use crate::bundle::{SerStep, StepRef, VersionData};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use uuid::Uuid;

/// LocaleText (jsonb) → строка: берём 'en', иначе первое значение.
fn loc(v: &serde_json::Value) -> String {
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

// Подключение к той же Postgres, что у Next (DATABASE_URL). Runtime-запросы
// (без compile-time проверки), чтобы сборка не требовала живой БД.
pub async fn connect() -> Result<PgPool, sqlx::Error> {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL не задан (см. .env)");
    PgPoolOptions::new().max_connections(5).connect(&url).await
}

/// Резолв списка по owner handle + slug → (template_id, current_version).
pub async fn resolve_list(pool: &PgPool, owner: &str, slug: &str) -> Result<Option<(Uuid, i32)>, sqlx::Error> {
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

/// Число опубликованных публичных списков — быстрый self-check связи с БД.
pub async fn published_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    let (n,): (i64,) = sqlx::query_as(
        "select count(*) from templates where status = 'published' and visibility = 'public' and moderation = 'active'",
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Загрузка всей истории версий списка для материализации репо (порт bundle.ts loadVersions).
/// title/desc/tags/ordered — с уровня списка (одинаковы для всех версий); шаги — по версии.
pub async fn load_bundle_data(pool: &PgPool, list_id: Uuid) -> Result<Vec<VersionData>, sqlx::Error> {
    let trow = sqlx::query("select title, \"desc\", tags, ordered from templates where id = $1")
        .bind(list_id)
        .fetch_one(pool)
        .await?;
    let title = loc(&trow.get::<serde_json::Value, _>("title"));
    let desc = loc(&trow.get::<serde_json::Value, _>("desc"));
    let tags: Vec<String> = trow.get("tags");
    let ordered: bool = trow.get("ordered");

    let vrows = sqlx::query(
        "select id, version, note, extract(epoch from created_at)::bigint as ts \
         from template_versions where template_id = $1 order by version asc",
    )
    .bind(list_id)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(vrows.len());
    for vr in vrows {
        let vid: Uuid = vr.get("id");
        let version: i32 = vr.get("version");
        let note: String = vr.get("note");
        let ts: i64 = vr.get("ts");

        let srows = sqlx::query(
            "select n, title, \"desc\", command, level::text as level, why, section, subtasks, refs \
             from steps where version_id = $1 order by n asc",
        )
        .bind(vid)
        .fetch_all(pool)
        .await?;

        let mut steps = Vec::with_capacity(srows.len());
        for sr in srows {
            let subtasks_json: serde_json::Value = sr.get("subtasks");
            let subtasks = subtasks_json
                .as_array()
                .map(|a| a.iter().map(loc).filter(|s| !s.is_empty()).collect())
                .unwrap_or_default();
            let refs_json: serde_json::Value = sr.get("refs");
            let refs = refs_json
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|r| {
                            let label = r.get("label").map(loc).unwrap_or_default();
                            if label.is_empty() {
                                return None;
                            }
                            let url = r.get("url").and_then(|u| u.as_str()).map(|s| s.to_string());
                            Some(StepRef { label, url })
                        })
                        .collect()
                })
                .unwrap_or_default();
            steps.push(SerStep {
                n: sr.get("n"),
                title: loc(&sr.get::<serde_json::Value, _>("title")),
                desc: loc(&sr.get::<serde_json::Value, _>("desc")),
                command: sr.get::<String, _>("command"),
                level: sr.get::<String, _>("level"),
                why: loc(&sr.get::<serde_json::Value, _>("why")),
                section: loc(&sr.get::<serde_json::Value, _>("section")),
                subtasks,
                refs,
            });
        }
        out.push(VersionData {
            version,
            note,
            ts,
            title: title.clone(),
            desc: desc.clone(),
            tags: tags.clone(),
            ordered,
            steps,
        });
    }
    Ok(out)
}
