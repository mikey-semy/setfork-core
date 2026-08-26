//! Чтение истории версий: одним запросом, без N+1.
//!
//! Здесь же `ser_step_from_row` — и это НЕ случайность соседства. Две функции объявлены
//! зеркалами друг друга: одна собирает строку шага из базы, другая разворачивает её в
//! канон. Согласованная ошибка в обеих не ловится сверкой их между собой, поэтому третья
//! сторона обязательна и живёт в `tests/canon_data_roundtrip.rs` (линза 07 §2 и §8).

use sqlx::Row;
use sqlx::postgres::PgPool;
use uuid::Uuid;

use super::{Level, loc};
use crate::blocks::is_step_type;
use crate::git::bundle::{SerStep, StepRef, VersionData};

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
