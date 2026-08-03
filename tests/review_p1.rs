//! Тесты на находки авто-ревью core#63 (оба P1):
//! 1) непроецированный push (main впереди БЕЗ тега) замечается и лечится —
//!    раньше счётчики «сходились» и следующая веб-запись хоронила push;
//! 2) push не стирает надстройки Postgres (image_key, «нужен человек») —
//!    они переносятся из текущей версии по идентичности блока.
//!
//! Запуск: TEST_DATABASE_URL=... cargo test -- --include-ignored
mod support;

use std::path::Path;

use setfork_core::db::{Level, StepRow};
use setfork_core::git::bundle::{self, SerStep, VersionData};
use setfork_core::git::version::{SyncOutcome, WebEdit, commit_web_version, sync_repo_with_db};
use sqlx::postgres::PgPool;
use uuid::Uuid;

/// Каталог-однодневка: удаляется на Drop (в т.ч. при panic внутри теста).
struct Tmp(std::path::PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tmp_root(prefix: &str) -> Tmp {
    let p = std::env::temp_dir().join(format!("setfork-{prefix}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).expect("create tmp root");
    Tmp(p)
}

/// Список в БД: v1 с одним шагом «First».
async fn seed_list(pool: &PgPool, handle: &str, slug: &str, title: &str) -> Uuid {
    let owner = support::seed_user(pool, handle).await;
    let list_id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) \
         values ($1, $2, $3::jsonb, 1) returning id",
    )
    .bind(owner)
    .bind(slug)
    .bind(serde_json::json!({ "en": title }))
    .fetch_one(pool)
    .await
    .expect("seed template");
    let ver_id: Uuid = sqlx::query_scalar(
        "insert into template_versions (template_id, version, note) values ($1, 1, 'initial') returning id",
    )
    .bind(list_id)
    .fetch_one(pool)
    .await
    .expect("seed v1");
    sqlx::query(
        "insert into steps (version_id, n, type, title, level) values ($1, 1, 'step', '{\"en\":\"First\"}', 'required')",
    )
    .bind(ver_id)
    .execute(pool)
    .await
    .expect("seed step");
    list_id
}

async fn v1_data(pool: &PgPool, list_id: Uuid, title: &str) -> VersionData {
    let ts: i64 = sqlx::query_scalar(
        "select floor(extract(epoch from created_at))::bigint from template_versions \
         where template_id = $1 and version = 1",
    )
    .bind(list_id)
    .fetch_one(pool)
    .await
    .expect("v1 ts");
    VersionData {
        version: 1,
        note: "initial".into(),
        ts,
        title: title.into(),
        desc: String::new(),
        tags: vec![],
        ordered: true,
        kind: None,
        steps: vec![SerStep {
            n: 1,
            block_type: None,
            content: serde_json::Value::Null,
            block_id: None,
            title: "First".into(),
            desc: String::new(),
            command: String::new(),
            image_key: None,
            level: "required".into(),
            needs_human: false,
            needs_human_ask: None,
            why: String::new(),
            section: String::new(),
            subtasks: vec![],
            refs: vec![],
        }],
    }
}

fn step_row(title: &str) -> StepRow {
    StepRow {
        block_type: "step".into(),
        content: serde_json::json!({}),
        block_id: None,
        title: serde_json::json!({ "en": title }),
        desc: serde_json::json!({}),
        command: String::new(),
        image_key: None,
        level: Level::Required,
        why: serde_json::json!({}),
        section: serde_json::json!({}),
        subtasks: serde_json::json!([]),
        refs: serde_json::json!([]),
        needs_human: false,
        needs_human_ask: serde_json::json!({}),
    }
}

/// Коммит сырого list.json на main (валидный путь: через update_main, БЕЗ тега) —
/// имитация push-коммита, чья проекция не случилась. Возвращает sha.
fn commit_raw_list_json_on_main(bare: &Path, list_json: &[u8]) -> String {
    let repo = git2::Repository::open_bare(bare).expect("open");
    let old = repo.refname_to_id("refs/heads/main").expect("main");
    let blob = repo.blob(list_json).expect("blob");
    let parent = repo.find_commit(old).expect("parent");
    let mut tb = repo.treebuilder(Some(&parent.tree().expect("tree"))).expect("tb");
    tb.insert("list.json", blob, 0o100644).expect("insert");
    if tb.get("steps").expect("get").is_some() {
        tb.remove("steps").expect("rm");
    }
    let tree = repo.find_tree(tb.write().expect("write")).expect("tree");
    let sig =
        git2::Signature::new("Pusher", "p@example.com", &git2::Time::new(1_700_000_200, 0)).expect("sig");
    let new = repo.commit(None, &sig, &sig, "v2: user push", &tree, &[&parent]).expect("commit");
    setfork_core::git::update::update_main(&repo, new, Some(old), "test: push").expect("update main");
    new.to_string()
}

fn commit_canon_on_main(bare: &Path, canon: &serde_json::Value) -> String {
    commit_raw_list_json_on_main(bare, serde_json::to_string_pretty(canon).unwrap().as_bytes())
}

/// НЕПРОЕЦИРОВАННЫЙ PUSH ЗАМЕЧАЕТСЯ: проекция push упала до постановки тега →
/// счётчики «сошлись» (max_tag == current), но main впереди. Раньше sync
/// объявлял InSync, и следующая веб-запись клала свой коммит поверх — принятый
/// push никогда не становился версией.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn непроецированный_push_замечается_и_проецируется() {
    let pool = support::pool_with_schema().await;
    let list_id = seed_list(&pool, "untagged", "untagged", "Untagged").await;

    let root = tmp_root("untagged");
    let bare = root.0.join("repo.git");
    bundle::bootstrap_bare(&[v1_data(&pool, list_id, "Untagged").await], &bare).expect("bootstrap");

    commit_canon_on_main(
        &bare,
        &serde_json::json!({
            "title": "Untagged", "desc": "", "tags": [], "ordered": true, "version": 2,
            "steps": [
                { "n": 1, "title": "First", "desc": "", "command": "", "level": "required",
                  "why": "", "section": "", "subtasks": [], "refs": [] },
                { "n": 2, "title": "Pushed but lost", "desc": "", "command": "", "level": "required",
                  "why": "", "section": "", "subtasks": [], "refs": [] }
            ]
        }),
    );
    assert_eq!(bundle::max_tag_version(&bare), 1, "предусловие: тега на push-коммите нет");

    let sync = sync_repo_with_db(&pool, list_id, &bare).await.expect("sync");
    assert_eq!(sync, SyncOutcome::ProjectedTip { version: 2 }, "main впереди без тега → проекция tip");
    let title: serde_json::Value = sqlx::query_scalar(
        "select s.title from steps s join template_versions tv on tv.id = s.version_id \
         where tv.template_id = $1 and tv.version = 2 and s.n = 2",
    )
    .bind(list_id)
    .fetch_one(&pool)
    .await
    .expect("v2 step 2");
    assert_eq!(title["en"], "Pushed but lost", "содержимое push стало версией");
    assert_eq!(bundle::max_tag_version(&bare), 2, "тег доехал");

    // Следующая веб-запись идёт ПОВЕРХ, а не вместо.
    let out = commit_web_version(
        &pool,
        list_id,
        &bare,
        WebEdit::new("after heal", None, vec![step_row("Third")], Default::default()),
    )
    .await
    .unwrap_or_else(|e| panic!("веб-версия: {e:?}"));
    assert_eq!(out.version, 3);
}

