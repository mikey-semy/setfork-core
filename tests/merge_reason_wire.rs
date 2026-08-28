//! Причина отказа на СЛИЯНИИ должна доезжать до клиента ПО ПРОВОДУ.
//!
//! Зачем отдельный тест, если проверка уже есть. Она есть — но в `squash_probe`, а пробы
//! стоят за `--features probes`, и CI их только СОБИРАЕТ (`cargo check --features probes
//! --tests`), не запуская. То есть контракт «фронт различает отказы слияния по причине»
//! держался тестом, который не исполняется. Замер 28.08.2026: девять файлов проб в этом
//! состоянии.
//!
//! Второе отличие: та проверка зовёт сервис НАПРЯМУЮ. На первой вертикали выяснилось, что
//! это разные вещи — трейлер, видимый в процессе, не обязан пережить транспорт. Здесь
//! поднимается настоящий сервер и ходит настоящий клиент.
//!
//! Разбор: setfork-hq/reviews/vertical/2026-08-28-fork-suggest-merge-ledger.md
mod support;

use setfork_core::pb::git_core_client::GitCoreClient;
use setfork_core::pb::git_core_server::GitCore;
use setfork_core::pb::git_core_server::GitCoreServer;
use setfork_core::pb::{
    CommitToBranchRequest, CreateBranchRequest, MergeBranchRequest, ParseCanonRequest, RepoRef,
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

fn list_json_at(bare: &std::path::Path, sha: &str) -> String {
    let repo = git2::Repository::open_bare(bare).expect("open bare");
    let commit = repo.find_commit(git2::Oid::from_str(sha).expect("oid")).expect("commit");
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

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn merge_conflict_reason_survives_the_wire() {
    let dir = support::own_git_data_dir("merge-wire").await;
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "carol").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let created = write
        .create(Request::new(CreateListRequest {
            owner_id: owner.to_string(),
            slug: "merge-wire".into(),
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
            steps: vec![step("Общий")],
        }))
        .await
        .expect("create")
        .into_inner();
    let list_id = uuid::Uuid::parse_str(&created.id).expect("uuid");
    let rr = Some(RepoRef { owner: "carol".into(), slug: "merge-wire".into() });

    // Подготовка конфликта делается ВНУТРИ процесса намеренно: проверяется доставка
    // причины, а не способ построения конфликта. Так тест остаётся про одно.
    let svc = GitCoreSvc { pool: pool.clone() };
    svc.create_branch(Request::new(CreateBranchRequest {
        repo: rr.clone(),
        name: "pr-wire".into(),
        from: String::new(),
    }))
    .await
    .expect("create_branch");

    let bare = dir.path.join(format!("{list_id}.git"));
    let base = list_json_at(&bare, &main_tip(&bare));
    let branch_canon = base.replacen("Общий", "Версия ветки", 1);
    let content = svc
        .parse_canon(Request::new(ParseCanonRequest { repo: rr.clone(), canon: branch_canon }))
        .await
        .expect("канон разбирается")
        .into_inner()
        .content;

    svc.commit_to_branch(Request::new(CommitToBranchRequest {
        repo: rr.clone(),
        branch: "pr-wire".into(),
        content,
        message: "правка ветки".into(),
        expected_tip: String::new(),
        author_name: "Аня".into(),
        author_email: "anya@example.com".into(),
    }))
    .await
    .expect("commit_to_branch");

    // main правит ТО ЖЕ место — отсюда конфликт.
    setfork_core::db::add_version(&pool, list_id, "web", &[proj_step("Версия main")])
        .await
        .expect("add_version");
    svc.list_branches(Request::new(RepoRef { owner: "carol".into(), slug: "merge-wire".into() }))
        .await
        .expect("list_branches");

    // Теперь — НАСТОЯЩИЙ сервер и НАСТОЯЩИЙ клиент.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = probe.local_addr().expect("addr");
    drop(probe);
    let served = GitCoreSvc { pool: pool.clone() };
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder().add_service(GitCoreServer::new(served)).serve(addr).await;
    });

    let mut client = {
        let mut c = None;
        for _ in 0..50 {
            if let Ok(x) = GitCoreClient::connect(format!("http://{addr}")).await {
                c = Some(x);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        c.expect("сервер не поднялся за секунду")
    };

    let err = client
        .merge_branch(MergeBranchRequest {
            repo: rr.clone(),
            name: "pr-wire".into(),
            mode: String::new(),
            message: "Слияние".into(),
        })
        .await
        .expect_err("правки одного места обязаны дать конфликт");

    assert_eq!(err.code(), tonic::Code::FailedPrecondition, "код отказа прежний");
    assert_eq!(
        err.metadata().get(setfork_core::reason::REASON_KEY).and_then(|v| v.to_str().ok()),
        Some("CONFLICT"),
        "причина не пережила транспорт — фронт не отличит конфликт от других отказов с тем же кодом \
         (под FailedPrecondition их четыре)"
    );
}
