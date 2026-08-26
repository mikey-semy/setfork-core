//! ПРОБА линзы проверки ядра: squash-слияние #54 через
//! настоящий RPC merge_branch, а не через чистую функцию with_coauthors.
//!
//! В master покрыта только сборка трейлеров. Проверяем:
//!   1) дерево берётся merge-ом, а не деревом ветки — иначе squash тихо
//!      откатывает работу, приехавшую в main после создания ветки;
//!   2) проверка режима идёт ДО fast-forward;
//!   3) что происходит с выбранным режимом, когда слияние пошло через
//!      MergeResolved (конфликт).
//!
//! `TEST_DATABASE_URL=... cargo test --test squash_probe -- --include-ignored --test-threads=1`
#![cfg(feature = "probes")]

mod support;

use setfork_core::pb::git_core_server::GitCore;
use setfork_core::pb::{
    CommitToBranchRequest, CreateBranchRequest, ListContent, MergeBranchRequest, MergeResolvedRequest,
    ParseCanonRequest, RepoRef,
};
use setfork_core::pb_domain::list_write_server::ListWrite;
use setfork_core::pb_domain::{CreateListRequest, LocaleText, NewStep};
use setfork_core::services::git_core::GitCoreSvc;
use setfork_core::services::list::ListWriteSvc;
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

fn repo_ref(owner: &str, slug: &str) -> Option<RepoRef> {
    Some(RepoRef { owner: owner.into(), slug: slug.into() })
}

fn bare_of(dir: &support::OwnGitDataDir, list_id: uuid::Uuid) -> std::path::PathBuf {
    dir.path.join(format!("{list_id}.git"))
}

/// list.json из дерева коммита.
fn list_json_at(bare: &std::path::Path, sha: &str) -> String {
    let repo = git2::Repository::open_bare(bare).expect("open bare");
    let oid = git2::Oid::from_str(sha).expect("oid");
    let commit = repo.find_commit(oid).expect("commit");
    let id = commit.tree().expect("tree").get_name("list.json").expect("list.json есть").id();
    String::from_utf8_lossy(repo.find_blob(id).expect("blob").content()).to_string()
}

fn main_tip(bare: &std::path::Path) -> String {
    git2::Repository::open_bare(bare)
        .expect("open")
        .refname_to_id("refs/heads/main")
        .expect("main")
        .to_string()
}

async fn seed(pool: &sqlx::PgPool, handle: &str, slug: &str, steps: Vec<NewStep>) -> uuid::Uuid {
    let owner = support::seed_user(pool, handle).await;
    let write = ListWriteSvc { pool: pool.clone() };
    let created = write
        .create(Request::new(CreateListRequest {
            owner_id: owner.to_string(),
            slug: slug.into(),
            title: lt("Probe"),
            desc: lt("d"),
            tags: vec![],
            ordered: true,
            visibility: String::new(),
            status: String::new(),
            origin: String::new(),
            forked_from_id: String::new(),
            moderation: String::new(),
            note: "v1".into(),
            steps,
        }))
        .await
        .expect("create")
        .into_inner();
    uuid::Uuid::parse_str(&created.id).expect("uuid")
}

fn proj_step(title: &str) -> setfork_core::git::project::ProjStep {
    setfork_core::git::project::ProjStep {
        block_type: "step".into(),
        content: serde_json::Value::Null,
        block_id: None,
        title: title.into(),
        desc: "d".into(),
        command: String::new(),
        level: "required".into(),
        why: String::new(),
        section: String::new(),
        subtasks: vec![],
        refs: vec![],
        image_key: None,
        needs_human: None,
        needs_human_ask: None,
        danger: None,
    }
}

