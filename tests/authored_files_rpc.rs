//! `GetAuthoredFiles` по проводу сервиса: смысл `found` и отказ без `repo`.
//!
//! Байты и исполняемость файлов держат тесты модуля (`authored_files.rs`); здесь —
//! то, что видит приложение. `found = false` для несуществующей версии обязан быть
//! именно «нет такой версии», а не пустым набором: приложение по нему решает, что
//! собирать скилл из блоков, и перепутать эти два ответа значило бы отдать архив
//! без файлов автора там, где они есть, — или наоборот.
mod support;

use setfork_core::pb::git_core_server::GitCore;
use setfork_core::pb::{AuthoredFilesRequest, RepoRef};
use setfork_core::pb_domain::list_write_server::ListWrite;
use setfork_core::pb_domain::{CreateListRequest, LocaleText, NewStep};
use setfork_core::services::git_core::GitCoreSvc;
use setfork_core::services::list::ListWriteSvc;
use tonic::Request;

fn lt(s: &str) -> Option<LocaleText> {
    Some(LocaleText { v: [("en".to_string(), s.to_string())].into_iter().collect() })
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn found_means_the_version_exists_and_files_may_be_empty() {
    let _dir = support::own_git_data_dir("authored-rpc").await;
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "authored").await;
    ListWriteSvc { pool: pool.clone() }
        .create(Request::new(CreateListRequest {
            authored: None,
            owner_id: owner.to_string(),
            slug: "skill".into(),
            title: lt("Skill"),
            desc: lt("d"),
            tags: vec![],
            ordered: true,
            visibility: String::new(),
            status: String::new(),
            origin: String::new(),
            forked_from_id: String::new(),
            moderation: String::new(),
            note: "v1".into(),
            steps: vec![NewStep { title: lt("Шаг"), ..Default::default() }],
        }))
        .await
        .expect("create");
    let git = GitCoreSvc { pool: pool.clone() };
    let ask = |version: i32| AuthoredFilesRequest {
        repo: Some(RepoRef { owner: "authored".into(), slug: "skill".into() }),
        version,
    };

    let v1 = git.get_authored_files(Request::new(ask(1))).await.expect("v1").into_inner();
    assert!(v1.found, "существующая версия выдана за несуществующую");
    assert!(v1.files.is_empty(), "у версии без авторских файлов что-то нашлось");

    let main = git.get_authored_files(Request::new(ask(0))).await.expect("main").into_inner();
    assert!(main.found, "вершина main (версия 0) не нашлась");

    let missing =
        git.get_authored_files(Request::new(ask(99))).await.expect("ответ, а не ошибка").into_inner();
    assert!(!missing.found, "несуществующая версия выдана за существующую без файлов");

    let err = git
        .get_authored_files(Request::new(AuthoredFilesRequest { repo: None, version: 1 }))
        .await
        .expect_err("запрос без repo прошёл");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}
