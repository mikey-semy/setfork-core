//! Ф4 «правка списка как кода»: контракт двух RPC, на которых держится редактор.
//!
//! Главное свойство — КРУГ: текст, который ядро показало человеку (`RenderCanon`),
//! оно же принимает обратно без единой придирки (`ParseCanon`), и содержимое после
//! круга то же самое. Разъезд здесь означал бы худшее для редактора кода: показали
//! одно, а сохранить не дают (или сохраняют другое).
//!
//! Интеграционные: TEST_DATABASE_URL=... cargo test -- --include-ignored
mod support;

use setfork_core::pb::git_core_server::GitCore;
use setfork_core::pb::{
    ListContent, ParseCanonRequest, RenderCanonRequest, RepoRef, SnapshotRef, SnapshotStep,
};
use setfork_core::services::git_core::GitCoreSvc;
use sqlx::postgres::PgPool;
use tonic::Request;
use uuid::Uuid;

async fn seed_list(pool: &PgPool, handle: &str, slug: &str) -> Uuid {
    let owner = support::seed_user(pool, handle).await;
    sqlx::query_scalar(
        "insert into templates (owner_id, slug, title) values ($1, $2, '{\"en\":\"C\"}') returning id",
    )
    .bind(owner)
    .bind(slug)
    .fetch_one(pool)
    .await
    .expect("seed template")
}

