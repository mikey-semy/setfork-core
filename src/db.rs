//! Postgres-слой (та же БД, что у Next): пул, чтение истории версий для
//! материализации, единый движок записи версий (StepRow/add_version_rows),
//! обновление метаданных списка из git-проекции.
use crate::blocks::is_step_type;
use crate::git::bundle::{SerStep, StepRef, VersionData};
use crate::git::project::ProjStep;
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
    /// Мягкий парс уровня: неизвестное/пустое → Required (как TS-дефолт).
    pub fn parse(s: &str) -> Level {
        match s.trim() {
            "recommended" => Level::Recommended,
            "optional" => Level::Optional,
            _ => Level::Required,
        }
    }
}

// LocaleText jsonb: {"en": s} для непустого, иначе {} (порт project.ts L()).
fn loc_val(s: &str) -> serde_json::Value {
    let t = s.trim();
    if t.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::json!({ "en": t })
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
    // Размер пула — под нагрузку/лимиты Postgres (PGPOOL_MAX, по умолч. 10).
    // acquire_timeout — быстрый отказ вместо зависания, если пул исчерпан;
    // test_before_acquire — не отдаём мёртвое соединение после рестарта БД.
    let max = std::env::var("PGPOOL_MAX")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10);
    PgPoolOptions::new()
        .max_connections(max)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .test_before_acquire(true)
        .connect(&url)
        .await
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

    // Шаги ВСЕХ версий одним запросом (вместо запроса на версию — история
    // длинного списка давала N+1 round-trip'ов), группировка по version_id.
    let srows = sqlx::query(
        "select s.version_id, s.n, s.\"type\", s.content, s.title, s.\"desc\", s.command, \
                s.level::text as level, s.why, s.section, s.subtasks, s.refs \
         from steps s join template_versions tv on tv.id = s.version_id \
         where tv.template_id = $1 order by s.version_id, s.n asc",
    )
    .bind(list_id)
    .fetch_all(pool)
    .await?;
    let mut steps_by_ver: std::collections::HashMap<Uuid, Vec<SerStep>> = std::collections::HashMap::new();
    {
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
            // Блочная модель: type/content несём только у не-step блоков.
            let block_type: Option<String> = sr.try_get::<Option<String>, _>("type").ok().flatten().filter(|t| !is_step_type(t));
            let content: serde_json::Value = if block_type.is_some() {
                sr.try_get::<serde_json::Value, _>("content").unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Null
            };
            let vid: Uuid = sr.get("version_id");
            steps_by_ver.entry(vid).or_default().push(SerStep {
                n: sr.get("n"),
                block_type,
                content,
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
    }

    let mut out = Vec::with_capacity(vrows.len());
    for vr in vrows {
        let vid: Uuid = vr.get("id");
        out.push(VersionData {
            version: vr.get("version"),
            note: vr.get("note"),
            ts: vr.get("ts"),
            title: title.clone(),
            desc: desc.clone(),
            tags: tags.clone(),
            ordered,
            steps: steps_by_ver.remove(&vid).unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Каноническая строка steps под вставку. Оба пути записи версий — git-проекция
/// (ProjStep, санитизация git-входа) и доменный ListWrite (NewStep, семантика TS
/// как есть) — маппятся сюда; транзакция и INSERT одни на всех.
pub struct StepRow {
    pub block_type: String,          // 'step' | 'text' | 'image' | …
    pub content: serde_json::Value,  // {} у шага
    pub title: serde_json::Value,    // LocaleText jsonb
    pub desc: serde_json::Value,
    pub command: String,
    pub image_key: Option<String>,   // None → has_image = false
    pub level: Level,
    pub why: serde_json::Value,
    pub section: serde_json::Value,
    pub subtasks: serde_json::Value, // jsonb-массив LocaleText
    pub refs: serde_json::Value,     // jsonb-массив {label, url?}
}

/// Вставка шагов версии — единственный INSERT в steps.
pub async fn insert_step_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ver_id: Uuid,
    rows: &[StepRow],
) -> Result<(), sqlx::Error> {
    for (i, r) in rows.iter().enumerate() {
        sqlx::query(
            "insert into steps (version_id, n, type, content, title, \"desc\", command, has_image, image_key, level, why, section, subtasks, refs) \
             values ($1, $2, $3, $4::jsonb, $5::jsonb, $6::jsonb, $7, $8, $9, $10, $11::jsonb, $12::jsonb, $13::jsonb, $14::jsonb)",
        )
        .bind(ver_id)
        .bind((i as i32) + 1)
        .bind(&r.block_type)
        .bind(&r.content)
        .bind(&r.title)
        .bind(&r.desc)
        .bind(&r.command)
        .bind(r.image_key.is_some())
        .bind(&r.image_key)
        .bind(&r.level)
        .bind(&r.why)
        .bind(&r.section)
        .bind(&r.subtasks)
        .bind(&r.refs)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Единственный путь «новая версия»: FOR UPDATE current_version → insert
/// template_versions → шаги → bump current_version + updated_at, всё в одной
/// транзакции (FOR UPDATE — защита от гонки нумерации и вне guarded-пути).
/// None = списка нет. Возвращает (ver_id, version, created_at_ms).
pub async fn add_version_rows(
    pool: &PgPool,
    template_id: Uuid,
    note: &str,
    rows: &[StepRow],
) -> Result<Option<(Uuid, i32, i64)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let current: Option<i32> =
        sqlx::query_scalar("select current_version from templates where id = $1 for update")
            .bind(template_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(current) = current else {
        return Ok(None);
    };
    let new_version = current + 1;
    let (ver_id, created_ms): (Uuid, i64) = sqlx::query_as(
        "insert into template_versions (template_id, version, note) values ($1, $2, $3) \
         returning id, floor(extract(epoch from created_at) * 1000)::bigint",
    )
    .bind(template_id)
    .bind(new_version)
    .bind(note)
    .fetch_one(&mut *tx)
    .await?;

    insert_step_rows(&mut tx, ver_id, rows).await?;

    sqlx::query("update templates set current_version = $1, updated_at = now() where id = $2")
        .bind(new_version)
        .bind(template_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some((ver_id, new_version, created_ms)))
}

// ProjStep → StepRow: санитизация git-входа (порт project.ts): trim, пустые
// subtasks/refs выбрасываются, LocaleText жмётся в {"en": …}, image не несём.
fn proj_step_row(s: &ProjStep) -> StepRow {
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
    // Блочная модель: не-step блоки несут type/content; у шага — 'step'/{}.
    let is_step = is_step_type(&s.block_type);
    StepRow {
        block_type: if is_step { "step".into() } else { s.block_type.clone() },
        content: if is_step { serde_json::json!({}) } else { s.content.clone() },
        title: loc_val(&s.title),
        desc: loc_val(&s.desc),
        command: s.command.trim().to_string(),
        image_key: None,
        level: Level::parse(&s.level),
        why: loc_val(&s.why),
        section: loc_val(&s.section),
        subtasks,
        refs,
    }
}

/// Новая версия списка из проекции push. Возвращает номер новой версии;
/// RowNotFound, если списка нет (как прежний fetch_one).
pub async fn add_version(pool: &PgPool, template_id: Uuid, note: &str, steps: &[ProjStep]) -> Result<i32, sqlx::Error> {
    let rows: Vec<StepRow> = steps.iter().map(proj_step_row).collect();
    match add_version_rows(pool, template_id, note, &rows).await? {
        Some((_ver_id, version, _ms)) => Ok(version),
        None => Err(sqlx::Error::RowNotFound),
    }
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
                .bind(loc_val(&t))
                .bind(template_id)
                .execute(pool)
                .await?;
        }
    }
    if let Some(d) = desc {
        sqlx::query("update templates set \"desc\" = $1::jsonb where id = $2")
            .bind(loc_val(&d))
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