/// БИТЫЙ TIP НЕ БЛОКИРУЕТ: push с нечитаемым list.json версии не образует (так
/// было всегда) — sync оставляет его как есть, запись ложится поверх, история цела.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn битый_tip_не_блокирует_запись() {
    let pool = support::pool_with_schema().await;
    let list_id = seed_list(&pool, "brokentip", "broken", "Broken").await;

    let root = tmp_root("broken");
    let bare = root.0.join("repo.git");
    bundle::bootstrap_bare(&[v1_data(&pool, list_id, "Broken").await], &bare).expect("bootstrap");
    // list.json ЕСТЬ (pre-receive требует наличия), но это не JSON.
    let broken_tip = commit_raw_list_json_on_main(&bare, b"definitely not json");

    let sync = sync_repo_with_db(&pool, list_id, &bare).await.expect("sync");
    assert_eq!(sync, SyncOutcome::InSync, "битый tip — не версия и не блокер");

    let out = commit_web_version(
        &pool,
        list_id,
        &bare,
        WebEdit::new("over broken", None, vec![step_row("Recovered")], Default::default()),
    )
    .await
    .unwrap_or_else(|e| panic!("веб-версия: {e:?}"));
    assert_eq!(out.version, 2);
    // Битый коммит остался в истории (родителем) — git не теряет принятое.
    let repo = git2::Repository::open_bare(&bare).expect("open");
    let tip = repo.refname_to_id("refs/heads/main").expect("main");
    let parent = repo.find_commit(tip).expect("tip").parent_id(0).expect("parent").to_string();
    assert_eq!(parent, broken_tip, "запись легла поверх, а не вместо");
}

