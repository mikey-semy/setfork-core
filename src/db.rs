use crate::bundle::{SerStep, StepRef, VersionData};
use crate::project::ProjStep;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use uuid::Uuid;

// step_level enum БД — прямой bind (без text→enum каста).
#[derive(sqlx::Type)]
#[sqlx(type_name = "step_level", rename_all = "lowercase")]
pub enum Level {
    Required,
    Recommended,
    Optional,
}
impl Level {
    fn parse(s: &str) -> Level {
        match s.trim() {
            "recommended" => Level::Recommended,
            "optional" => Level::Optional,
            _ => Level::Required,
        }
    }
}

// LocaleText jsonb-строка: {"en": s} для непустого, иначе {} (порт project.ts L()).
fn loc_str(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        "{}".to_string()
    } else {
        serde_json::json!({ "en": t }).to_string()
    }
}

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
        // floor, не round: git усекает дробные секунды ISO-даты (TS передаёт .toISOString()).
        "select id, version, note, floor(extract(epoch from created_at))::bigint as ts \
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

/// Новая версия списка из проекции push (порт list-store.adapter addVersion).
/// current_version+1 → insert template_versions → insert steps → update current_version.
/// Всё в одной транзакции. Возвращает номер новой версии.
pub async fn add_version(pool: &PgPool, template_id: Uuid, note: &str, steps: &[ProjStep]) -> Result<i32, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let current: i32 = sqlx::query_scalar("select current_version from templates where id = $1")
        .bind(template_id)
        .fetch_one(&mut *tx)
        .await?;
    let new_version = current + 1;
    let ver_id: Uuid = sqlx::query_scalar(
        "insert into template_versions (template_id, version, note) values ($1, $2, $3) returning id",
    )
    .bind(template_id)
    .bind(new_version)
    .bind(note)
    .fetch_one(&mut *tx)
    .await?;

    for (i, s) in steps.iter().enumerate() {
        let n = (i as i32) + 1;
        let subtasks = serde_json::Value::Array(
            s.subtasks
                .iter()
                .filter(|x| !x.trim().is_empty())
                .map(|x| serde_json::json!({ "en": x.trim() }))
                .collect(),
        );
        let refs = serde_json::Value::Array(
            s.refs
                .iter()
                .filter(|r| !r.label.trim().is_empty())
                .map(|r| {
                    let mut m = serde_json::Map::new();
                    m.insert("label".into(), serde_json::json!({ "en": r.label.trim() }));
                    if let Some(u) = r.url.as_ref().map(|u| u.trim()).filter(|u| !u.is_empty()) {
                        m.insert("url".into(), serde_json::Value::String(u.to_string()));
                    }
                    serde_json::Value::Object(m)
                })
                .collect(),
        );
        sqlx::query(
            "insert into steps (version_id, n, title, \"desc\", command, has_image, image_key, level, why, section, subtasks, refs) \
             values ($1, $2, $3::jsonb, $4::jsonb, $5, false, null, $6, $7::jsonb, $8::jsonb, $9::jsonb, $10::jsonb)",
        )
        .bind(ver_id)
        .bind(n)
        .bind(loc_str(&s.title))
        .bind(loc_str(&s.desc))
        .bind(s.command.trim())
        .bind(Level::parse(&s.level))
        .bind(loc_str(&s.why))
        .bind(loc_str(&s.section))
        .bind(subtasks.to_string())
        .bind(refs.to_string())
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query("update templates set current_version = $1, updated_at = now() where id = $2")
        .bind(new_version)
        .bind(template_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(new_version)
}

/// Метаданные списка из list.json (title/desc/tags/ordered) — порт project.ts patch.
/// None = поле отсутствовало в list.json → не трогаем.
pub async fn update_meta(
    pool: &PgPool,
    template_id: Uuid,
    title: Option<String>,
    desc: Option<String>,
    tags: Option<Vec<String>>,
    ordered: Option<bool>,
) -> Result<(), sqlx::Error> {
    if let Some(t) = title {
        if !t.trim().is_empty() {
            sqlx::query("update templates set title = $1::jsonb where id = $2")
                .bind(serde_json::json!({ "en": t.trim() }).to_string())
                .bind(template_id)
                .execute(pool)
                .await?;
        }
    }
    if let Some(d) = desc {
        let j = loc_str(&d);
        sqlx::query("update templates set \"desc\" = $1::jsonb where id = $2")
            .bind(j)
            .bind(template_id)
            .execute(pool)
            .await?;
    }
    if let Some(tg) = tags {
        let tg: Vec<String> = tg.into_iter().take(20).collect();
        sqlx::query("update templates set tags = $1 where id = $2")
            .bind(&tg)
            .bind(template_id)
            .execute(pool)
            .await?;
    }
    if let Some(o) = ordered {
        sqlx::query("update templates set ordered = $1 where id = $2")
            .bind(o)
            .bind(template_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}