fn content() -> ListContent {
    ListContent {
        title: "Развернуть стенд".into(),
        desc: "Проверка круга".into(),
        tags: vec!["ops".into()],
        ordered: true,
        version: 1,
        steps: vec![
            SnapshotStep {
                n: 1,
                title: "Поставить docker".into(),
                desc: "по инструкции".into(),
                command: "apt install docker".into(),
                level: "required".into(),
                why: "без него нечем запускать".into(),
                section: "Подготовка".into(),
                subtasks: vec!["проверить версию".into()],
                refs: vec![SnapshotRef { label: "docs".into(), url: "https://docs.docker.com".into() }],
                r#type: String::new(),
                content_json: String::new(),
                block_id: String::new(),
                danger: false,
                image_key: String::new(),
                needs_human: false,
                needs_human_ask: String::new(),
            },
            SnapshotStep {
                n: 2,
                title: String::new(),
                desc: String::new(),
                command: String::new(),
                level: "required".into(),
                why: String::new(),
                section: String::new(),
                subtasks: vec![],
                refs: vec![],
                r#type: "text".into(),
                content_json: r#"{"md":"пояснение"}"#.into(),
                block_id: String::new(),
                danger: false,
                image_key: String::new(),
                needs_human: false,
                needs_human_ask: String::new(),
            },
        ],
    }
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn the_shown_canon_is_accepted_back_without_complaints() {
    let pool = support::pool_with_schema().await;
    seed_list(&pool, "canon-круг", "стенд").await;
    let svc = GitCoreSvc { pool };
    let repo = || Some(RepoRef { owner: "canon-круг".into(), slug: "стенд".into() });

    let shown = svc
        .render_canon(Request::new(RenderCanonRequest { repo: repo(), content: Some(content()) }))
        .await
        .expect("канон отдан")
        .into_inner()
        .canon;
    assert!(shown.contains("\"$schema\""), "в каноне ссылка на схему: {shown}");

    let back = svc
        .parse_canon(Request::new(ParseCanonRequest { repo: repo(), canon: shown }))
        .await
        .expect("разбор состоялся")
        .into_inner();
    assert!(back.issues.is_empty(), "свой же канон обязан приниматься: {:?}", back.issues.len());

    let got = back.content.expect("содержимое");
    assert_eq!(got.title, "Развернуть стенд");
    assert_eq!(got.tags, vec!["ops".to_string()]);
    assert_eq!(got.steps.len(), 2, "оба блока пережили круг");
    assert_eq!(got.steps[0].command, "apt install docker");
    assert_eq!(got.steps[0].subtasks, vec!["проверить версию".to_string()]);
    assert_eq!(got.steps[0].refs[0].url, "https://docs.docker.com");
    assert_eq!(got.steps[1].r#type, "text", "презентационный блок остался блоком");
    assert!(got.steps[1].content_json.contains("пояснение"));
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_flaw_in_the_text_arrives_as_complaints_not_silence() {
    let pool = support::pool_with_schema().await;
    seed_list(&pool, "canon-брак", "стенд").await;
    let svc = GitCoreSvc { pool };
    let repo = || Some(RepoRef { owner: "canon-брак".into(), slug: "стенд".into() });

    let shown = svc
        .render_canon(Request::new(RenderCanonRequest { repo: repo(), content: Some(content()) }))
        .await
        .expect("канон отдан")
        .into_inner()
        .canon;
    // Человек стёр заголовок первого шага. Проекция push такой пункт молча
    // выбросила бы — редактор обязан сказать, где именно.
    let broken = shown.replace("Поставить docker", "   ");

    let out = svc
        .parse_canon(Request::new(ParseCanonRequest { repo: repo(), canon: broken }))
        .await
        .expect("разбор состоялся")
        .into_inner();
    assert!(out.content.is_none(), "с придирками содержимое не отдаём");
    let issue = out.issues.first().expect("придирка");
    assert_eq!(issue.code, "step_title_required");
    assert_eq!(issue.path, "/steps/0/title", "указатель на само поле");
}

/// Поля-довески канона обязаны доехать до вызывающего: он понесёт содержимое в
/// AddVersion, где набор шагов перезаписывается ЦЕЛИКОМ. Пустое место любого из
/// них означало бы стёртую картинку и снятую пометку — молча, при сохранении.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn image_and_mark_from_the_text_reach_the_caller() {
    let pool = support::pool_with_schema().await;
    seed_list(&pool, "canon-довески", "стенд").await;
    let svc = GitCoreSvc { pool };

    let text = r#"{"$schema":"https://setfork.com/schema/list.v1.json","title":"Т","desc":"","tags":[],"ordered":true,"version":1,"steps":[{"n":1,"title":"Ш","desc":"","command":"rm -rf /tmp/x","imageKey":"u/1/shot.png","level":"required","needsHuman":true,"needsHumanAsk":"глянь глазами","danger":true,"why":"","section":"","subtasks":[],"refs":[]}]}"#;

    let out = svc
        .parse_canon(Request::new(ParseCanonRequest {
            repo: Some(RepoRef { owner: "canon-довески".into(), slug: "стенд".into() }),
            canon: text.into(),
        }))
        .await
        .expect("разбор состоялся")
        .into_inner();
    assert!(out.issues.is_empty(), "текст валиден");
    let step = &out.content.expect("содержимое").steps[0];
    assert_eq!(step.image_key, "u/1/shot.png");
    assert!(step.needs_human, "пометка «нужен человек» пережила провод");
    assert_eq!(step.needs_human_ask, "глянь глазами");
    assert!(step.danger, "разрушительный пункт пережил провод");
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_ref_without_a_label_is_not_lost_silently() {
    let pool = support::pool_with_schema().await;
    seed_list(&pool, "canon-ссылка", "стенд").await;
    let svc = GitCoreSvc { pool };

    let text = r#"{"$schema":"https://setfork.com/schema/list.v1.json","title":"Т","desc":"","tags":[],"ordered":true,"version":1,"steps":[{"n":1,"title":"Ш","desc":"","command":"","level":"required","why":"","section":"","subtasks":[],"refs":[{"label":"  ","url":"https://example.com"}]}]}"#;

    let out = svc
        .parse_canon(Request::new(ParseCanonRequest {
            repo: Some(RepoRef { owner: "canon-ссылка".into(), slug: "стенд".into() }),
            canon: text.into(),
        }))
        .await
        .expect("разбор состоялся")
        .into_inner();
    let issue = out.issues.first().expect("придирка");
    assert_eq!(issue.code, "ref_label_required");
    assert_eq!(issue.path, "/steps/0/refs/0/label");
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn broken_json_shows_the_line_of_the_break() {
    let pool = support::pool_with_schema().await;
    seed_list(&pool, "canon-синтаксис", "стенд").await;
    let svc = GitCoreSvc { pool };

    let out = svc
        .parse_canon(Request::new(ParseCanonRequest {
            repo: Some(RepoRef { owner: "canon-синтаксис".into(), slug: "стенд".into() }),
            canon: "{\n  \"title\": \n}".into(),
        }))
        .await
        .expect("разбор состоялся")
        .into_inner();
    let issue = out.issues.first().expect("придирка");
    assert_eq!(issue.code, "syntax");
    assert_eq!(issue.line, 3, "строка разрыва, а не начало файла");
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_foreign_list_is_rejected_before_parsing() {
    let pool = support::pool_with_schema().await;
    let svc = GitCoreSvc { pool };
    let err = svc
        .parse_canon(Request::new(ParseCanonRequest {
            repo: Some(RepoRef { owner: "нет-такого".into(), slug: "и-такого".into() }),
            canon: "{}".into(),
        }))
        .await
        .expect_err("списка нет");
    assert_eq!(err.code(), tonic::Code::NotFound);
}
