//! ПРОБА линзы проверки ядра: GetMergeState — тройка для разрешения конфликта.
//!
//! Этот путь, как выяснилось в заходе 20, срабатывает часто: list.json один, и
//! правки в вебе, пока предложение открыто, дают конфликт. Ошибка в базе (base)
//! означает, что пользователь разрешает конфликт относительно НЕ ТОЙ версии —
//! и молча теряет правки, которые считает сохранёнными.
//!
//! Проверяем не «код выглядит верным», а сверку с настоящим git merge-base.
#![cfg(feature = "probes")]

mod support;

use setfork_core::pb::git_core_server::GitCore;
use setfork_core::pb::{CommitToBranchRequest, CreateBranchRequest, MergeStateRequest, RepoRef};
use setfork_core::pb_domain::list_write_server::ListWrite;
use setfork_core::pb_domain::{CreateListRequest, LocaleText, NewStep};
use setfork_core::services::git_core::GitCoreSvc;
use setfork_core::services::list::ListWriteSvc;
use tonic::Request;

struct Tmp(std::path::PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
    }
}

fn proj(title: &str) -> setfork_core::git::project::ProjStep {
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
    }
}

async fn git_data_dir() -> (Tmp, tokio::sync::MutexGuard<'static, ()>) {
    let guard = support::GIT_DATA_DIR_LOCK.lock().await;
    let p = std::env::temp_dir().join(format!("setfork-ms-probe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).expect("mkdir");
    unsafe { std::env::set_var("GIT_DATA_DIR", &p) };
    (Tmp(p), guard)
}

fn list_json_at(bare: &std::path::Path, sha: &str) -> String {
    let repo = git2::Repository::open_bare(bare).expect("open");
    let c = repo.find_commit(git2::Oid::from_str(sha).expect("oid")).expect("commit");
    let id = c.tree().expect("tree").get_name("list.json").expect("list.json").id();
    String::from_utf8_lossy(repo.find_blob(id).expect("blob").content()).to_string()
}

/// База для трёхстороннего слияния обязана совпадать с тем, что считает настоящий git.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres) и git в PATH"]
async fn база_совпадает_с_настоящим_git_merge_base() {
    let (dir, _git_dir_guard) = git_data_dir().await;
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "alice").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let created = write
        .create(Request::new(CreateListRequest {
            owner_id: owner.to_string(),
            slug: "ms-probe".into(),
            title: lt("Probe"),
            desc: lt("d"),
            tags: vec![],
            ordered: true,
            visibility: String::new(),
            status: String::new(),
            origin: String::new(),
            forked_from_id: String::new(),
            note: "v1".into(),
            steps: vec![step("Первый"), step("Второй")],
        }))
        .await
        .expect("create")
        .into_inner();
    let list_id = uuid::Uuid::parse_str(&created.id).expect("uuid");
    let git = GitCoreSvc { pool: pool.clone() };
    let rr = Some(RepoRef { owner: "alice".into(), slug: "ms-probe".into() });

    git.create_branch(Request::new(CreateBranchRequest {
        repo: rr.clone(),
        name: "pr-ms".into(),
        from: String::new(),
    }))
    .await
    .expect("create_branch");

    let bare = dir.0.join(format!("{list_id}.git"));
    let base_before = list_json_at(&bare, &{
        git2::Repository::open_bare(&bare)
            .expect("o")
            .refname_to_id("refs/heads/main")
            .expect("m")
            .to_string()
    });

    // Ветка правит своё.
    git.commit_to_branch(Request::new(CommitToBranchRequest {
        repo: rr.clone(),
        branch: "pr-ms".into(),
        list_json: base_before.replacen("Второй", "Второй (ветка)", 1).into_bytes(),
        message: "правка ветки".into(),
        expected_tip: String::new(),
        author_name: "Аня".into(),
        author_email: "anya@example.com".into(),
    }))
    .await
    .expect("commit_to_branch");

    // main уезжает вперёд.
    setfork_core::db::add_version(&pool, list_id, "web", &[proj("Первый (main)"), proj("Второй")])
        .await
        .expect("add_version");
    git.list_branches(Request::new(RepoRef { owner: "alice".into(), slug: "ms-probe".into() }))
        .await
        .expect("materialize");

    let st = git
        .get_merge_state(Request::new(MergeStateRequest { repo: rr.clone(), branch: "pr-ms".into() }))
        .await
        .expect("get_merge_state")
        .into_inner();
    assert!(st.found, "ветка и merge-base существуют");

    // Сверка с настоящим git — не с нашим же кодом.
    let out = std::process::Command::new("git")
        .args(["--git-dir", &bare.to_string_lossy(), "merge-base", "main", "pr-ms"])
        .output()
        .expect("git merge-base");
    let real = String::from_utf8_lossy(&out.stdout).trim().to_string();
    println!("ЯДРО: {}\nGIT:  {real}", st.merge_base_sha);
    assert_eq!(st.merge_base_sha, real, "база расходится с настоящим git");

    // И тройка не перепутана местами: ours = main, theirs = ветка.
    let ours = st.ours.expect("ours");
    let theirs = st.theirs.expect("theirs");
    let base = st.base.expect("base");
    let titles = |s: &setfork_core::pb::BranchSnapshotResponse| {
        s.steps.iter().map(|x| x.title.clone()).collect::<Vec<_>>().join(" | ")
    };
    println!("BASE:   {}", titles(&base));
    println!("OURS:   {}", titles(&ours));
    println!("THEIRS: {}", titles(&theirs));
    assert!(titles(&ours).contains("Первый (main)"), "ours обязан быть main: {}", titles(&ours));
    assert!(titles(&theirs).contains("Второй (ветка)"), "theirs обязан быть веткой: {}", titles(&theirs));
    assert!(
        !titles(&base).contains("(main)") && !titles(&base).contains("(ветка)"),
        "base — общий предок без правок обеих сторон: {}",
        titles(&base)
    );
}