/// Главное решение автора #54: дерево берётся MERGE-ом, а не деревом ветки.
/// Иначе squash тихо откатывает работу, приехавшую в main после создания ветки.
/// Канон ТЕКСТОМ → содержимое для записи. Пробы правят канон подстрокой (так же
/// делает фронт в редакторе кода), а провод с #60 принимает СТРУКТУРУ, а не байты.
/// Разбираем тем же RPC, которым пользуется редактор, — тогда проба продолжает
/// проверять своё, а не форму запроса.
async fn blob_text(git: &GitCoreSvc, rr: Option<RepoRef>, canon: String) -> Option<ListContent> {
    git.parse_canon(Request::new(ParseCanonRequest { repo: rr, canon }))
        .await
        .expect("канон разбирается")
        .into_inner()
        .content
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn squash_does_not_revert_work_that_landed_in_main() {
    let dir = support::own_git_data_dir("squash-probe").await;
    let pool = support::pool_with_schema().await;
    let list_id = seed(
        &pool,
        "alice",
        "squash-probe",
        vec![step("Первый"), step("Второй"), step("Третий"), step("Четвёртый")],
    )
    .await;
    let git = GitCoreSvc { pool: pool.clone() };
    let rr = repo_ref("alice", "squash-probe");

    git.create_branch(Request::new(CreateBranchRequest {
        repo: rr.clone(),
        name: "pr-1".into(),
        from: String::new(),
    }))
    .await
    .expect("create_branch");

    // Ветка правит НАЧАЛО файла (заголовок первого шага) — как это делает фронт:
    // берёт канон и меняет кусок.
    let bare = bare_of(&dir, list_id);
    let base_json = list_json_at(&bare, &main_tip(&bare));
    // Правим шаг в СЕРЕДИНЕ: поле "version" в начале файла меняется при каждой
    // веб-версии, и правка рядом с ним конфликтует из-за контекста git.
    let branch_json = base_json.replacen("Третий", "Третий (правка ветки)", 1);
    assert_ne!(branch_json, base_json, "правка ветки должна что-то менять");

    git.commit_to_branch(Request::new(CommitToBranchRequest {
        repo: rr.clone(),
        branch: "pr-1".into(),
        content: blob_text(&git, rr.clone(), branch_json).await,
        message: "правка ветки".into(),
        expected_tip: String::new(),
        author_name: "Аня".into(),
        author_email: "anya@example.com".into(),
    }))
    .await
    .expect("commit_to_branch");

    // main уезжает вперёд ПОСЛЕ создания ветки — веб-версия дописывает шаг в КОНЕЦ.
    setfork_core::db::add_version(
        &pool,
        list_id,
        "web",
        &[
            proj_step("Первый"),
            proj_step("Второй"),
            proj_step("Третий"),
            proj_step("Четвёртый"),
            proj_step("Шаг добавленный в main"),
        ],
    )
    .await
    .expect("add_version v2");
    // ensure_repo дописывает новую версию в main при следующем обращении.
    git.list_branches(Request::new(setfork_core::pb::RepoRef {
        owner: "alice".into(),
        slug: "squash-probe".into(),
    }))
    .await
    .expect("list_branches (триггерит append main)");

    let merged = git
        .merge_branch(Request::new(MergeBranchRequest {
            repo: rr.clone(),
            name: "pr-1".into(),
            mode: "squash".into(),
            message: String::new(),
        }))
        .await
        .expect("merge_branch squash")
        .into_inner();

    let repo = git2::Repository::open_bare(&bare).expect("open");
    let head = repo.find_commit(git2::Oid::from_str(&merged.tip_sha).expect("oid")).expect("commit");

    assert_eq!(head.parent_count(), 1, "squash — ОДИН родитель, история ветки в main не уезжает");

    let msg = head.message().unwrap_or_default().to_string();
    assert!(
        msg.contains("Co-authored-by: Аня <anya@example.com>"),
        "при squash трейлер — единственная запись об авторстве: {msg:?}"
    );

    let json = list_json_at(&bare, &merged.tip_sha);
    assert!(
        json.contains("Шаг добавленный в main"),
        "взято дерево ветки вместо merge — работа из main потеряна"
    );
    assert!(json.contains("Третий (правка ветки)"), "правка ветки должна доехать");
}

/// Перематываемая ветка при mode=squash обязана сплющиваться, а не уезжать
/// в main историей — иначе выбор режима работает через раз.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn squash_flattens_a_fast_forwardable_branch_too() {
    let dir = support::own_git_data_dir("squash-probe").await;
    let pool = support::pool_with_schema().await;
    let list_id = seed(&pool, "bob", "squash-ff", vec![step("База")]).await;
    let git = GitCoreSvc { pool: pool.clone() };
    let rr = repo_ref("bob", "squash-ff");

    git.create_branch(Request::new(CreateBranchRequest {
        repo: rr.clone(),
        name: "pr-ff".into(),
        from: String::new(),
    }))
    .await
    .expect("create_branch");

    let bare = bare_of(&dir, list_id);
    let base = list_json_at(&bare, &main_tip(&bare));
    for (i, who) in [("первый", "Аня"), ("второй", "Боря")] {
        let j = base.replacen("База", &format!("База {i}"), 1);
        git.commit_to_branch(Request::new(CommitToBranchRequest {
            repo: rr.clone(),
            branch: "pr-ff".into(),
            content: blob_text(&git, rr.clone(), j).await,
            message: i.into(),
            author_name: who.into(),
            author_email: format!("{}@example.com", who.to_lowercase()),
            expected_tip: String::new(),
        }))
        .await
        .expect("commit_to_branch");
    }

    let merged = git
        .merge_branch(Request::new(MergeBranchRequest {
            repo: rr.clone(),
            name: "pr-ff".into(),
            mode: "squash".into(),
            message: "Сплющенное".into(),
        }))
        .await
        .expect("merge squash ff")
        .into_inner();

    assert!(!merged.fast_forward, "при squash перемотки быть не должно");

    let repo = git2::Repository::open_bare(&bare).expect("open");
    let head = repo.find_commit(git2::Oid::from_str(&merged.tip_sha).expect("oid")).expect("commit");
    assert_eq!(head.parent_count(), 1, "один родитель");
    let parent = head.parent(0).expect("parent");
    assert!(
        !parent.message().unwrap_or_default().contains("второй"),
        "родитель squash-коммита — старый main, а не tip ветки"
    );
    let msg = head.message().unwrap_or_default().to_string();
    assert!(msg.starts_with("Сплющенное"), "заголовок из запроса: {msg:?}");
    assert!(msg.contains("Аня") && msg.contains("Боря"), "оба автора в трейлерах: {msg:?}");
}

