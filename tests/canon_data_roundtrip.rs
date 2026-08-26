//! Линза 02 §6: КРУГ «канон → разбор → канон» на данных, а не на фикстурах.
//!
//! Сериализация пишет `list.json`, проекция читает его строго, и набор шагов
//! версии перезаписывается ЦЕЛИКОМ. Значит поле, которое пишется, но не читается
//! обратно, теряется молча при первом же push — и заметно это станет через
//! неделю, когда восстанавливать будет неоткуда.
//!
//! Проверка настоящая: список со ВСЕМИ заполненными полями сериализуется, тот же
//! текст уходит НАСТОЯЩИМ push (через pre-receive), проецируется обратно и
//! сериализуется снова. Байты обязаны совпасть — кроме номера версии, который
//! push и поднимает.
//!
//! Запуск: TEST_DATABASE_URL=... cargo test --test canon_data_roundtrip -- --include-ignored
mod support;

use std::path::Path;
use std::process::Command;

use setfork_core::db;
use setfork_core::git::{bundle, project, serialize};
use sqlx::PgPool;
use uuid::Uuid;

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(cwd)
        .env("SETFORK_ROLE", "owner")
        .args(["-c", "user.email=test@setfork.com", "-c", "user.name=Tester"])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(out.status.success(), "git {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
}

/// Список + одна версия. `steps_sql` — строки VALUES для таблицы steps.
async fn seed(pool: &PgPool, handle: &str, kind: Option<&str>, steps_sql: &str) -> Uuid {
    let owner = support::seed_user(pool, handle).await;
    let id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, \"desc\", tags, ordered, list_kind, current_version) \
         values ($1, $2, '{\"en\":\"Круг\"}', '{\"en\":\"Проверка круга\"}', array['ops','круг'], true, $3, 1) \
         returning id",
    )
    .bind(owner)
    .bind(handle)
    .bind(kind)
    .fetch_one(pool)
    .await
    .expect("seed template");
    let vid: Uuid = sqlx::query_scalar(
        "insert into template_versions (template_id, version, note) values ($1, 1, 'initial') returning id",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("seed version");
    if !steps_sql.trim().is_empty() {
        // Строки VALUES собраны в самом тесте (константы выше), пользовательского
        // ввода тут нет — поэтому AssertSqlSafe, а не bind по два десятка полей.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "insert into steps (version_id, n, block_id, \"type\", content, title, \"desc\", command, image_key, \
             level, needs_human, needs_human_ask, danger, why, section, subtasks, refs) values {steps_sql}"
        )))
        .bind(vid)
        .execute(pool)
        .await
        .expect("seed steps");
    }
    id
}

/// Канон версии `ver` этого списка — тем же кодом, что пишет файл в git.
async fn canon_of(pool: &PgPool, id: Uuid, ver: i32) -> String {
    let versions = db::load_bundle_data(pool, id).await.expect("load bundle");
    let v = versions.into_iter().find(|v| v.version == ver).expect("версия есть");
    serialize::list_json(&v)
}

/// Проводит канон списка через НАСТОЯЩИЙ push и возвращает (канон до, канон после).
async fn round(pool: &PgPool, id: Uuid) -> (String, String) {
    let before = canon_of(pool, id, 1).await;

    let root = std::env::temp_dir().join(format!("setfork-canon-{}", Uuid::new_v4()));
    let bare = root.join("repo.git");
    let versions = db::load_bundle_data(pool, id).await.expect("load bundle");
    bundle::bootstrap_bare(&versions, &bare).expect("bootstrap");

    // Пушим ТОТ ЖЕ текст, подняв только номер версии: любое расхождение после
    // круга — потеря разбора, а не правка человека.
    let mut value: serde_json::Value = serde_json::from_str(&before).expect("канон — валидный json");
    value["version"] = serde_json::json!(2);
    let work = root.join("work");
    git(&root, &["clone", "-q", bare.to_str().unwrap(), work.to_str().unwrap()]);
    std::fs::write(work.join("list.json"), serde_json::to_string_pretty(&value).unwrap()).expect("write");
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "v2: круг"]);
    git(&work, &["push", "-q", "origin", "main"]);

    let ver = project::project_pushed_commit(pool, id, &bare)
        .await
        .expect("проекция не падает")
        .expect("есть что проецировать");
    assert_eq!(ver, 2, "push стал версией 2");

    let after = canon_of(pool, id, 2).await;
    let _ = std::fs::remove_dir_all(&root);
    (before, after)
}

