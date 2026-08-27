//! Рождение списка и появление канона — ДВА РАЗНЫХ СОБЫТИЯ.
//!
//! `ListWrite.Create` пишет только Postgres: ни репозитория, ни коммита, ни зеркала.
//! Канон появляется позже, при первом касании по git (`ensure_repo_by_id`), и до этого
//! момента у списка нет копии в git вовсе — то есть обещание ADR-0014 «разрыв суточного
//! дампа закрывается git'ом» на такой список не распространяется.
//!
//! Замер на проде 27.08.2026: 58 списков из 659 в этом состоянии, все с `current_version = 1`;
//! у 14 из 20 опубликованных канон есть только потому, что разово прогоняли `sync-repos`.
//! Разбор: `setfork-hq/reviews/vertical/2026-08-27-assemble-list-ledger.md`.
//!
//! Тест закрепляет ОБА факта: сегодняшний пробел и то, что материализация восстанавливает
//! канон ровно тот же, что собрал бы сериализатор из БД. Если поведение решат менять
//! (вариант 4 реестра — материализовать сразу после Create), первый assert покраснеет, и
//! это правильно: менять его придётся осознанно, а не мимоходом.
mod support;

use setfork_core::db;
use setfork_core::git::{repo, serialize};
use setfork_core::pb_domain::list_write_server::ListWrite;
use setfork_core::pb_domain::{CreateListRequest, LocaleText, NewStep};
use setfork_core::services::list::ListWriteSvc;
use tonic::Request;
use uuid::Uuid;

fn lt(pairs: &[(&str, &str)]) -> Option<LocaleText> {
    Some(LocaleText { v: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() })
}

/// Шаг с пометками, которые уже терялись на переходах: `block_id`, `needs_human`, `danger`.
/// Берём их намеренно — вертикаль спрашивает не «есть ли канон», а «что теряется по дороге».
fn marked_step(title: &str, bid: &str) -> NewStep {
    NewStep {
        block_id: bid.into(),
        title: lt(&[("en", title)]),
        desc: lt(&[("en", "desc")]),
        command: "echo hi".into(),
        level: "required".into(),
        why: lt(&[("en", "because")]),
        section: lt(&[("en", "Setup")]),
        subtasks: vec![],
        refs: vec![],
        image_ref: String::new(),
        r#type: String::new(),
        content_json: String::new(),
        needs_human: true,
        needs_human_ask: lt(&[("en", "ask the human")]),
        danger: true,
    }
}

/// Содержимое `list.json` из `refs/heads/main` голого репозитория.
fn canon_in_git(bare: &std::path::Path) -> String {
    let repo = git2::Repository::open_bare(bare).expect("репозиторий открывается");
    let tree = repo.find_reference("refs/heads/main").expect("main есть").peel_to_tree().expect("дерево");
    let entry = tree.get_name("list.json").expect("list.json в дереве");
    let blob = repo.find_blob(entry.id()).expect("blob");
    String::from_utf8_lossy(blob.content()).to_string()
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn create_leaves_no_canon_until_first_touch() {
    let dir = support::own_git_data_dir("create-canon-gap").await;
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "alice").await;
    let write = ListWriteSvc { pool: pool.clone() };

    let bid = Uuid::new_v4().to_string();
    let created = write
        .create(Request::new(CreateListRequest {
            owner_id: owner.to_string(),
            slug: "born-without-canon".into(),
            title: lt(&[("en", "Born without canon")]),
            desc: lt(&[("en", "d")]),
            tags: vec![],
            ordered: true,
            visibility: String::new(),
            status: String::new(),
            origin: String::new(),
            forked_from_id: String::new(),
            note: "initial".into(),
            steps: vec![marked_step("Install", &bid)],
            moderation: String::new(),
        }))
        .await
        .expect("создание проходит")
        .into_inner();

    let id = Uuid::parse_str(&created.id).expect("id — uuid");
    let bare = repo::repo_path(id);

    // 1. Список есть, канона нет. Это и есть пробел, ради которого тест написан.
    assert_eq!(created.current_version, 1, "рождение даёт версию 1");
    assert!(
        !bare.exists(),
        "Create завёл репозиторий {bare:?} — поведение изменилось. Если это сделано намеренно \
         (вариант 4 реестра вертикали), обнови тест И обещания: комментарий repo.rs, \
         runbooks/db-backup-restore.md, реестр вертикали."
    );
    assert!(dir.path.exists(), "каталог тома существует — значит дело не в отсутствии тома");

    // 2. Первое касание по git материализует репозиторий из истории БД.
    let materialized = repo::ensure_repo_by_id(&pool, id).await.expect("материализация").expect("список есть");
    assert_eq!(materialized, bare, "материализовано по каноническому пути");
    assert!(bare.exists(), "после первого касания репозиторий появился");

    // 3. И канон в git равен тому, что сериализатор собрал бы из БД — байт в байт.
    //    Проверяется не «файл есть», а совпадение содержимого: иначе материализация могла бы
    //    завести пустой репозиторий и тест бы этого не заметил.
    let versions = db::load_bundle_data(&pool, id).await.expect("история из БД");
    let v1 = versions.into_iter().find(|v| v.version == 1).expect("версия 1 в БД");
    let from_db = serialize::list_json(&v1);
    assert_eq!(canon_in_git(&bare), from_db, "канон в git расходится с тем, что даёт БД");

    // 4. Пометки, терявшиеся на прежних переходах, дошли до канона.
    let canon = canon_in_git(&bare);
    assert!(canon.contains(&bid), "block_id не доехал до канона");
    assert!(canon.contains("needs_human") || canon.contains("needsHuman"), "пометка «нужен человек» не доехала");
    assert!(canon.contains("danger"), "пометка «разрушительный» не доехала");
}