/// ВЫБРАННЫЙ РЕЖИМ ДОЖИВАЕТ ДО РАЗРЕШЕНИЯ КОНФЛИКТА.
///
/// list.json — единственный файл списка, поэтому правка одного и того же места в
/// ветке и в main даёт конфликт merge. Штатный путь дальше — `MergeResolved`.
///
/// ⚠️ Эта проба РОДИЛАСЬ КАК ДЕМОНСТРАЦИЯ ДЕФЕКТА: выбор «squash» терялся, и
/// разрешение конфликта всегда давало merge-коммит с историей ветки в main. Дефект
/// починен (в запросе появилось поле `mode`), поэтому утверждения перевёрнуты:
/// теперь она сторожит регрессию, а не фиксирует беду. Имя изменено вслед за смыслом.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn on_conflict_the_chosen_squash_mode_survives_until_the_merge() {
    let dir = support::own_git_data_dir("squash-probe").await;
    let pool = support::pool_with_schema().await;
    let list_id = seed(&pool, "carol", "squash-conflict", vec![step("Общий")]).await;
    let git = GitCoreSvc { pool: pool.clone() };
    let rr = repo_ref("carol", "squash-conflict");

    git.create_branch(Request::new(CreateBranchRequest {
        repo: rr.clone(),
        name: "pr-c".into(),
        from: String::new(),
    }))
    .await
    .expect("create_branch");

    let bare = bare_of(&dir, list_id);
    let base = list_json_at(&bare, &main_tip(&bare));

    // Ветка и main правят ОДНО И ТО ЖЕ место.
    git.commit_to_branch(Request::new(CommitToBranchRequest {
        repo: rr.clone(),
        branch: "pr-c".into(),
        content: blob_text(&git, rr.clone(), base.replacen("Общий", "Версия ветки", 1)).await,
        message: "правка ветки".into(),
        expected_tip: String::new(),
        author_name: "Аня".into(),
        author_email: "anya@example.com".into(),
    }))
    .await
    .expect("commit_to_branch");

    setfork_core::db::add_version(&pool, list_id, "web", &[proj_step("Версия main")])
        .await
        .expect("add_version");
    git.list_branches(Request::new(setfork_core::pb::RepoRef {
        owner: "carol".into(),
        slug: "squash-conflict".into(),
    }))
    .await
    .expect("list_branches");

    // Пользователь выбрал squash.
    let err = git
        .merge_branch(Request::new(MergeBranchRequest {
            repo: rr.clone(),
            name: "pr-c".into(),
            mode: "squash".into(),
            message: "Сплющенное".into(),
        }))
        .await
        .expect_err("правки одного места обязаны дать конфликт");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    // Сверяем ПРИЧИНУ из трейлера, а не текст. Раньше здесь стояло `== "conflict"`, и
    // проба покраснела ровно тогда, когда текст стал человеческим («merge does not
    // apply cleanly») — то есть кодировала СПОСОБ, а не смысл. Контракт с И1 держится
    // на машинной причине; текст можно менять свободно.
    assert_eq!(
        err.metadata().get(setfork_core::reason::REASON_KEY).and_then(|v| v.to_str().ok()),
        Some("CONFLICT"),
        "причина отказа — конфликт слияния"
    );

    // Штатный путь: пользователь разрешил конфликт и подтвердил слияние.
    let resolved = git
        .merge_resolved(Request::new(MergeResolvedRequest {
            repo: rr.clone(),
            branch: "pr-c".into(),
            content: blob_text(&git, rr.clone(), base.replacen("Общий", "Разрешённая версия", 1)).await,
            // Режим приезжает из ЗАПРОСА (#54): проба про то и есть — выбор «squash»
            // обязан дожить до разрешения конфликта, а не потеряться по дороге.
            mode: "squash".into(),
            message: String::new(),
        }))
        .await
        .expect("merge_resolved")
        .into_inner();

    let repo = git2::Repository::open_bare(&bare).expect("open");
    let head = repo.find_commit(git2::Oid::from_str(&resolved.tip_sha).expect("oid")).expect("commit");

    // Режим соблюдён: ОДИН родитель, история ветки в main не уехала.
    assert_eq!(head.parent_count(), 1, "выбранный squash обязан дожить до MergeResolved");
    let msg = head.message().unwrap_or_default().to_string();
    assert!(msg.contains("Co-authored-by"), "вклад авторов ветки переносится трейлерами: {msg:?}");
}

