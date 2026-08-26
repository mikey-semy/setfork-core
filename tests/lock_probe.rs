//! ПРОБА линзы проверки ядра: распределённый лок репозитория под параллельной нагрузкой.
//!
//! `repo_guard` берёт pg_advisory_xact_lock перед каждой операцией, двигающей main.
//! Тестами это не проверено ничем: все существующие тесты последовательные.
//!
//! Цена ошибки прямая: без сериализации второе слияние читает устаревший main-tip и
//! затирает первое — правка пользователя исчезает, притом что интерфейс отчитался
//! об успехе.
#![cfg(feature = "probes")]

mod support;

use setfork_core::pb::git_core_server::GitCore;
use setfork_core::pb::{
    CommitToBranchRequest, CreateBranchRequest, ListContent, MergeBranchRequest, ParseCanonRequest, RepoRef,
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

fn list_json_at(bare: &std::path::Path, sha: &str) -> String {
    let repo = git2::Repository::open_bare(bare).expect("open");
    let c = repo.find_commit(git2::Oid::from_str(sha).expect("oid")).expect("commit");
    let id = c.tree().expect("tree").get_name("list.json").expect("list.json").id();
    String::from_utf8_lossy(repo.find_blob(id).expect("blob").content()).to_string()
}

/// Два слияния РАЗНЫХ веток в один список одновременно. Обе правки обязаны
/// оказаться в main: лок должен выстроить их в очередь, а не дать второму
/// затереть первого.
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
async fn concurrent_merges_do_not_clobber_each_other() {
    let dir = support::own_git_data_dir("lock-probe").await;
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "alice").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let created = write
        .create(Request::new(CreateListRequest {
            owner_id: owner.to_string(),
            slug: "lock-probe".into(),
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
            steps: vec![step("Первый"), step("Второй"), step("Третий"), step("Четвёртый")],
        }))
        .await
        .expect("create")
        .into_inner();
    let list_id = uuid::Uuid::parse_str(&created.id).expect("uuid");
    let git = GitCoreSvc { pool: pool.clone() };
    let rr = Some(RepoRef { owner: "alice".into(), slug: "lock-probe".into() });

    // Две ветки от общей базы, правки в РАЗНЫХ местах файла (иначе конфликт —
    // это уже другая история, см. заход 20).
    for b in ["pr-a", "pr-b"] {
        git.create_branch(Request::new(CreateBranchRequest {
            repo: rr.clone(),
            name: b.into(),
            from: String::new(),
        }))
        .await
        .expect("create_branch");
    }
    let bare = dir.path.join(format!("{list_id}.git"));
    let base = list_json_at(&bare, &{
        git2::Repository::open_bare(&bare)
            .expect("o")
            .refname_to_id("refs/heads/main")
            .expect("m")
            .to_string()
    });

    git.commit_to_branch(Request::new(CommitToBranchRequest {
        repo: rr.clone(),
        branch: "pr-a".into(),
        content: blob_text(&git, rr.clone(), base.replacen("Второй", "Второй (ветка A)", 1)).await,
        message: "правка A".into(),
        expected_tip: String::new(),
        author_name: "Аня".into(),
        author_email: "anya@example.com".into(),
    }))
    .await
    .expect("commit A");
    git.commit_to_branch(Request::new(CommitToBranchRequest {
        repo: rr.clone(),
        branch: "pr-b".into(),
        content: blob_text(&git, rr.clone(), base.replacen("Четвёртый", "Четвёртый (ветка B)", 1)).await,
        message: "правка B".into(),
        expected_tip: String::new(),
        author_name: "Боря".into(),
        author_email: "borya@example.com".into(),
    }))
    .await
    .expect("commit B");

    // ОДНОВРЕМЕННО.
    let (ga, gb) = (GitCoreSvc { pool: pool.clone() }, GitCoreSvc { pool: pool.clone() });
    let (ra, rb) = (rr.clone(), rr.clone());
    let a = tokio::spawn(async move {
        ga.merge_branch(Request::new(MergeBranchRequest {
            repo: ra,
            name: "pr-a".into(),
            mode: String::new(),
            message: String::new(),
        }))
        .await
        .map(|r| r.into_inner().tip_sha)
        .map_err(|e| (e.code(), e.message().to_string()))
    });
    let b = tokio::spawn(async move {
        gb.merge_branch(Request::new(MergeBranchRequest {
            repo: rb,
            name: "pr-b".into(),
            mode: String::new(),
            message: String::new(),
        }))
        .await
        .map(|r| r.into_inner().tip_sha)
        .map_err(|e| (e.code(), e.message().to_string()))
    });
    let (ra, rb) = (a.await.expect("join a"), b.await.expect("join b"));
    println!("A: {ra:?}");
    println!("B: {rb:?}");

    let tip = git2::Repository::open_bare(&bare)
        .expect("open")
        .refname_to_id("refs/heads/main")
        .expect("main")
        .to_string();
    let json = list_json_at(&bare, &tip);
    println!("MAIN содержит A: {}, B: {}", json.contains("(ветка A)"), json.contains("(ветка B)"));

    assert!(ra.is_ok() && rb.is_ok(), "оба слияния обязаны пройти: A={ra:?} B={rb:?}");
    assert!(json.contains("(ветка A)"), "правка A потеряна — второе слияние затёрло первое");
    assert!(json.contains("(ветка B)"), "правка B потеряна");

    // Версии в БД: каждое слияние обязано спроецироваться, номера без дублей.
    let versions: Vec<i32> =
        sqlx::query_scalar("select version from template_versions where template_id = $1 order by version")
            .bind(list_id)
            .fetch_all(&pool)
            .await
            .expect("versions");
    println!("ВЕРСИИ: {versions:?}");
    let mut u = versions.clone();
    u.dedup();
    assert_eq!(u.len(), versions.len(), "номера версий не должны дублироваться: {versions:?}");
}
