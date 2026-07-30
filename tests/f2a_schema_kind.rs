//! Ф2a: манифест list.json — версионированный публичный контракт.
//!
//! 1) Опубликованная JSON Schema (schema/list.v1.json, draft-07) валидирует
//!    всё, что ядро реально пишет, и отвергает мусор.
//! 2) kind (тип списка, ADR-0010) выживает в полном цикле:
//!    БД → канон коммита → push на другой список → проекция → БД.
//!
//! Интеграционные: TEST_DATABASE_URL=... cargo test -- --include-ignored
mod support;

use std::path::Path;

use setfork_core::db::{Level, StepRow};
use setfork_core::git::bundle::{self, SerStep, VersionData};
use setfork_core::git::serialize::{list_json, schema_url};
use setfork_core::git::version::commit_web_version;
use sqlx::postgres::PgPool;
use uuid::Uuid;

fn schema() -> serde_json::Value {
    let raw = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/list.v1.json"))
        .expect("schema/list.v1.json");
    serde_json::from_str(&raw).expect("схема — валидный JSON")
}

fn ser_step(n: i32, title: &str) -> SerStep {
    SerStep {
        n,
        block_type: None,
        content: serde_json::Value::Null,
        block_id: Some("11111111-2222-3333-4444-555555555555".into()),
        title: title.into(),
        desc: "desc".into(),
        command: "echo hi".into(),
        image_key: None,
        level: "recommended".into(),
        needs_human: false,
        needs_human_ask: None,
        why: "why".into(),
        section: "Setup".into(),
        subtasks: vec!["sub".into()],
        refs: vec![
            setfork_core::git::bundle::StepRef {
                label: "docs".into(),
                url: Some("https://x.example".into()),
            },
            setfork_core::git::bundle::StepRef { label: "без url".into(), url: None },
        ],
    }
}

fn ver(kind: Option<&str>) -> VersionData {
    VersionData {
        version: 3,
        note: "n".into(),
        ts: 0,
        title: "L".into(),
        desc: "D".into(),
        tags: vec!["t".into()],
        ordered: true,
        kind: kind.map(str::to_string),
        steps: vec![
            ser_step(1, "First"),
            SerStep {
                n: 2,
                block_type: Some("text".into()),
                content: serde_json::json!({ "md": "intro" }),
                block_id: None,
                title: String::new(),
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
            },
        ],
    }
}

/// Всё, что пишет ядро, валидно по опубликованной схеме — с kind и без,
/// с шагами и презентационными блоками, с url и без.
#[test]
fn канон_валиден_по_опубликованной_схеме() {
    let schema = schema();
    for v in [ver(None), ver(Some("recipe"))] {
        let canon: serde_json::Value = serde_json::from_str(&list_json(&v)).expect("канон — JSON");
        if let Err(e) = jsonschema::draft7::validate(&schema, &canon) {
            panic!("канон обязан быть валиден: {e}");
        }
    }
}

/// Схема строгая: мусорный kind, потерянный title и посторонний ключ — отказ.
/// Без этого схема была бы украшением, а не контрактом.
#[test]
fn схема_отвергает_мусор() {
    let schema = schema();
    let good: serde_json::Value = serde_json::from_str(&list_json(&ver(Some("recipe")))).expect("json");

    let mut bad_kind = good.clone();
    bad_kind["kind"] = serde_json::json!("салат");
    assert!(jsonschema::draft7::validate(&schema, &bad_kind).is_err(), "kind вне реестра");

    let mut no_title = good.clone();
    no_title.as_object_mut().unwrap().remove("title");
    assert!(jsonschema::draft7::validate(&schema, &no_title).is_err(), "title обязателен");

    let mut extra = good.clone();
    extra["surprise"] = serde_json::json!(1);
    assert!(jsonschema::draft7::validate(&schema, &extra).is_err(), "посторонний ключ корня");

    let mut bad_level = good;
    bad_level["steps"][0]["level"] = serde_json::json!("mandatory");
    assert!(jsonschema::draft7::validate(&schema, &bad_level).is_err(), "level вне enum");
}

/// $id схемы == дефолтный schema_url: файл и ссылка из канона — одна сущность.
#[test]
fn id_схемы_совпадает_с_умолчанием_schema_url() {
    assert_eq!(schema()["$id"], serde_json::json!(schema_url()));
}

// ── Интеграционные: kind сквозь полный цикл ──────────────────────────────