/// Диагностика: насколько сильно веб-версия переписывает list.json.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn list_json_divergence_dump() {
    let dir = support::own_git_data_dir("squash-probe").await;
    let pool = support::pool_with_schema().await;
    let list_id = seed(&pool, "dave", "dump", vec![step("Первый"), step("Второй")]).await;
    let git = GitCoreSvc { pool: pool.clone() };
    git.list_branches(Request::new(setfork_core::pb::RepoRef { owner: "dave".into(), slug: "dump".into() }))
        .await
        .expect("materialize");
    let bare = bare_of(&dir, list_id);
    let before = list_json_at(&bare, &main_tip(&bare));
    setfork_core::db::add_version(
        &pool,
        list_id,
        "web",
        &[proj_step("Первый"), proj_step("Второй"), proj_step("Третий")],
    )
    .await
    .expect("v2");
    git.list_branches(Request::new(setfork_core::pb::RepoRef { owner: "dave".into(), slug: "dump".into() }))
        .await
        .expect("lb");
    let after = list_json_at(&bare, &main_tip(&bare));
    std::fs::write(std::env::temp_dir().join("probe-before.json"), &before).ok();
    std::fs::write(std::env::temp_dir().join("probe-after.json"), &after).ok();
    println!("BEFORE_LINES={} AFTER_LINES={}", before.lines().count(), after.lines().count());
}
