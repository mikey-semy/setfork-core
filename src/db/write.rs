//! Запись версии списка: строки версии, шаги, счётчик, мета.
//!
//! Держится вместе потому, что это ОДНА транзакция по смыслу: версия без шагов, шаги
//! без версии и мета без версии — три разных способа оставить базу в состоянии, которого
//! нет ни в одном коммите. Линза 04 нашла тут ровно такой случай у меты.

use sqlx::postgres::PgPool;
use uuid::Uuid;

use super::carryover::{current_marks, proj_step_row};
use super::{StepRow, loc_val};
use crate::git::project::ProjStep;

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

/// Мета списка из запушенного канона (title/desc/tags/ordered/kind) — порт project.ts patch.
/// `None` в поле = его не было в list.json, значит не трогаем.
///
/// ОДНОЙ транзакцией: полей пять, и сбой на третьем оставлял бы мету наполовину применённой — то есть базу в состоянии,
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
