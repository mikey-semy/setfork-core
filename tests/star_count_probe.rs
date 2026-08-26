//! ПРОБА линзы проверки ядра: сходится ли счётчик звёзд с фактом под гонкой.
//!
//! `toggle_star` в транзакции читает наличие звезды, затем delete/insert И
//! правит денормализованный `templates.stars_count`. Тесты в `domain.rs`
//! последовательные, параллельного случая нет.
//!
//! Цена ошибки не в самой цифре, а в том, что по `stars_count` строится выдача
//! каталога: разъехавшийся счётчик — это ложная популярность, которую никто не
//! заметит, потому что число выглядит правдоподобно.
#![cfg(feature = "probes")]

mod support;

use setfork_core::pb_domain::UserList;
use setfork_core::pb_domain::curation_write_server::CurationWrite;
use setfork_core::services::curation::CurationWriteSvc;
use tonic::Request;
use uuid::Uuid;

async fn seed_list(pool: &sqlx::PgPool, owner: Uuid) -> Uuid {
    sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) \
         values ($1, 'stars-probe', '{\"en\":\"Stars\"}', 1) returning id",
    )
    .bind(owner)
    .fetch_one(pool)
    .await
    .expect("seed template")
}

async fn fact_and_counter(pool: &sqlx::PgPool, list_id: Uuid) -> (i64, i32) {
    let rows: i64 = sqlx::query_scalar("select count(*) from stars where template_id = $1")
        .bind(list_id)
        .fetch_one(pool)
        .await
        .expect("count");
    let counter: i32 = sqlx::query_scalar("select stars_count from templates where id = $1")
        .bind(list_id)
        .fetch_one(pool)
        .await
        .expect("counter");
    (rows, counter)
}

/// Разные пользователи ставят звезду одновременно.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn stars_of_different_users_are_counted_correctly() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "owner").await;
    let list_id = seed_list(&pool, owner).await;

    let mut users = Vec::new();
    for i in 0..8 {
        users.push(support::seed_user(&pool, &format!("fan{i}")).await);
    }

    let mut set = tokio::task::JoinSet::new();
    for u in users {
        let svc = CurationWriteSvc { pool: pool.clone() };
        set.spawn(async move {
            svc.toggle_star(Request::new(UserList { list_id: list_id.to_string(), user_id: u.to_string() }))
                .await
                .map(|r| r.into_inner().value)
                .map_err(|e| (e.code(), e.message().to_string()))
        });
    }
    let mut ok = 0;
    let mut err = Vec::new();
    while let Some(r) = set.join_next().await {
        match r.expect("join") {
            Ok(_) => ok += 1,
            Err(e) => err.push(e),
        }
    }
    let (rows, counter) = fact_and_counter(&pool, list_id).await;
    println!("8 разных: успешных {ok}, отказов {}, строк {rows}, счётчик {counter}", err.len());
    for e in &err {
        println!("  ОТКАЗ: {e:?}");
    }
    assert_eq!(rows, 8, "все восемь звёзд должны быть в таблице");
    assert_eq!(counter as i64, rows, "счётчик обязан совпадать с фактом");
}

/// Один пользователь жмёт звезду много раз подряд (двойной клик, повтор запроса).
/// Итог может быть любым — 0 или 1 звезда, — но счётчик ОБЯЗАН совпасть с фактом.
#[tokio::test]
#[ignore = "ПАДАЕТ (дефект): счётчик звёзд растёт при повторном нажатии — счётчик=4 при 1 строке"]
async fn repeated_clicks_by_one_user_do_not_break_the_counter() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "owner2").await;
    let list_id = seed_list(&pool, owner).await;
    let fan = support::seed_user(&pool, "клик").await;

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..6 {
        let svc = CurationWriteSvc { pool: pool.clone() };
        set.spawn(async move {
            svc.toggle_star(Request::new(UserList { list_id: list_id.to_string(), user_id: fan.to_string() }))
                .await
                .map(|r| r.into_inner().value)
                .map_err(|e| (e.code(), e.message().to_string()))
        });
    }
    let mut states = Vec::new();
    let mut err = Vec::new();
    while let Some(r) = set.join_next().await {
        match r.expect("join") {
            Ok(v) => states.push(v),
            Err(e) => err.push(e),
        }
    }
    let (rows, counter) = fact_and_counter(&pool, list_id).await;
    println!("6 нажатий одного: ответы {states:?}, отказов {}, строк {rows}, счётчик {counter}", err.len());
    for e in &err {
        println!("  ОТКАЗ: {e:?}");
    }
    assert!(rows == 0 || rows == 1, "у одного пользователя не может быть больше одной звезды: {rows}");
    assert_eq!(counter as i64, rows, "счётчик разъехался с фактом: счётчик={counter}, строк={rows}");
}