/// Сравнение без номера версии — его push поднимает законно.
fn same_but_version(before: &str, after: &str) -> (serde_json::Value, serde_json::Value) {
    let mut a: serde_json::Value = serde_json::from_str(before).unwrap();
    let mut b: serde_json::Value = serde_json::from_str(after).unwrap();
    a["version"] = serde_json::json!(0);
    b["version"] = serde_json::json!(0);
    (a, b)
}

const FULL_STEPS: &str = "\
 ($1, 1, '11111111-1111-1111-1111-111111111111', 'step', '{}', '{\"en\":\"Шаг с ВСЕМИ полями\"}', \
  '{\"en\":\"описание с «кавычками» и — тире\"}', 'rm -rf /tmp/х', 'img/скрин.png', 'required', true, \
  '{\"en\":\"Точно сносим?\"}', true, '{\"en\":\"иначе не взлетит\"}', '{\"en\":\"Раздел\"}', \
  '[{\"en\":\"подзадача\"}]', '[{\"label\":{\"en\":\"док\"},\"url\":\"https://setfork.com\"}]'), \
 ($1, 2, '22222222-2222-2222-2222-222222222222', 'text', '{\"md\":\"Вводный **абзац**\"}', '{}', '{}', '', \
  null, 'optional', false, '{}', false, '{}', '{}', '[]', '[]')";

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn the_round_trip_keeps_every_list_field() {
    let pool = support::pool_with_schema().await;
    let id = seed(&pool, "roundfull", Some("checklist"), FULL_STEPS).await;

    let (before, after) = round(&pool, id).await;

    // Сперва — что круг вообще НЕ ЗЕЛЁН ПО ПОСТРОЕНИЮ. Надстройки Postgres
    // (imageKey, needsHuman, danger) умеет возвращать перенос по blockId, и
    // сравнение «до/после» прошло бы, даже если бы файл их не нёс вовсе. Поэтому
    // отдельно требуем, чтобы их нёс САМ канон: он и есть предмет этой линзы.
    // ⚠️ Проверяем ПОСЕЯННЫЕ ЗНАЧЕНИЯ, а не имена полей. Раньше здесь стоял
    // `before.contains("\"why\"")`, и этого было МАЛО: имя ключа канон печатает и при
    // пустом значении, поэтому путь загрузки, обнуляющий поле, проходил проверку
    // насквозь. Поймано мутацией 26.08 (линза 07 §8): `why` заменён на `String::new()`
    // в `db.rs` — круг остался ЗЕЛЁНЫМ, хотя ловить такое и есть его работа.
    for (поле, ожидаем) in [
        ("blockId", "\"blockId\": \"11111111-1111-1111-1111-111111111111\""),
        ("imageKey", "\"imageKey\": \"img/скрин.png\""),
        ("needsHuman", "\"needsHuman\": true"),
        ("needsHumanAsk", "\"needsHumanAsk\": \"Точно сносим?\""),
        ("danger", "\"danger\": true"),
        ("kind", "\"kind\": \"checklist\""),
        ("why", "\"why\": \"иначе не взлетит\""),
        ("section", "\"section\": \"Раздел\""),
        ("subtasks", "\"подзадача\""),
        ("refs", "\"url\": \"https://setfork.com\""),
    ] {
        assert!(
            before.contains(ожидаем),
            "канон НЕ НЕСЁТ значение поля {поле}: ждали подстроку {ожидаем:?}.\n\
             Это не косметика: перенос по blockId умеет вернуть надстройки из Postgres, \
             и сравнение «до/после» сошлось бы даже при пустом файле. Канон обязан нести \
             значение САМ.\nканон:\n{before}"
        );
    }

    let (a, b) = same_but_version(&before, &after);
    assert_eq!(a, b, "круг потерял поля:\nДО:\n{before}\nПОСЛЕ:\n{after}");
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn the_round_trip_holds_a_stepless_list_and_a_single_block_list() {
    let pool = support::pool_with_schema().await;

    // Список БЕЗ шагов: пустой массив обязан пережить круг, а не превратиться в
    // отсутствие поля (проекция читает steps как Option).
    let empty = seed(&pool, "roundempty", None, "").await;
    let (b1, a1) = round(&pool, empty).await;
    let (x, y) = same_but_version(&b1, &a1);
    assert_eq!(x, y, "список без шагов не пережил круг");

    // Один НЕ-step блок: у него нет ни title, ни команды — только payload.
    let only_text = seed(
        &pool,
        "roundtext",
        None,
        "($1, 1, '33333333-3333-3333-3333-333333333333', 'text', '{\"md\":\"Только текст\"}', '{}', '{}', '', \
          null, 'required', false, '{}', false, '{}', '{}', '[]', '[]')",
    )
    .await;
    let (b2, a2) = round(&pool, only_text).await;
    let (x2, y2) = same_but_version(&b2, &a2);
    assert_eq!(x2, y2, "список из одного не-step блока не пережил круг");
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_block_with_bad_content_neither_breaks_projection_nor_lies_silently() {
    // Третий случай §6: payload не-step блока пришёл НЕ объектом. В Postgres такое
    // не заводится (колонка jsonb со схемой блока), но push приносит чужой файл, и
    // проекция обязана дать определённый ответ, а не панику и не тихую подмену.
    let pool = support::pool_with_schema().await;
    let id = seed(
        &pool,
        "roundbad",
        None,
        "($1, 1, '44444444-4444-4444-4444-444444444444', 'text', '{\"md\":\"Текст\"}', '{}', '{}', '', \
          null, 'required', false, '{}', false, '{}', '{}', '[]', '[]')",
    )
    .await;

    let before = canon_of(&pool, id, 1).await;
    let root = std::env::temp_dir().join(format!("setfork-canon-{}", Uuid::new_v4()));
    let bare = root.join("repo.git");
    let versions = db::load_bundle_data(&pool, id).await.expect("load bundle");
    bundle::bootstrap_bare(&versions, &bare).expect("bootstrap");

    let mut value: serde_json::Value = serde_json::from_str(&before).unwrap();
    value["version"] = serde_json::json!(2);
    value["steps"][0]["content"] = serde_json::json!("просто строка, а не объект");
    let work = root.join("work");
    git(&root, &["clone", "-q", bare.to_str().unwrap(), work.to_str().unwrap()]);
    std::fs::write(work.join("list.json"), serde_json::to_string_pretty(&value).unwrap()).expect("write");
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "v2: негодный content"]);
    git(&work, &["push", "-q", "origin", "main"]);

    let ver = project::project_pushed_commit(&pool, id, &bare).await.expect("проекция не падает");

    // Что бы ядро ни решило — оно обязано решить ОДНО из двух и не сорваться на панику.
    match ver {
        None => {
            let cur: i32 = sqlx::query_scalar("select current_version from templates where id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(cur, 1, "версия не создана — текущая обязана остаться нетронутой");
        }
        Some(v) => {
            assert_eq!(v, 2);
            let content: Option<serde_json::Value> = sqlx::query_scalar(
                "select s.content from steps s join template_versions tv on tv.id = s.version_id \
                 where tv.template_id = $1 and tv.version = 2 order by s.n limit 1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
            // Главное: строка НЕ выдаётся за объект блока. Любой из двух исходов
            // (пусто или как есть) честен, подмена «похожим объектом» — нет.
            let c = content.unwrap_or(serde_json::Value::Null);
            assert!(
                c.is_null() || c.is_string() || c.as_object().is_some_and(|o| o.is_empty()),
                "негодный payload доехал до базы чем-то третьим: {c}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// РАЗЪЕЗД СХЕМЫ ОБЯЗАН БЫТЬ ГРОМКИМ.
///
/// Линза 04 §3, замер: раньше чтение шагов глушило любую беду декодирования и
/// подставляло дефолт. От отсутствия колонки это не спасало (все колонки названы в
/// SELECT — пропавшую база не отдаст), зато прятало СМЕНУ ТИПА: после
/// `needs_human boolean → text` список читался успешно, и у всех шагов пометка
/// «здесь нужен человек» становилась false. Сайт остаётся рабочим и врёт — хуже,
/// чем честный отказ.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_column_type_change_does_not_pass_silently() {
    let pool = support::pool_with_schema().await;
    let id = seed(
        &pool,
        "loudtype",
        None,
        "($1, 1, '55555555-5555-5555-5555-555555555555', 'step', '{}', '{\"en\":\"Шаг\"}', '{}', '', \
          null, 'required', true, '{}', false, '{}', '{}', '[]', '[]')",
    )
    .await;
    assert!(db::load_bundle_data(&pool, id).await.expect("до правки схемы")[0].steps[0].needs_human);

    sqlx::query("alter table steps alter column needs_human type text using needs_human::text")
        .execute(&pool)
        .await
        .expect("смена типа");

    // Свежий пул: в проде после применения схемы соединения новые, и кэш планов
    // (он даёт СВОЙ отказ на старом соединении) ситуацию не спасает.
    let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
    let schema: String = sqlx::query_scalar("select current_schema()").fetch_one(&pool).await.expect("схема");
    let sep = if url.contains('?') { '&' } else { '?' };
    let fresh = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&format!("{url}{sep}options=-csearch_path%3D{schema}"))
        .await
        .expect("свежий пул");

    let res = db::load_bundle_data(&fresh, id).await;

    assert!(res.is_err(), "чтение обязано ОТКАЗАТЬ, а не отдать список с потерянными пометками");
}
