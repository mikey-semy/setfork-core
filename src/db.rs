use sqlx::postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;

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
