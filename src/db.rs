//! Postgres-слой (та же БД, что у Next): пул, чтение истории версий для
//! материализации, единый движок записи версий (StepRow/add_version_rows),
//! обновление метаданных списка из git-проекции.
use crate::blocks::is_step_type;
use crate::git::bundle::{SerStep, StepRef, VersionData};
use crate::git::project::ProjStep;
use sqlx::Row;
use sqlx::postgres::{PgPool, PgPoolOptions};
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
    PgPoolOptions::new()
        .max_connections(4)
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

/// Настройки зеркала списка (Ф3): (url, шифрованный токен) или None — не настроено.
pub async fn load_mirror(pool: &PgPool, id: Uuid) -> Result<Option<(String, String)>, sqlx::Error> {
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("select mirror_url, mirror_token from templates where id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.and_then(|(url, token)| match (url, token) {
        (Some(u), Some(t)) if !u.trim().is_empty() && !t.trim().is_empty() => Some((u, t)),
        _ => None,
    }))
}

/// Статус последнего пуша зеркала: молчаливой деградации быть не должно —
/// и успех, и ошибка записываются с отметкой времени (видно в настройках).
///
/// Ф2: здесь же ведётся счётчик неудач ПОДРЯД — по нему приложение решает, стоит
/// ли повторять (отозванный токен повторами не лечится) и что сказать владельцу.
/// Считает ядро, а не приложение, по простой причине: фоновый пуш после каждой
/// записи в main проходит только здесь, приложение о нём не узнаёт вовсе.
///
/// Успех обнуляет счётчик. Отметка времени обновляется при ЛЮБОМ исходе, то есть
/// это время последней ПОПЫТКИ — на нём приложение и строит паузу до следующей.
pub async fn record_mirror_result(pool: &PgPool, id: Uuid, error: Option<&str>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "update templates set mirror_synced_at = now(), mirror_error = $1, \
         mirror_attempts = case when $1::text is null then 0 else mirror_attempts + 1 end \
         where id = $2",
    )
    .bind(error)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
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
    let trow = sqlx::query("select title, \"desc\", tags, ordered, list_kind from templates where id = $1")
        .bind(list_id)
        .fetch_one(pool)
        .await?;
    let title = loc(&trow.get::<serde_json::Value, _>("title"));
    let desc = loc(&trow.get::<serde_json::Value, _>("desc"));
    let tags: Vec<String> = trow.get("tags");
    let ordered: bool = trow.get("ordered");
    // kind в канон — только валидное значение (Ф2a): мусор в колонке не должен
    // становиться публичным контрактом файла.
    let kind: Option<String> = trow
        .try_get::<Option<String>, _>("list_kind")
        .ok()
        .flatten()
        .filter(|k| crate::git::serialize::is_valid_kind(k));

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
        "select s.version_id, s.n, s.block_id, s.\"type\", s.content, s.title, s.\"desc\", s.command, \
                s.image_key, s.level::text as level, s.needs_human, s.needs_human_ask, s.danger, \
                s.why, s.section, s.subtasks, s.refs \
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
            // ЧТЕНИЕ СТРОГОЕ, и это осознанно. Раньше здесь стояло `.ok()`, которое
            // глушило ЛЮБУЮ беду декодирования и подставляло дефолт. От отсутствия
            // колонки оно не спасало вовсе — все колонки перечислены в SELECT выше,
            // и пропавшую база не отдаст в принципе, — а вот СМЕНУ ТИПА прятало:
            // замер линзы 04 §3 показал, что после `needs_human boolean → text` список
            // читается успешно и у всех шагов пометка «нужен человек» становится
            // false. Молчаливая потеря на чтении хуже громкого отказа: сайт остаётся
            // рабочим и врёт.
            let block_type: Option<String> =
                sr.try_get::<Option<String>, _>("type")?.filter(|t| !is_step_type(t));
            let content: serde_json::Value = if block_type.is_some() {
                sr.try_get::<Option<serde_json::Value>, _>("content")?.unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::Value::Null
            };
            let vid: Uuid = sr.get("version_id");
            // Идентичность блока сквозь версии. Колонка обязательна в схеме:
            // деплой Rust идёт ПОСЛЕ применения схемы (db:push), как и раньше.
            let block_id: Option<String> = sr.try_get::<Option<Uuid>, _>("block_id")?.map(|u| u.to_string());
            steps_by_ver.entry(vid).or_default().push(SerStep {
                n: sr.get("n"),
                block_type,
                content,
                block_id,
                title: loc(&sr.get::<serde_json::Value, _>("title")),
                desc: loc(&sr.get::<serde_json::Value, _>("desc")),
                command: sr.get::<String, _>("command"),
                // Ф2a-довесок: картинка и пометка — честное содержимое канона.
                image_key: sr.try_get::<Option<String>, _>("image_key")?,
                level: sr.get::<String, _>("level"),
                needs_human: sr.try_get::<Option<bool>, _>("needs_human")?.unwrap_or(false),
                needs_human_ask: sr
                    .try_get::<Option<serde_json::Value>, _>("needs_human_ask")?
                    .map(|v| loc(&v))
                    .filter(|a| !a.is_empty()),
                danger: sr.try_get::<Option<bool>, _>("danger")?.unwrap_or(false),
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
            kind: kind.clone(),
            steps: steps_by_ver.remove(&vid).unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Каноническая строка steps под вставку. Оба пути записи версий — git-проекция
/// (ProjStep, санитизация git-входа) и доменный ListWrite (NewStep, семантика TS
/// как есть) — маппятся сюда; транзакция и INSERT одни на всех.
pub struct StepRow {
    pub block_type: String,         // 'step' | 'text' | 'image' | …
    pub content: serde_json::Value, // {} у шага
    // Стабильная идентичность блока сквозь версии. None — идентичность неизвестна
    // (старые данные или запись мимо редактора): дифф падает на фолбэк по заголовку.
    pub block_id: Option<Uuid>,
    pub title: serde_json::Value, // LocaleText jsonb
    pub desc: serde_json::Value,
    pub command: String,
    pub image_key: Option<String>, // None → has_image = false
    pub level: Level,
    pub why: serde_json::Value,
    pub section: serde_json::Value,
    pub subtasks: serde_json::Value, // jsonb-массив LocaleText
    pub refs: serde_json::Value,     // jsonb-массив {label, url?}
    /// «Здесь нужен человек»: место, где машина знать не может (цены, вкус, опыт).
    /// Долг катовера закрыт 2026-07-28: без этих полей запись через домен стирала
    /// бы пометку — набор шагов перезаписывается ЦЕЛИКОМ.
    pub needs_human: bool,
    pub needs_human_ask: serde_json::Value, // LocaleText jsonb; {} = общий текст
    /// Разрушительный пункт: команда необратима. Как и needs_human, обязан ехать
    /// обоими путями записи — набор шагов версии перезаписывается ЦЕЛИКОМ, и
    /// поле, о котором путь не знает, тихо исчезает вместе с версией.
    pub danger: bool,
}

/// Вставка шагов версии — единственный INSERT в steps.
pub async fn insert_step_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ver_id: Uuid,
    rows: &[StepRow],
) -> Result<(), sqlx::Error> {
    for (i, r) in rows.iter().enumerate() {
        sqlx::query(
            "insert into steps (version_id, n, block_id, type, content, title, \"desc\", command, has_image, image_key, level, why, section, subtasks, refs, needs_human, needs_human_ask, danger) \
             values ($1, $2, $3, $4, $5::jsonb, $6::jsonb, $7::jsonb, $8, $9, $10, $11, $12::jsonb, $13::jsonb, $14::jsonb, $15::jsonb, $16, $17::jsonb, $18)",
        )
        .bind(ver_id)
        .bind((i as i32) + 1)
        .bind(r.block_id)
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
        .bind(r.needs_human)
        .bind(&r.needs_human_ask)
        .bind(r.danger)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Вставка строки template_versions — часть движка «новая версия» (общая для
/// git-проекции и git-first веб-пути). Возвращает (ver_id, created_at сек,
/// created_at мс): секунды нужны git-коммиту (усечение как у ISO-даты в TS),
/// миллисекунды — ответу домена.
pub async fn insert_version_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    template_id: Uuid,
    version: i32,
    note: &str,
    author_id: Option<Uuid>,
) -> Result<(Uuid, i64, i64), sqlx::Error> {
    sqlx::query_as(
        "insert into template_versions (template_id, version, note, author_id) values ($1, $2, $3, $4) \
         returning id, floor(extract(epoch from created_at))::bigint, \
                   floor(extract(epoch from created_at) * 1000)::bigint",
    )
    .bind(template_id)
    .bind(version)
    .bind(note)
    .bind(author_id)
    .fetch_one(&mut **tx)
    .await
}

/// Завершение движка «новая версия»: current_version + updated_at.
pub async fn bump_current_version(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    template_id: Uuid,
    version: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("update templates set current_version = $1, updated_at = now() where id = $2")
        .bind(version)
        .bind(template_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Путь «новая версия» для git-проекции (push/merge): FOR UPDATE current_version →
/// insert template_versions → шаги → bump, всё в одной транзакции (FOR UPDATE —
/// защита от гонки нумерации и вне guarded-пути). None = списка нет.
/// Возвращает (ver_id, version, created_at_ms).
///
/// Веб-путь идёт НЕ здесь, а через git::version::commit_web_version — тем же
/// движком (insert_version_row/insert_step_rows/bump_current_version), но с
/// git-коммитом внутри транзакции: сначала коммит, потом строки.
pub async fn add_version_rows(
    pool: &PgPool,
    template_id: Uuid,
    note: &str,
    author_id: Option<Uuid>,
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
    let (ver_id, _created_s, created_ms) =
        insert_version_row(&mut tx, template_id, new_version, note, author_id).await?;

    insert_step_rows(&mut tx, ver_id, rows).await?;
    bump_current_version(&mut tx, template_id, new_version).await?;
    tx.commit().await?;
    Ok(Some((ver_id, new_version, created_ms)))
}

/// Что переносится в новую версию из ТЕКУЩЕЙ по идентичности блока: надстройки
/// Postgres, которых нет в каноне list.json (ADR-0014) — push их не приносит,
/// и без переноса он бы их молча стирал.
#[derive(Clone)]
pub struct CarryOver {
    pub needs_human: bool,
    pub needs_human_ask: serde_json::Value,
    pub image_key: Option<String>,
    pub danger: bool,
}

impl Default for CarryOver {
    fn default() -> Self {
        // ask = {} (не Null!): колонка jsonb NOT NULL, Null-bind уронил бы вставку.
        CarryOver {
            needs_human: false,
            needs_human_ask: serde_json::json!({}),
            image_key: None,
            danger: false,
        }
    }
}

// ProjStep → StepRow: санитизация git-входа (порт project.ts): trim, пустые
// subtasks/refs выбрасываются, LocaleText жмётся в {"en": …}.
fn proj_step_row(s: &ProjStep, keep: &std::collections::HashMap<Uuid, CarryOver>) -> StepRow {
    let subtasks = serde_json::Value::Array(
        s.subtasks
            .iter()
            .filter(|x| !x.trim().is_empty())
            .map(|x| serde_json::json!({ "en": x.trim() }))
            .collect(),
    );
    // Надстройки из ТЕКУЩЕЙ версии по идентичности блока (см. CarryOver).
    let carry = s
        .block_id
        .as_deref()
        .and_then(|v| Uuid::parse_str(v).ok())
        .and_then(|id| keep.get(&id).cloned())
        .unwrap_or_default();
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
        block_type: crate::blocks::storage_type(&s.block_type),
        content: if is_step { serde_json::json!({}) } else { s.content.clone() },
        // Идентичность из list.json. Невалидный uuid из чужого git-входа молча
        // отбрасываем (санитизация проекции), а не роняем пуш.
        block_id: s.block_id.as_deref().and_then(|v| Uuid::parse_str(v).ok()),
        title: loc_val(&s.title),
        desc: loc_val(&s.desc),
        command: s.command.trim().to_string(),
        // Ф2a-довесок: канон несёт imageKey — файл теперь источник. ТРИСТЕЙТ, как у
        // needs_human: поля нет (старый клон) → перенос по block_id, чтобы push
        // старого клона не стирал картинку (P1 авто-ревью #63); значение есть →
        // файл источник; ПУСТОЕ значение → явное снятие картинки пушем.
        //
        // Без последней ветки картинку нельзя было снять через git вовсе: любой
        // способ сказать «её нет» читался как «не знаю» и возвращал старый ключ
        // (F9 линзы 02). Пустая строка вдобавок доезжала в колонку как есть, и
        // has_image (image_key.is_some()) выставлялся у шага БЕЗ картинки.
        image_key: match s.image_key.as_deref().map(str::trim) {
            Some("") => None,
            Some(k) => Some(k.to_string()),
            None => carry.image_key.clone(),
        },
        level: Level::parse(&s.level),
        why: loc_val(&s.why),
        section: loc_val(&s.section),
        subtasks,
        refs,
        // Ф2a-довесок: пометка теперь в каноне. Тристейт: поле есть → файл источник
        // (true с вопросом из файла; ЯВНЫЙ false — снятие пометки пушем); поля нет
        // (старый клон) → перенос по block_id, чтобы push не стирал честные пометки
        // (та потеря чинилась во фронте 2026-07-27 и в P1 авто-ревью #63).
        needs_human: s.needs_human.unwrap_or(carry.needs_human),
        needs_human_ask: match s.needs_human {
            Some(true) => s.needs_human_ask.as_deref().map(loc_val).unwrap_or(serde_json::json!({})),
            Some(false) => serde_json::json!({}),
            None => carry.needs_human_ask.clone(),
        },
        // Разрушительный пункт — тот же тристейт: файл источник, когда поле в нём
        // есть; иначе перенос по block_id, чтобы push старого клона не снимал
        // пометку с команды, которая сносит данные.
        danger: s.danger.unwrap_or(carry.danger),
    }
}

/// Надстройки текущей версии, переносимые по идентичности блока (CarryOver).
/// Пустая карта — нормальный случай (список без пометок/картинок или без
/// block_id у строк).
pub(crate) async fn current_marks(
    pool: &PgPool,
    template_id: Uuid,
) -> Result<std::collections::HashMap<Uuid, CarryOver>, sqlx::Error> {
    let rows = sqlx::query(
        "select s.block_id, s.needs_human, s.needs_human_ask, s.image_key, s.danger \
         from steps s \
         join template_versions tv on tv.id = s.version_id \
         join templates t on t.id = tv.template_id and t.current_version = tv.version \
         where tv.template_id = $1 and s.block_id is not null",
    )
    .bind(template_id)
    .fetch_all(pool)
    .await?;
    let mut out = std::collections::HashMap::new();
    for r in rows {
        // Строго, по той же причине, что и в load_bundle_data: `.ok()` здесь могло
        // спрятать только смену типа, а не отсутствие колонки, и превращало разъезд
        // схемы в тихую потерю надстроек (линза 04 §3).
        if let Some(id) = r.try_get::<Option<Uuid>, _>("block_id")? {
            out.insert(
                id,
                CarryOver {
                    needs_human: r.try_get::<Option<bool>, _>("needs_human")?.unwrap_or(false),
                    needs_human_ask: r
                        .try_get::<Option<serde_json::Value>, _>("needs_human_ask")?
                        .unwrap_or(serde_json::json!({})),
                    image_key: r.try_get::<Option<String>, _>("image_key")?,
                    danger: r.try_get::<Option<bool>, _>("danger")?.unwrap_or(false),
                },
            );
        }
    }
    Ok(out)
}

/// Новая версия списка из проекции push. Возвращает номер новой версии;
/// RowNotFound, если списка нет (как прежний fetch_one).
pub async fn add_version(
    pool: &PgPool,
    template_id: Uuid,
    note: &str,
    steps: &[ProjStep],
) -> Result<i32, sqlx::Error> {
    // Пометки ТЕКУЩЕЙ версии по block_id — чтобы push их не стёр (см. proj_step_row).
    let keep = current_marks(pool, template_id).await?;
    let rows: Vec<StepRow> = steps.iter().map(|s| proj_step_row(s, &keep)).collect();
    match add_version_rows(pool, template_id, note, None, &rows).await? {
        Some((_ver_id, version, _ms)) => Ok(version),
        None => Err(sqlx::Error::RowNotFound),
    }
}

/// StepRow → SerStep: en-проекция строки под канон list.json.
///
/// ЗЕРКАЛО чтения load_bundle_data (строка БД → SerStep): git-first путь строит
/// канон из ещё не вставленных строк, и он обязан быть байт-в-байт тем, что
/// bootstrap соберёт из этих же строк после вставки — иначе восстановленный из
/// БД репозиторий разойдётся с оригиналом. Правила фильтрации те же: пустые
/// subtasks выбрасываются, ref без label выбрасывается, пустой url = None,
/// type/content несём только у не-step блоков.
pub fn ser_step_from_row(n: i32, r: &StepRow) -> SerStep {
    let is_step = is_step_type(&r.block_type);
    SerStep {
        n,
        block_type: Some(r.block_type.clone()).filter(|t| !is_step_type(t)),
        content: if is_step { serde_json::Value::Null } else { r.content.clone() },
        block_id: r.block_id.map(|u| u.to_string()),
        title: loc(&r.title),
        desc: loc(&r.desc),
        command: r.command.clone(),
        image_key: r.image_key.clone(),
        level: r.level.as_str().to_string(),
        needs_human: r.needs_human,
        needs_human_ask: Some(loc(&r.needs_human_ask)).filter(|a| !a.is_empty() && r.needs_human),
        danger: r.danger,
        why: loc(&r.why),
        section: loc(&r.section),
        subtasks: r
            .subtasks
            .as_array()
            .map(|a| a.iter().map(loc).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default(),
        refs: r
            .refs
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let label = x.get("label").map(loc).unwrap_or_default();
                        if label.is_empty() {
                            return None;
                        }
                        let url = x.get("url").and_then(|u| u.as_str()).map(|s| s.to_string());
                        Some(StepRef { label, url })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Метаданные списка из list.json (title/desc/tags/ordered/kind) — порт project.ts patch.
/// None = поле отсутствовало в list.json → не трогаем.
/// Мета списка из запушенного канона. ОДНОЙ транзакцией: полей четыре, и сбой на
/// третьем оставлял бы мету наполовину применённой — то есть базу в состоянии,
/// которого нет ни в одном коммите (линза проверки 04 §7). Восстановилось бы это
/// только следующим пушем, а до тех пор список показывал бы новый заголовок со
/// старыми тегами.
pub async fn update_meta(
    pool: &PgPool,
    template_id: Uuid,
    title: Option<String>,
    desc: Option<String>,
    tags: Option<Vec<String>>,
    ordered: Option<bool>,
    kind: Option<String>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    if let Some(t) = title
        && !t.trim().is_empty()
    {
        sqlx::query("update templates set title = $1::jsonb where id = $2")
            .bind(loc_val(&t))
            .bind(template_id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(d) = desc {
        sqlx::query("update templates set \"desc\" = $1::jsonb where id = $2")
            .bind(loc_val(&d))
            .bind(template_id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(tg) = tags {
        let tg: Vec<String> = tg.into_iter().take(20).collect();
        sqlx::query("update templates set tags = $1 where id = $2")
            .bind(&tg)
            .bind(template_id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(o) = ordered {
        sqlx::query("update templates set ordered = $1 where id = $2")
            .bind(o)
            .bind(template_id)
            .execute(&mut *tx)
            .await?;
    }
    // kind из push: пишем только валидное значение (санитизация чужого git-входа,
    // как block_id); отсутствие поля или мусор колонку не трогают — иначе push
    // старого клона стирал бы тип, выставленный генерацией.
    if let Some(k) = kind.filter(|k| crate::git::serialize::is_valid_kind(k)) {
        sqlx::query("update templates set list_kind = $1 where id = $2")
            .bind(&k)
            .bind(template_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

#[cfg(test)]
mod ser_step_tests {
    use super::{Level, StepRow, ser_step_from_row};
    use uuid::Uuid;

    fn row() -> StepRow {
        StepRow {
            block_type: "step".into(),
            content: serde_json::json!({}),
            block_id: Some(Uuid::from_u128(7)),
            title: serde_json::json!({ "en": "Install Redis", "ru": "Поставить Redis" }),
            desc: serde_json::json!({ "en": "Grab it" }),
            command: "brew install redis".into(),
            image_key: None,
            level: Level::Recommended,
            why: serde_json::json!({ "en": "нужно для кэша" }),
            section: serde_json::json!({ "en": "Setup" }),
            subtasks: serde_json::json!([{ "en": "проверить версию" }, { "en": "" }, {}]),
            refs: serde_json::json!([
                { "label": { "en": "docs" }, "url": "https://redis.io" },
                { "label": { "en": "без ссылки" } },
                { "label": { "en": "" }, "url": "https://dropped.example" },
            ]),
            needs_human: true,
            needs_human_ask: serde_json::json!({}),
            danger: false,
        }
    }

    /// en-проекция строки: фильтры ровно как у чтения БД (load_bundle_data) —
    /// иначе bootstrap из вставленных строк соберёт другой канон.
    #[test]
    fn en_проекция_и_фильтры_совпадают_с_чтением_бд() {
        let s = ser_step_from_row(3, &row());
        assert_eq!(s.n, 3);
        assert_eq!(s.block_type, None, "step не несёт type");
        assert_eq!(s.content, serde_json::Value::Null, "у шага content не пишется");
        assert_eq!(s.block_id.as_deref(), Some("00000000-0000-0000-0000-000000000007"));
        assert_eq!(s.title, "Install Redis", "канон берёт en");
        assert_eq!(s.level, "recommended");
        assert_eq!(s.subtasks, vec!["проверить версию".to_string()], "пустые подзадачи выброшены");
        assert_eq!(s.refs.len(), 2, "ref без label выброшен");
        assert_eq!(s.refs[0].url.as_deref(), Some("https://redis.io"));
        assert_eq!(s.refs[1].url, None, "отсутствующий url = None");
    }

    /// Не-step блок: type/content уезжают в канон как есть.
    #[test]
    fn блок_несёт_type_и_content() {
        let mut r = row();
        r.block_type = "text".into();
        r.content = serde_json::json!({ "md": "Вступление" });
        let s = ser_step_from_row(1, &r);
        assert_eq!(s.block_type.as_deref(), Some("text"));
        assert_eq!(s.content, serde_json::json!({ "md": "Вступление" }));
    }

    /// Ф2a-довесок: картинка и пометка — ЧЕСТНОЕ содержимое канона (решение
    /// владельца). Пишутся только при наличии; вопрос — только при поднятой пометке.
    #[test]
    fn надстройки_текут_в_канон_намеренно() {
        let mut r = row();
        r.image_key = Some("steps/x.png".into());
        let s = ser_step_from_row(1, &r);
        assert_eq!(s.image_key.as_deref(), Some("steps/x.png"));
        assert!(s.needs_human, "пометка из строки");
        let json = crate::git::serialize::list_json(&crate::git::bundle::VersionData {
            version: 1,
            note: String::new(),
            ts: 0,
            title: "L".into(),
            desc: String::new(),
            tags: vec![],
            ordered: true,
            kind: None,
            steps: vec![s],
        });
        assert!(json.contains("\"imageKey\": \"steps/x.png\""), "картинка в каноне: {json}");
        assert!(json.contains("\"needsHuman\": true"), "пометка в каноне: {json}");

        // Без картинки и пометки поля не пишутся — байты старых списков не меняются.
        let mut plain = row();
        plain.image_key = None;
        plain.needs_human = false;
        let s = ser_step_from_row(1, &plain);
        let json = crate::git::serialize::list_json(&crate::git::bundle::VersionData {
            version: 1,
            note: String::new(),
            ts: 0,
            title: "L".into(),
            desc: String::new(),
            tags: vec![],
            ordered: true,
            kind: None,
            steps: vec![s],
        });
        assert!(!json.contains("imageKey"), "нет картинки — нет поля: {json}");
        assert!(!json.contains("needsHuman"), "нет пометки — нет поля: {json}");
    }
}
