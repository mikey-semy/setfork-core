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

/// Разбор имени тега не должен принимать `vv2` и `v+2` за версию 2.
///
/// `is_version_tag` (`git_core/names.rs`) резервирует только одиночное `v` + цифры, значит
/// `vv2` — законное имя релиза, оно ложится в тот же репозиторий и попадает под глоб
/// `refs/tags/v*`. Прежний разбор снимал ВСЕ ведущие `v` и получал версию 2 — столкновение
/// с настоящим тегом, победитель по порядку обхода рефов. Показанная «байтовая
/// идентичность» указывала бы на чужой коммит и менялась от запроса к запросу.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn release_tags_shaped_like_versions_do_not_hijack_version_two() {
    let dir = support::own_git_data_dir("vv2").await;
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "bob").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let read = ListReadSvc { pool: pool.clone() };

    let created = write
        .create(Request::new(CreateListRequest {
            owner_id: owner.to_string(),
            slug: "vv-collision".into(),
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

    // Релизный тег `vv2` на ПЕРВОМ коммите — как если бы человек назвал релиз так.
    let list_id = uuid::Uuid::parse_str(&created.id).expect("uuid");
    let bare = dir.path.join(format!("{list_id}.git"));
    {
        let repo = git2::Repository::open_bare(&bare).expect("open bare");
        let v1 = repo.refname_to_id("refs/tags/v1").expect("тег v1 есть");
        let obj = repo.find_object(v1, None).expect("object");
        repo.tag_lightweight("vv2", &obj, false).expect("релизный тег vv2");
        // Второе имя той же семьи: `parse::<i32>()` принимает ведущий плюс, а
        // `is_version_tag("v+2")` ложно (плюс не цифра) — значит `v+2` законный релиз.
        // Голый parse разобрал бы его как версию 2 и столкнул с настоящим тегом.
        repo.tag_lightweight("v+2", &obj, false).expect("релизный тег v+2");
    }

    let after = read
        .list_versions(Request::new(ListId { id: created.id.clone() }))
        .await
        .expect("list_versions")
        .into_inner();
    let v2 = after.versions.iter().find(|v| v.version == 2).expect("версия 2 есть");
    assert_eq!(
        v2.commit_sha, written.commit_sha,
        "релизный тег (vv2 или v+2) подменил SHA версии 2 — разбор шире правила резервирования"
    );

    let one = read
        .get_version(Request::new(GetVersionRequest { list_id: created.id.clone(), version: 2 }))
        .await
        .expect("get_version")
        .into_inner();
    assert_eq!(
        one.version.expect("версия есть").commit_sha,
        written.commit_sha,
        "одиночное чтение тоже обязано брать ровно refs/tags/v2"
    );
}

/// Без тома чтение версий отдаёт пустой SHA, а НЕ падает.
///
/// Команды golden-CLI исполняются до `require_git_data_dir()` и по замыслу работают без
/// `GIT_DATA_DIR`. `repo_path` внутри делает `expect` — значит зов пути отсюда ронял бы
/// `domain-read` паникой вместо вывода JSON, причём ДО `spawn_blocking`, то есть никакой
/// `unwrap_or_default` этого не поймал бы.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn reading_versions_without_a_volume_yields_empty_not_panic() {
    // Замок тот же, что у own_git_data_dir: переменная принадлежит процессу, не тесту.
    let _lock = support::GIT_DATA_DIR_LOCK.lock().await;
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "carol").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let read = ListReadSvc { pool: pool.clone() };
    let created = write
        .create(Request::new(CreateListRequest {
            owner_id: owner.to_string(),
            slug: "no-volume".into(),
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

    let saved = std::env::var("GIT_DATA_DIR").ok();
    // SAFETY: замок держится до конца теста — другой поток env сейчас не пишет.
    unsafe { std::env::remove_var("GIT_DATA_DIR") };
    let listed = read.list_versions(Request::new(ListId { id: created.id.clone() })).await;
    let single =
        read.get_version(Request::new(GetVersionRequest { list_id: created.id.clone(), version: 1 })).await;
    if let Some(v) = saved {
        unsafe { std::env::set_var("GIT_DATA_DIR", v) };
    }

    let listed = listed.expect("чтение версий без тома обязано ОТВЕТИТЬ, а не упасть").into_inner();
    assert!(listed.versions[0].commit_sha.is_empty(), "без тома SHA неизвестен");
    let single = single.expect("одиночное чтение без тома тоже обязано ответить").into_inner();
    assert!(single.version.expect("версия есть").commit_sha.is_empty(), "без тома SHA неизвестен");
}
