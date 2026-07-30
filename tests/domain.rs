//! Интеграционные тесты доменных сервисов (ListRead/Write, Curation, Collab)
//! против настоящего Postgres. Все тесты #[ignore]: запуск —
//! `TEST_DATABASE_URL=... cargo test -- --include-ignored`
//! (или `bash scripts/ci-local.sh`, он поднимет эфемерный Postgres сам).
mod support;

use setfork_core::db;
use setfork_core::pb_domain::collab_write_server::CollabWrite;
use setfork_core::pb_domain::curation_read_server::CurationRead;
use setfork_core::pb_domain::curation_write_server::CurationWrite;
use setfork_core::pb_domain::list_read_server::ListRead;
use setfork_core::pb_domain::list_write_server::ListWrite;
use setfork_core::pb_domain::{
    AddIssueCommentRequest, AddVersionRequest, CreateListRequest, CreateSuggestionRequest, GetVersionRequest,
    ListId, ListRef, LocaleText, NewStep, OpenIssueRequest, SetIssueStatusRequest, UserList,
};
use setfork_core::services::collab::CollabWriteSvc;
use setfork_core::services::curation::{CurationReadSvc, CurationWriteSvc};
use setfork_core::services::list::{ListReadSvc, ListWriteSvc};
use tonic::Request;

fn lt(pairs: &[(&str, &str)]) -> Option<LocaleText> {
    Some(LocaleText { v: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() })
}

fn step(title_en: &str) -> NewStep {
    NewStep {
        block_id: String::new(),
        title: lt(&[("en", title_en)]),
        desc: lt(&[("en", "step desc")]),
        command: "echo hi".into(),
        level: "recommended".into(),
        why: lt(&[("en", "because")]),
        section: lt(&[("en", "Setup")]),
        subtasks: vec![lt(&[("en", "sub")]).unwrap()],
        refs: vec![],
        image_ref: String::new(),
        r#type: String::new(),
        content_json: String::new(),
        needs_human: false,
        needs_human_ask: None,
    }
}

/// Шаг с пометкой «здесь нужен человек»: место, где машина знать не может.
fn step_needing_human(title_en: &str, ask_en: &str) -> NewStep {
    NewStep { needs_human: true, needs_human_ask: lt(&[("en", ask_en)]), ..step(title_en) }
}

