//! Ф2: счётчик неудач зеркала — контракт между ядром и подметальщиком ретраев.
//!
//! Ядро — единственный, кто видит фоновый пуш (приложение о нём не узнаёт), и
//! единственный, кто может сказать «эта неудача уже пятая подряд». По счётчику
//! приложение решает, повторять ли и что показать владельцу. Поэтому правило
//! проверяется здесь, а не подразумевается: «растёт на неудаче, обнуляется
//! успехом» — ровно тот инвариант, поломка которого либо крутит бесполезные
//! повторы вечно, либо объявляет «повторы прекращены» после первой же осечки.
//!
//! Запуск: TEST_DATABASE_URL=... cargo test -- --include-ignored
mod support;

use setfork_core::db;
use uuid::Uuid;

async fn attempts(pool: &sqlx::PgPool, id: Uuid) -> (i32, Option<String>) {
    sqlx::query_as("select mirror_attempts, mirror_error from templates where id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read mirror state")
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn неудачи_копятся_подряд_а_успех_обнуляет() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "mirrorman").await;
    let id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) \
         values ($1, 'mirrored', '{\"en\":\"Mirrored\"}', 1) returning id",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .expect("seed template");

    let (n, err) = attempts(&pool, id).await;
    assert_eq!((n, err), (0, None), "новый список начинает с чистого счётчика");

    for expected in 1..=3 {
        db::record_mirror_result(&pool, id, Some("403 Forbidden")).await.expect("record error");
        let (n, err) = attempts(&pool, id).await;
        assert_eq!(n, expected, "каждая неудача поднимает счётчик");
        assert_eq!(err.as_deref(), Some("403 Forbidden"));
    }

    db::record_mirror_result(&pool, id, None).await.expect("record success");
    let (n, err) = attempts(&pool, id).await;
    assert_eq!(n, 0, "успех обнуляет серию — иначе зеркало навсегда осталось бы «сломанным»");
    assert_eq!(err, None, "и ошибка снимается");

    // Следующая неудача начинает счёт заново, а не продолжает прежний.
    db::record_mirror_result(&pool, id, Some("timeout")).await.expect("record error again");
    assert_eq!(attempts(&pool, id).await.0, 1);
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn счётчик_чужого_списка_не_трогается() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "twolists").await;
    let mk = async |slug: &str| -> Uuid {
        sqlx::query_scalar(
            "insert into templates (owner_id, slug, title, current_version) \
             values ($1, $2, '{\"en\":\"L\"}', 1) returning id",
        )
        .bind(owner)
        .bind(slug)
        .fetch_one(&pool)
        .await
        .expect("seed template")
    };
    let (a, b) = (mk("list-a").await, mk("list-b").await);

    db::record_mirror_result(&pool, a, Some("boom")).await.expect("record");
    assert_eq!(attempts(&pool, a).await.0, 1);
    assert_eq!(attempts(&pool, b).await.0, 0, "неудача одного зеркала не отражается на другом");
}
