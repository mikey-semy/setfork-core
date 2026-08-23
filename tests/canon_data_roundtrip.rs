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
async fn круг_сохраняет_все_поля_списка() {
    let pool = support::pool_with_schema().await;
    let id = seed(&pool, "roundfull", Some("checklist"), FULL_STEPS).await;

    let (before, after) = round(&pool, id).await;

    // Сперва — что круг вообще НЕ ЗЕЛЁН ПО ПОСТРОЕНИЮ. Надстройки Postgres
    // (imageKey, needsHuman, danger) умеет возвращать перенос по blockId, и
    // сравнение «до/после» прошло бы, даже если бы файл их не нёс вовсе. Поэтому
    // отдельно требуем, чтобы их нёс САМ канон: он и есть предмет этой линзы.
    for key in [
        "\"blockId\"",
        "\"imageKey\"",
        "\"needsHuman\"",
        "\"needsHumanAsk\"",
        "\"danger\"",
        "\"kind\"",
        "\"refs\"",
        "\"subtasks\"",
        "\"section\"",
        "\"why\"",
    ] {
        assert!(before.contains(key), "канон не несёт {key} — переносу нечего проверять:\n{before}");
    }

    let (a, b) = same_but_version(&before, &after);
    assert_eq!(a, b, "круг потерял поля:\nДО:\n{before}\nПОСЛЕ:\n{after}");
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn круг_держит_список_без_шагов_и_список_из_одного_блока() {
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
async fn блок_с_негодным_content_не_роняет_проекцию_и_не_врёт_молча() {
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
