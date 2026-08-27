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
async fn failures_accumulate_while_success_resets() {
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
async fn another_lists_counter_is_untouched() {
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

/// ⚠️ СБОЙ ЗЕРКАЛА ОБЯЗАН ДОЕХАТЬ ДО СТАТУСА — это обещание модуля, и до 27.08 его не
/// проверял никто.
///
/// `services/git_core/mirror.rs` в своей же шапке обещает: «любой сбой — в статус списка
/// (mirror_error) и метрику: молчаливой деградации быть не должно». Покрытие модуля при
/// этом было **7,7% строк**: тесты рядом дёргали `record_mirror_result` напрямую, то есть
/// проверяли ЗАПИСЬ, но не то, что до неё вообще доходит дело.
///
/// Разница существенная: если сбой расшифровки вернётся из функции раньше записи, владелец
/// увидит зеркало «в порядке» при неработающей синхронизации — ровно та тихая деградация,
/// против которой шапка и написана.
///
/// Проверяем ЧЕРЕЗ RPC, а не через внутренности: путь целиком, как у живого вызова.
///
/// ⚠️ ЧТО ИМЕННО ПОКРЫТО, а что нет — важно не преувеличить. В этом бинаре
/// `SETFORK_MIRROR_SECRET` не задан, поэтому срабатывает ветка «секрет не задан», а до
/// ветки «токен не расшифровался» дело НЕ доходит: `mirror_secret()` кэширует значение в
/// `OnceLock`, то есть в одном процессе живёт ровно одно из двух состояний.
///
/// Проверено мутацией: снять запись в статус — тест краснеет; превратить ветку расшифровки
/// в `Ok(())` — тест ОСТАЁТСЯ ЗЕЛЁНЫМ, потому что эта ветка здесь недостижима. Второе
/// записано не как недостаток, а чтобы никто не прочитал зелёный тест шире, чем он есть.
///
/// Сама расшифровка при этом покрыта отдельно, юнитами в `git/mirror.rs` (83% строк);
/// непокрытой остаётся только ПРОВОДКА этой ветки до записи статуса — а проводка у обеих
/// веток общая и здесь проверена.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_mirror_failure_reaches_the_list_status() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "mirrorfail").await;
    let id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version, mirror_url, mirror_token) \
         values ($1, 'mirror-fail', '{\"en\":\"Mirror fail\"}', 1, \
                 'https://github.com/someone/repo', 'заведомо-негодный-шифротекст') returning id",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .expect("seed template с настроенным зеркалом");

    let (n, err) = attempts(&pool, id).await;
    assert_eq!((n, err), (0, None), "до попытки счётчик чист");

    let svc = setfork_core::services::git_core::GitCoreSvc { pool: pool.clone() };
    let res = setfork_core::pb::git_core_server::GitCore::mirror_push(
        &svc,
        tonic::Request::new(setfork_core::pb::RepoRef {
            owner: "mirrorfail".into(),
            slug: "mirror-fail".into(),
        }),
    )
    .await
    .expect("RPC не падает: сбой зеркала — это ответ, а не ошибка транспорта")
    .into_inner();

    assert!(!res.ok, "пуш обязан сообщить о неудаче: токен нерасшифровываем");
    assert!(!res.error.is_empty(), "и назвать причину, а не отдать пустую строку");

    // ГЛАВНОЕ: неудача доехала до статуса списка, а не осталась в возвращаемом значении.
    let (n, err) = attempts(&pool, id).await;
    assert_eq!(n, 1, "счётчик неудач поднялся — иначе владелец видит «всё в порядке»");
    assert_eq!(
        err.as_deref(),
        Some(res.error.as_str()),
        "в статусе лежит ТА ЖЕ причина, что вернул RPC: расхождение здесь означало бы, \
         что человек в интерфейсе и человек в ответе видят разное"
    );
}