fn text_block(md: &str) -> NewStep {
    NewStep { r#type: "text".into(), content_json: format!(r#"{{"md":"{md}"}}"#), ..step("") }
}

fn create_req(owner_id: &str, slug: &str) -> CreateListRequest {
    CreateListRequest {
        owner_id: owner_id.into(),
        slug: slug.into(),
        title: lt(&[("en", "Test List"), ("ru", "Тестовый список")]),
        desc: lt(&[("en", "descr")]),
        tags: vec!["redis".into()],
        ordered: true,
        visibility: String::new(), // дефолт public
        status: String::new(),     // дефолт published
        origin: String::new(),     // дефолт authored
        forked_from_id: String::new(),
        note: "initial".into(),
        steps: vec![step("Install"), text_block("intro **md**"), step("Configure")],
    }
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn needs_human_survives_domain_write() {
    // ДОЛГ КАТОВЕРА (закрыт 2026-07-28). Набор шагов перезаписывается ЦЕЛИКОМ, поэтому поле,
    // о котором путь записи не знает, не «остаётся прежним», а ИСЧЕЗАЕТ. Ровно так пометка
    // «здесь нужен человек» терялась во фронте (починено там же), и ровно так она терялась бы
    // здесь при SETFORK_DOMAIN_WRITES=1 — только тише, потому что доменная запись выключена.
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "alice").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let read = ListReadSvc { pool: pool.clone() };

    let mut req = create_req(&owner.to_string(), "marked-list");
    req.steps = vec![step_needing_human("Купить муку", "Сколько стоит у вас?"), step("Замесить")];
    let created = write.create(Request::new(req)).await.expect("create").into_inner();

    let ver = read
        .get_version(Request::new(GetVersionRequest { list_id: created.id.clone(), version: 1 }))
        .await
        .expect("get_version")
        .into_inner();
    assert!(ver.found);
    assert!(ver.steps[0].needs_human, "пометка обязана пережить запись через домен");
    assert_eq!(ver.steps[0].needs_human_ask.as_ref().unwrap().v["en"], "Сколько стоит у вас?");
    assert!(!ver.steps[1].needs_human, "непомеченный шаг остаётся непомеченным");
    assert!(
        ver.steps[1].needs_human_ask.as_ref().map(|a| a.v.is_empty()).unwrap_or(true),
        "вопрос без пометки — висячий текст, его быть не должно",
    );
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn needs_human_survives_git_projection() {
    // В list.json пометка НЕ пишется (golden-паритет), поэтому из git она прийти не может.
    // Если бы проекция писала false, каждый push молча стирал бы честные пометки. Переносим
    // по block_id: идентичность блока git как раз несёт.
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "alice").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let read = ListReadSvc { pool: pool.clone() };

    let bid = uuid::Uuid::new_v4().to_string();
    let mut req = create_req(&owner.to_string(), "pushed-list");
    let mut marked = step_needing_human("Купить муку", "Сколько стоит у вас?");
    marked.block_id = bid.clone();
    req.steps = vec![marked];
    let created = write.create(Request::new(req)).await.expect("create").into_inner();
    let list_id = uuid::Uuid::parse_str(&created.id).expect("uuid");

    // Проекция push: тот же блок по идентичности, но без пометки — как приходит из git.
    let proj = vec![setfork_core::git::project::ProjStep {
        block_type: "step".into(),
        content: serde_json::Value::Null,
        block_id: Some(bid.clone()),
        title: "Купить муку".into(),
        desc: "".into(),
        command: "".into(),
        level: "required".into(),
        why: "".into(),
        section: "".into(),
        subtasks: vec![],
        refs: vec![],
    }];
    let v2 = setfork_core::db::add_version(&pool, list_id, "push", &proj).await.expect("add_version");

    let ver = read
        .get_version(Request::new(GetVersionRequest { list_id: created.id.clone(), version: v2 }))
        .await
        .expect("get_version")
        .into_inner();
    assert!(ver.steps[0].needs_human, "push НЕ должен стирать пометку — переносится по block_id");
    assert_eq!(ver.steps[0].needs_human_ask.as_ref().unwrap().v["en"], "Сколько стоит у вас?");
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn create_and_read_roundtrip() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "alice").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let read = ListReadSvc { pool: pool.clone() };

    let created = write
        .create(Request::new(create_req(&owner.to_string(), "test-list")))
        .await
        .expect("create")
        .into_inner();
    assert_eq!(created.slug, "test-list");
    assert_eq!(created.current_version, 1);
    assert_eq!(created.visibility, "public");
    assert_eq!(created.status, "published");
    assert_eq!(created.moderation, "active");
    assert_eq!(created.repository_id, created.id, "synthetic solo-repo");

    let got = read
        .get_list(Request::new(ListRef { owner: "alice".into(), slug: "test-list".into() }))
        .await
        .expect("get_list")
        .into_inner();
    assert!(got.found);
    let l = got.list.expect("list");
    assert_eq!(l.id, created.id);
    assert_eq!(l.title.as_ref().unwrap().v["ru"], "Тестовый список");
    assert_eq!(l.tags, vec!["redis".to_string()]);

    let ver = read
        .get_version(Request::new(GetVersionRequest { list_id: l.id.clone(), version: 1 }))
        .await
        .expect("get_version")
        .into_inner();
    assert!(ver.found);
    assert_eq!(ver.steps.len(), 3);
    // Шаг: type пуст, LocaleText/level сохранены.
    let s0 = &ver.steps[0];
    assert_eq!(s0.r#type, "");
    assert_eq!(s0.title.as_ref().unwrap().v["en"], "Install");
    assert_eq!(s0.level, "recommended");
    assert_eq!(s0.subtasks.len(), 1);
    // Блок: type/content_json проехали в обе стороны.
    let s1 = &ver.steps[1];
    assert_eq!(s1.r#type, "text");
    let content: serde_json::Value = serde_json::from_str(&s1.content_json).expect("content json");
    assert_eq!(content["md"], "intro **md**");

    // Резолв и счётчик публичных — те же данные видны db-слою.
    let resolved = db::resolve_list(&pool, "alice", "test-list").await.expect("resolve");
    assert_eq!(resolved.map(|(_, v)| v), Some(1));
    assert_eq!(db::published_count(&pool).await.expect("count"), 1);
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn add_version_bumps_current_and_orders_desc() {
    support::ensure_git_data_dir(); // git-first: add_version коммитит в репо
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "bob").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let read = ListReadSvc { pool: pool.clone() };

    let created =
        write.create(Request::new(create_req(&owner.to_string(), "l"))).await.expect("create").into_inner();

    let v2 = write
        .add_version(Request::new(AddVersionRequest {
            list_id: created.id.clone(),
            note: "second".into(),
            steps: vec![step("Only")],
            author_id: String::new(), // '' = null (автор версии; тест не про авторство)
        }))
        .await
        .expect("add_version")
        .into_inner();
    assert_eq!(v2.version, 2);
    assert_eq!(v2.note, "second");
    // Git-first: версия — это коммит; ответ несёт его sha (раньше поле было пустым).
    assert_eq!(v2.commit_sha.len(), 40, "sha коммита версии в ответе: {:?}", v2.commit_sha);

    let got = read
        .get_list(Request::new(ListRef { owner: "bob".into(), slug: "l".into() }))
        .await
        .expect("get_list")
        .into_inner();
    assert_eq!(got.list.unwrap().current_version, 2, "current_version поднят");

    let versions = read
        .list_versions(Request::new(ListId { id: created.id.clone() }))
        .await
        .expect("list_versions")
        .into_inner()
        .versions;
    assert_eq!(versions.iter().map(|v| v.version).collect::<Vec<_>>(), vec![2, 1], "по убыванию version");
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn add_version_unknown_list_is_not_found() {
    support::ensure_git_data_dir(); // git-first: add_version резолвит путь репо
    let pool = support::pool_with_schema().await;
    let write = ListWriteSvc { pool: pool.clone() };
    let err = write
        .add_version(Request::new(AddVersionRequest {
            list_id: uuid::Uuid::new_v4().to_string(),
            note: String::new(),
            steps: vec![],
            author_id: String::new(),
        }))
        .await
        .expect_err("несуществующий список");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn create_duplicate_slug_errors() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "carol").await;
    let write = ListWriteSvc { pool: pool.clone() };
    write.create(Request::new(create_req(&owner.to_string(), "dup"))).await.expect("первый create");
    let err = write
        .create(Request::new(create_req(&owner.to_string(), "dup")))
        .await
        .expect_err("дубль slug должен падать");
    // Таксономия Фазы 4: unique violation (23505) → ALREADY_EXISTS, без утечки SQL.
    assert_eq!(err.code(), tonic::Code::AlreadyExists);
    assert!(!err.message().contains("duplicate key"), "детали Postgres не текут клиенту");
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn star_toggle_moves_counter_transactionally() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "dave").await;
    let fan = support::seed_user(&pool, "fan").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let cur_w = CurationWriteSvc { pool: pool.clone() };
    let cur_r = CurationReadSvc { pool: pool.clone() };
    let read = ListReadSvc { pool: pool.clone() };

    let created = write
        .create(Request::new(create_req(&owner.to_string(), "starred")))
        .await
        .expect("create")
        .into_inner();
    let ul = UserList { list_id: created.id.clone(), user_id: fan.to_string() };

    let on = cur_w.toggle_star(Request::new(ul.clone())).await.expect("on").into_inner();
    assert!(on.value, "toggle возвращает НОВОЕ состояние");
    assert!(cur_r.is_starred(Request::new(ul.clone())).await.unwrap().into_inner().value);
    let l = read
        .get_list(Request::new(ListRef { owner: "dave".into(), slug: "starred".into() }))
        .await
        .unwrap()
        .into_inner()
        .list
        .unwrap();
    assert_eq!(l.stars_count, 1, "счётчик двинулся в той же транзакции");

    let off = cur_w.toggle_star(Request::new(ul.clone())).await.expect("off").into_inner();
    assert!(!off.value);
    let l = read
        .get_list(Request::new(ListRef { owner: "dave".into(), slug: "starred".into() }))
        .await
        .unwrap()
        .into_inner()
        .list
        .unwrap();
    assert_eq!(l.stars_count, 0, "GREATEST(-1, 0): не уходит в минус");
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn watch_flow_idempotent() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "eve").await;
    let watcher = support::seed_user(&pool, "watcher").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let cur_w = CurationWriteSvc { pool: pool.clone() };
    let cur_r = CurationReadSvc { pool: pool.clone() };

    let created = write
        .create(Request::new(create_req(&owner.to_string(), "watched")))
        .await
        .expect("create")
        .into_inner();
    let ul = UserList { list_id: created.id.clone(), user_id: watcher.to_string() };
    let lid = ListId { id: created.id.clone() };

    assert!(cur_w.toggle_watch(Request::new(ul.clone())).await.unwrap().into_inner().value);
    // ensure_watch поверх существующего — идемпотентен (on conflict do nothing).
    cur_w.ensure_watch(Request::new(ul.clone())).await.expect("ensure");
    assert_eq!(cur_r.watch_count(Request::new(lid.clone())).await.unwrap().into_inner().value, 1);
    assert_eq!(
        cur_r.watcher_ids(Request::new(lid.clone())).await.unwrap().into_inner().ids,
        vec![watcher.to_string()]
    );
    assert!(!cur_w.toggle_watch(Request::new(ul.clone())).await.unwrap().into_inner().value);
    assert_eq!(cur_r.watch_count(Request::new(lid)).await.unwrap().into_inner().value, 0);
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn issues_numbering_status_and_comments() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "frank").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let collab = CollabWriteSvc { pool: pool.clone() };

    let created = write
        .create(Request::new(create_req(&owner.to_string(), "issued")))
        .await
        .expect("create")
        .into_inner();

    let i1 = collab
        .open_issue(Request::new(OpenIssueRequest {
            list_id: created.id.clone(),
            author_id: owner.to_string(),
            title: "first".into(),
            body: "body".into(),
            labels: vec!["bug".into()],
        }))
        .await
        .expect("issue 1")
        .into_inner();
    let i2 = collab
        .open_issue(Request::new(OpenIssueRequest {
            list_id: created.id.clone(),
            author_id: owner.to_string(),
            title: "second".into(),
            body: String::new(),
            labels: vec![],
        }))
        .await
        .expect("issue 2")
        .into_inner();
    assert_eq!((i1.number, i2.number), (1, 2), "авто-нумерация per-список");
    assert_eq!(i1.status, "open");
    assert_eq!(i1.closed_at, 0, "0 = null");

    let c = collab
        .add_issue_comment(Request::new(AddIssueCommentRequest {
            issue_id: i1.id.clone(),
            author_id: owner.to_string(),
            body: "comment".into(),
        }))
        .await
        .expect("comment")
        .into_inner();
    assert!(c.created_at > 0);

    collab
        .set_issue_status(Request::new(SetIssueStatusRequest {
            issue_id: i1.id.clone(),
            status: "closed".into(),
        }))
        .await
        .expect("close");
    let (status, closed): (String, Option<i64>) = sqlx::query_as(
        "select status::text, floor(extract(epoch from closed_at) * 1000)::bigint from issues where id = $1::uuid",
    )
    .bind(&i1.id)
    .fetch_one(&pool)
    .await
    .expect("проверка статуса");
    assert_eq!(status, "closed");
    assert!(closed.is_some(), "closed_at выставлен");
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn suggestion_roundtrip_and_contributors() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "grace").await;
    let author = support::seed_user(&pool, "helper").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let read = ListReadSvc { pool: pool.clone() };
    let collab = CollabWriteSvc { pool: pool.clone() };

    let created = write
        .create(Request::new(create_req(&owner.to_string(), "suggested")))
        .await
        .expect("create")
        .into_inner();

    let sug = collab
        .create_suggestion(Request::new(CreateSuggestionRequest {
            list_id: created.id.clone(),
            author_id: author.to_string(),
            note: "improve".into(),
            steps: vec![step("Proposed"), text_block("ctx")],
        }))
        .await
        .expect("suggestion")
        .into_inner();
    assert_eq!(sug.base_version, 1, "base = current_version на момент создания");
    assert_eq!(sug.status, "open");
    assert_eq!(sug.steps.len(), 2, "steps вернулись из jsonb");
    assert_eq!(sug.steps[0].title.as_ref().unwrap().v["en"], "Proposed");
    assert_eq!(sug.steps[1].r#type, "text");

    // Принятое предложение считается в contributors.
    sqlx::query("update suggestions set status = 'accepted' where id = $1::uuid")
        .bind(&sug.id)
        .execute(&pool)
        .await
        .expect("accept");
    let contribs = read
        .get_contributors(Request::new(ListId { id: created.id.clone() }))
        .await
        .expect("contributors")
        .into_inner()
        .contributors;
    assert_eq!(contribs.len(), 2);
    assert_eq!(contribs[0].handle, "grace", "владелец первым");
    assert_eq!(contribs[0].accepted, 0, "TS-паритет: у владельца 0");
    assert_eq!(contribs[1].handle, "helper");
    assert_eq!(contribs[1].accepted, 1);
}