async fn seed_list(pool: &PgPool, handle: &str, slug: &str, kind: Option<&str>) -> Uuid {
    let owner = support::seed_user(pool, handle).await;
    let list_id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, list_kind) values ($1, $2, '{\"en\":\"K\"}', $3) returning id",
    )
    .bind(owner)
    .bind(slug)
    .bind(kind)
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

fn tip_list_json(bare: &Path) -> Vec<u8> {
    let repo = git2::Repository::open_bare(bare).expect("open bare");
    let tip = repo.refname_to_id("refs/heads/main").expect("main");
    let commit = repo.find_commit(tip).expect("commit");
    let entry = commit.tree().expect("tree").get_path(Path::new("list.json")).expect("list.json");
    repo.find_blob(entry.id()).expect("blob").content().to_vec()
}

fn push_canon(bare: &Path, list_json: &[u8]) {
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
        git2::Signature::new("Pusher", "p@example.com", &git2::Time::new(1_700_000_500, 0)).expect("sig");
    let new = repo.commit(None, &sig, &sig, "v2: push", &tree, &[&parent]).expect("commit");
    setfork_core::git::update::update_main(&repo, new, Some(old), "test: push").expect("update");
}

/// БД → канон → push на другой список → проекция → БД: kind доезжает,
/// и раньше терялся именно на push (проекция его не читала).
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn kind_переживает_полный_цикл() {
    let pool = support::pool_with_schema().await;
    let a_id = seed_list(&pool, "kind-a", "kind", Some("recipe")).await;
    let b_id = seed_list(&pool, "kind-b", "kind", None).await;

    let tmp = std::env::temp_dir().join(format!("setfork-kind-{}", Uuid::new_v4()));
    let bare_a = tmp.join("a.git");
    let bare_b = tmp.join("b.git");
    let history_a = setfork_core::db::load_bundle_data(&pool, a_id).await.expect("history A");
    assert_eq!(history_a[0].kind.as_deref(), Some("recipe"), "kind из БД в VersionData");
    bundle::bootstrap_bare(&history_a, &bare_a).expect("bootstrap A");
    bundle::bootstrap_bare(
        &setfork_core::db::load_bundle_data(&pool, b_id).await.expect("history B"),
        &bare_b,
    )
    .expect("bootstrap B");

    // Веб-версия на A: канон несёт kind.
    let out =
        commit_web_version(&pool, a_id, &bare_a, "web", None, vec![step_row("Second")], Default::default())
            .await
            .unwrap_or_else(|e| panic!("веб-версия: {e:?}"));
    assert_eq!(out.version, 2);
    let canon = tip_list_json(&bare_a);
    let parsed: serde_json::Value = serde_json::from_slice(&canon).expect("json");
    assert_eq!(parsed["kind"], serde_json::json!("recipe"), "kind в каноне веб-версии");

    // Push ровно этого канона на B (kind там не выставлен) → проекция приносит kind.
    push_canon(&bare_b, &canon);
    let ver = setfork_core::git::project::project_pushed_commit(&pool, b_id, &bare_b)
        .await
        .expect("проекция")
        .expect("проецируемо");
    assert_eq!(ver, 2);
    let got: Option<String> = sqlx::query_scalar("select list_kind from templates where id = $1")
        .bind(b_id)
        .fetch_one(&pool)
        .await
        .expect("list_kind B");
    assert_eq!(got.as_deref(), Some("recipe"), "push принёс тип списка — раньше терялся");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Мусорный kind из чужого push не попадает в БД (санитизация, как block_id),
/// а отсутствие поля не стирает выставленный тип.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn мусорный_kind_отбрасывается_а_отсутствие_не_стирает() {
    let pool = support::pool_with_schema().await;
    let list_id = seed_list(&pool, "kind-c", "kindc", Some("checklist")).await;

    let tmp = std::env::temp_dir().join(format!("setfork-kindc-{}", Uuid::new_v4()));
    let bare = tmp.join("c.git");
    bundle::bootstrap_bare(
        &setfork_core::db::load_bundle_data(&pool, list_id).await.expect("history"),
        &bare,
    )
    .expect("bootstrap");

    // Push с мусорным kind (руками сломанный канон: схему никто не обязан уважать).
    let mut parsed: serde_json::Value = serde_json::from_slice(&tip_list_json(&bare)).expect("json");
    parsed["kind"] = serde_json::json!("салат");
    parsed["version"] = serde_json::json!(2);
    push_canon(&bare, serde_json::to_string_pretty(&parsed).unwrap().as_bytes());
    setfork_core::git::project::project_pushed_commit(&pool, list_id, &bare)
        .await
        .expect("проекция")
        .expect("проецируемо");
    let got: Option<String> = sqlx::query_scalar("select list_kind from templates where id = $1")
        .bind(list_id)
        .fetch_one(&pool)
        .await
        .expect("list_kind");
    assert_eq!(got.as_deref(), Some("checklist"), "мусор не затёр выставленный тип");

    // Push БЕЗ kind (старый клон) — тип тоже не стирается.
    let mut no_kind: serde_json::Value = serde_json::from_slice(&tip_list_json(&bare)).expect("json");
    no_kind.as_object_mut().unwrap().remove("kind");
    no_kind["version"] = serde_json::json!(3);
    push_canon(&bare, serde_json::to_string_pretty(&no_kind).unwrap().as_bytes());
    setfork_core::git::project::project_pushed_commit(&pool, list_id, &bare)
        .await
        .expect("проекция")
        .expect("проецируемо");
    let got: Option<String> = sqlx::query_scalar("select list_kind from templates where id = $1")
        .bind(list_id)
        .fetch_one(&pool)
        .await
        .expect("list_kind");
    assert_eq!(got.as_deref(), Some("checklist"), "push старого клона без kind не стирает тип");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Ф2a-довесок: картинка и пометка — содержимое канона. Файл — источник
/// (проекция читает их у списка, где carry взять неоткуда), явный false
/// снимает пометку пушем, recovery из канона возвращает картинку.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn картинка_и_пометка_переживают_полный_цикл() {
    const BLOCK: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let pool = support::pool_with_schema().await;
    let a_id = seed_list(&pool, "img-a", "img", None).await;
    let b_id = seed_list(&pool, "img-b", "img", None).await;
    // A: шаг с идентичностью, картинкой и пометкой.
    sqlx::query(
        "update steps set block_id = $1, image_key = 'steps/pan.png', has_image = true,                           needs_human = true, needs_human_ask = '{\"en\":\"how hot is your oven\"}'::jsonb          where version_id = (select id from template_versions where template_id = $2 and version = 1)",
    )
    .bind(Uuid::parse_str(BLOCK).unwrap())
    .bind(a_id)
    .execute(&pool)
    .await
    .expect("enrich A");

    let tmp = std::env::temp_dir().join(format!("setfork-img-{}", Uuid::new_v4()));
    let bare_a = tmp.join("a.git");
    let bare_b = tmp.join("b.git");
    bundle::bootstrap_bare(&setfork_core::db::load_bundle_data(&pool, a_id).await.expect("hist A"), &bare_a)
        .expect("bootstrap A");
    bundle::bootstrap_bare(&setfork_core::db::load_bundle_data(&pool, b_id).await.expect("hist B"), &bare_b)
        .expect("bootstrap B");

    // Канон A несёт новые поля прямо из bootstrap-истории.
    let canon = tip_list_json(&bare_a);
    let parsed: serde_json::Value = serde_json::from_slice(&canon).expect("json");
    assert_eq!(parsed["steps"][0]["imageKey"], serde_json::json!("steps/pan.png"));
    assert_eq!(parsed["steps"][0]["needsHuman"], serde_json::json!(true));
    assert_eq!(parsed["steps"][0]["needsHumanAsk"], serde_json::json!("how hot is your oven"));
    // И валиден по схеме.
    let schema = schema();
    jsonschema::draft7::validate(&schema, &parsed).expect("канон с новыми полями валиден");

    // Push ровно этого канона на B: у B carry пуст — поля обязаны прийти ИЗ ФАЙЛА.
    let mut v2 = parsed.clone();
    v2["version"] = serde_json::json!(2);
    push_canon(&bare_b, serde_json::to_string_pretty(&v2).unwrap().as_bytes());
    let ver = setfork_core::git::project::project_pushed_commit(&pool, b_id, &bare_b)
        .await
        .expect("проекция")
        .expect("проецируемо");
    assert_eq!(ver, 2);
    let (ik, nh, ask): (Option<String>, bool, serde_json::Value) = sqlx::query_as(
        "select s.image_key, s.needs_human, s.needs_human_ask from steps s          join template_versions tv on tv.id = s.version_id          where tv.template_id = $1 and tv.version = 2 and s.n = 1",
    )
    .bind(b_id)
    .fetch_one(&pool)
    .await
    .expect("B v2 step");
    assert_eq!(ik.as_deref(), Some("steps/pan.png"), "картинка пришла из файла (recovery работает)");
    assert!(nh, "пометка пришла из файла");
    assert_eq!(ask["en"], "how hot is your oven");

    // Явный false в файле СНИМАЕТ пометку (тристейт).
    let mut v3 = parsed.clone();
    v3["version"] = serde_json::json!(3);
    v3["steps"][0]["needsHuman"] = serde_json::json!(false);
    push_canon(&bare_b, serde_json::to_string_pretty(&v3).unwrap().as_bytes());
    let ver = setfork_core::git::project::project_pushed_commit(&pool, b_id, &bare_b)
        .await
        .expect("проекция")
        .expect("проецируемо");
    assert_eq!(ver, 3);
    let (nh, ask): (bool, serde_json::Value) = sqlx::query_as(
        "select s.needs_human, s.needs_human_ask from steps s          join template_versions tv on tv.id = s.version_id          where tv.template_id = $1 and tv.version = 3 and s.n = 1",
    )
    .bind(b_id)
    .fetch_one(&pool)
    .await
    .expect("B v3 step");
    assert!(!nh, "явный false снял пометку пушем");
    assert_eq!(ask, serde_json::json!({}), "вопрос погашен вместе с пометкой");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Ф2a-довесок: мета применяется ТОЙ ЖЕ транзакцией, что и версия, и коммит
/// сразу несёт свежие title/tags/ordered. Пустой title игнорируется.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn мета_едет_вместе_с_версией_и_попадает_в_канон() {
    let pool = support::pool_with_schema().await;
    let list_id = seed_list(&pool, "meta-a", "meta", None).await;

    let tmp = std::env::temp_dir().join(format!("setfork-meta-{}", Uuid::new_v4()));
    let bare = tmp.join("m.git");
    bundle::bootstrap_bare(&setfork_core::db::load_bundle_data(&pool, list_id).await.expect("hist"), &bare)
        .expect("bootstrap");

    let meta = setfork_core::git::version::MetaPatch {
        title: Some(serde_json::json!({ "en": "Renamed", "ru": "Переименован" })),
        desc: None,
        tags: Some(vec!["baking".into(), "flour".into()]),
        ordered: Some(false),
    };
    let out = commit_web_version(&pool, list_id, &bare, "with meta", None, vec![step_row("Mix")], meta)
        .await
        .unwrap_or_else(|e| panic!("веб-версия: {e:?}"));
    assert_eq!(out.version, 2);

    // Канон коммита сразу несёт свежую мету (en-проекция).
    let parsed: serde_json::Value = serde_json::from_slice(&tip_list_json(&bare)).expect("json");
    assert_eq!(parsed["title"], serde_json::json!("Renamed"));
    assert_eq!(parsed["tags"], serde_json::json!(["baking", "flour"]));
    assert_eq!(parsed["ordered"], serde_json::json!(false));

    // БД обновлена той же транзакцией (полный LocaleText, не en-срез).
    let (title, tags, ordered): (serde_json::Value, Vec<String>, bool) =
        sqlx::query_as("select title, tags, ordered from templates where id = $1")
            .bind(list_id)
            .fetch_one(&pool)
            .await
            .expect("templates");
    assert_eq!(title["ru"], "Переименован", "БД хранит полный LocaleText");
    assert_eq!(tags, vec!["baking".to_string(), "flour".to_string()]);
    assert!(!ordered);

    // Пустой title игнорируется — название обязательно.
    let bad = setfork_core::git::version::MetaPatch {
        title: Some(serde_json::json!({ "en": "  " })),
        ..Default::default()
    };
    commit_web_version(&pool, list_id, &bare, "empty title", None, vec![step_row("Mix")], bad)
        .await
        .unwrap_or_else(|e| panic!("веб-версия: {e:?}"));
    let title: serde_json::Value = sqlx::query_scalar("select title from templates where id = $1")
        .bind(list_id)
        .fetch_one(&pool)
        .await
        .expect("title");
    assert_eq!(title["en"], "Renamed", "пустой title не затёр название");

    let _ = std::fs::remove_dir_all(&tmp);
}