/// PUSH НЕ СТИРАЕТ НАДСТРОЙКИ: image_key и пометка «нужен человек» переносятся
/// из текущей версии по идентичности блока — канон их не несёт, и без переноса
/// любой push молча обнулял бы их у всех шагов.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn пуш_не_стирает_картинку_и_пометку_по_идентичности() {
    const BLOCK: &str = "11111111-2222-3333-4444-555555555555";
    let pool = support::pool_with_schema().await;
    let list_id = seed_list(&pool, "carrier", "carry", "Carry").await;
    // Довесим шагу v1 идентичность, картинку и пометку (сид кладёт голый шаг).
    sqlx::query(
        "update steps set block_id = $1, image_key = 'steps/img.png', has_image = true, \
                          needs_human = true, needs_human_ask = '{\"en\":\"ask the chef\"}'::jsonb \
         where version_id = (select id from template_versions where template_id = $2 and version = 1)",
    )
    .bind(Uuid::parse_str(BLOCK).unwrap())
    .bind(list_id)
    .execute(&pool)
    .await
    .expect("enrich v1 step");

    let root = tmp_root("carry");
    let bare = root.0.join("repo.git");
    bundle::bootstrap_bare(&[v1_data(&pool, list_id, "Carry").await], &bare).expect("bootstrap");

    // «Push»: тот же блок (по идентичности) + новый блок без идентичности.
    commit_canon_on_main(
        &bare,
        &serde_json::json!({
            "title": "Carry", "desc": "", "tags": [], "ordered": true, "version": 2,
            "steps": [
                { "n": 1, "blockId": BLOCK, "title": "First", "desc": "", "command": "",
                  "level": "required", "why": "", "section": "", "subtasks": [], "refs": [] },
                { "n": 2, "title": "New one", "desc": "", "command": "", "level": "required",
                  "why": "", "section": "", "subtasks": [], "refs": [] }
            ]
        }),
    );

    let ver = setfork_core::git::project::project_pushed_commit(&pool, list_id, &bare)
        .await
        .expect("проекция")
        .expect("проецируемо");
    assert_eq!(ver, 2);

    let rows: Vec<(i32, Option<String>, bool, serde_json::Value)> = sqlx::query_as(
        "select s.n, s.image_key, s.needs_human, s.needs_human_ask from steps s \
         join template_versions tv on tv.id = s.version_id \
         where tv.template_id = $1 and tv.version = 2 order by s.n",
    )
    .bind(list_id)
    .fetch_all(&pool)
    .await
    .expect("v2 steps");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1.as_deref(), Some("steps/img.png"), "картинка пережила push");
    assert!(rows[0].2, "пометка пережила push");
    assert_eq!(rows[0].3["en"], "ask the chef");
    assert_eq!(rows[1].1, None, "новый блок без идентичности — надстроек нет");
    assert!(!rows[1].2);
}

/// СПИСОК БЕЗ СТРОК ИСТОРИИ ПРИНИМАЕТ ВЕРСИЮ (репро падения итестов фронта на
/// master): current_version — дефолт колонки, строк версий нет. Старый drizzle-
/// путь вставлял v2 не глядя; git-first обязан уметь то же — репо рождается
/// пустым, первая версия становится ПЕРВЫМ коммитом (создание main).
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn список_без_истории_принимает_первую_версию() {
    support::ensure_git_data_dir();
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "fresh").await;
    // Ровно как тестовый сид фронта: только строка списка, БЕЗ template_versions.
    let list_id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title) values ($1, 'fresh', '{\"en\":\"Fresh\"}') returning id",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .expect("seed template only");

    let bare = setfork_core::git::repo::ensure_repo_by_id(&pool, list_id)
        .await
        .expect("ensure")
        .expect("репо обязано родиться пустым, а не not found");
    let _guard = setfork_core::git::repo::repo_guard(&pool, list_id).await.expect("guard");
    let out = commit_web_version(
        &pool,
        list_id,
        &bare,
        WebEdit::new("first real", None, vec![step_row("Первый")], Default::default()),
    )
    .await
    .unwrap_or_else(|e| panic!("веб-версия: {e:?}"));

    assert_eq!(out.version, 2, "как у старого пути: current(деф.1)+1, дыра v1 легальна");
    assert_eq!(bundle::max_tag_version(&bare), 2);
    let repo = git2::Repository::open_bare(&bare).expect("open");
    let tip = repo.refname_to_id("refs/heads/main").expect("main родился");
    assert_eq!(repo.find_commit(tip).expect("tip").parent_count(), 0, "первый коммит без родителей");
    let cur: i32 = sqlx::query_scalar("select current_version from templates where id = $1")
        .bind(list_id)
        .fetch_one(&pool)
        .await
        .expect("current");
    assert_eq!(cur, 2);

    // Повторный sync на таком репо — InSync, а не Conflict.
    let again = sync_repo_with_db(&pool, list_id, &bare).await.expect("sync");
    assert_eq!(again, SyncOutcome::InSync);

    let _ = std::fs::remove_dir_all(&bare);
}
