//! Настройки и статус зеркала.
//!
//! Отдельно потому, что это единственная часть `db`, чьи данные ЕДУТ НАРУЖУ: адрес
//! чужого форджа и зашифрованный токен. Исход пуша пишется сюда же и виден владельцу
//! списка в настройках.

use sqlx::postgres::PgPool;
use uuid::Uuid;

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
