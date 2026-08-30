//! SHA версии читается из ТЕГОВ канона, а не из колонки в Postgres.
//!
//! До 30.08 поле `Version.commit_sha` было в контракте и всегда пустым на чтении: настоящий
//! SHA видел ровно один вызывающий — тот, кто только что записал версию. То есть контракт
//! обещал байтовую идентичность, а отдавал пустоту (T1.3 плана 28.08).
//!
//! Тест держит три вещи сразу:
//!   1) у списка БЕЗ репозитория SHA пустой — это видимое лицо находки V1, не поломка;
//!   2) после записи чтение отдаёт ТОТ ЖЕ SHA, что вернула запись, — иначе витрина
//!      показывала бы «идентичность», не совпадающую с настоящей;
//!   3) старые версии тоже получают SHA — их теги родились при материализации.
mod support;

use setfork_core::pb_domain::list_read_server::ListRead;
use setfork_core::pb_domain::list_write_server::ListWrite;
use setfork_core::pb_domain::{
    AddVersionRequest, CreateListRequest, GetVersionRequest, ListId, LocaleText, NewStep,
};
use setfork_core::services::list::{ListReadSvc, ListWriteSvc};
use tonic::Request;

fn lt(s: &str) -> Option<LocaleText> {
    Some(LocaleText { v: [("en".to_string(), s.to_string())].into_iter().collect() })
}

fn step(title: &str) -> NewStep {
    NewStep {
        block_id: String::new(),
        title: lt(title),
        desc: lt("d"),
        command: String::new(),
        level: "required".into(),
        why: None,
        section: None,
        subtasks: vec![],
        refs: vec![],
        image_ref: String::new(),
        r#type: String::new(),
        content_json: String::new(),
        needs_human: false,
        needs_human_ask: None,
        danger: false,
    }
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn version_sha_comes_from_the_tag_and_matches_the_write() {
    let _dir = support::own_git_data_dir("version-sha").await;
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "alice").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let read = ListReadSvc { pool: pool.clone() };

    let created = write
        .create(Request::new(CreateListRequest {
            owner_id: owner.to_string(),
            slug: "sha-on-display".into(),
            title: lt("T"),
            desc: lt("d"),
            tags: vec![],
            ordered: true,
            visibility: String::new(),
            status: String::new(),
            origin: String::new(),
            forked_from_id: String::new(),
            moderation: String::new(),
            note: "v1".into(),
            steps: vec![step("Install")],
        }))
        .await
        .expect("create")
        .into_inner();

    // 1. Репозитория ещё нет — SHA неизвестен. Не ошибка, а честная пустота.
    let before = read
        .list_versions(Request::new(ListId { id: created.id.clone() }))
        .await
        .expect("list_versions до касания")
        .into_inner();
    assert_eq!(before.versions.len(), 1, "версия одна");
    assert!(
        before.versions[0].commit_sha.is_empty(),
        "у списка без репозитория SHA обязан быть ПУСТЫМ — материализовать на чтении нельзя"
    );

    // 2. Запись идёт git-first: она и создаёт репозиторий, и возвращает SHA.
    let written = write
        .add_version(Request::new(AddVersionRequest {
            list_id: created.id.clone(),
            note: "v2".into(),
            steps: vec![step("Configure")],
            author_id: owner.to_string(),
            expected_version: None,
            meta: None,
        }))
        .await
        .expect("add_version")
        .into_inner();
    assert!(!written.commit_sha.is_empty(), "запись обязана вернуть настоящий SHA");

    // 3. Чтение отдаёт ТОТ ЖЕ SHA. Это главное утверждение: витрина и запись обязаны
    //    называть одну и ту же идентичность, иначе «байты» — снова обещание.
    let after = read
        .list_versions(Request::new(ListId { id: created.id.clone() }))
        .await
        .expect("list_versions после записи")
        .into_inner();
    let v2 = after.versions.iter().find(|v| v.version == 2).expect("версия 2 есть");
    assert_eq!(v2.commit_sha, written.commit_sha, "чтение и запись назвали РАЗНЫЕ SHA одной версии");

    // 4. Версия 1 тоже получила SHA — её тег родился при материализации.
    let v1 = after.versions.iter().find(|v| v.version == 1).expect("версия 1 есть");
    assert!(!v1.commit_sha.is_empty(), "у прежних версий SHA берётся из их тегов");
    assert_ne!(v1.commit_sha, v2.commit_sha, "у разных версий разные коммиты");

    // 5. Одиночное чтение согласовано со списком.
    let one = read
        .get_version(Request::new(GetVersionRequest { list_id: created.id.clone(), version: 2 }))
        .await
        .expect("get_version")
        .into_inner();
    assert_eq!(
        one.version.expect("версия есть").commit_sha,
        written.commit_sha,
        "get_version и list_versions обязаны называть один SHA"
    );
}
