//! Перенос надстроек Postgres при проекции пуша.
//!
//! Самая опасная зона файла, и по построению: `needs_human`, `danger`, `image_key` в
//! каноне НЕ ЖИВУТ (ADR-0014), поэтому при пуше их надо ПЕРЕНЕСТИ со старой версии по
//! `block_id`. Забыть перенести — значит тихо стереть пометки, и заметно это станет не
//! сразу. Линза 02 нашла тут два таких случая, поэтому зона названа и держится вместе.

use sqlx::Row;
use sqlx::postgres::PgPool;
use uuid::Uuid;

use super::{Level, StepRow, loc_val};
use crate::blocks::is_step_type;
use crate::git::project::ProjStep;

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
pub(super) fn proj_step_row(s: &ProjStep, keep: &std::collections::HashMap<Uuid, CarryOver>) -> StepRow {
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
