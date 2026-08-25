//! Линза 06 §4: пути записи, идущие через git2, обязаны ПАКОВАТЬ объекты.
//!
//! `gc --auto` звался только из материализации и досыпки версий, а веточная запись и
//! слияния создают объекты через git2 — авто-gc там не срабатывает по устройству
//! (в отличие от `receive-pack`). Замер: 4 loose-объекта на веточную правку, 165 после
//! сорока, ни одного пака. Список, живущий ОДНИМИ предложениями, не паковался бы никогда.
//!
//! Здесь проверяется самый ускользающий путь — SQUASH-слияние: оно уходит из закрытия
//! раньше прочих, и первая версия правки его пропустила (авто-ревью на #106).
//!
//! ⚠️ Условия подобраны под НАСТОЯЩУЮ эвристику git, а не под догадку о ней: `gc --auto`
//! считает loose-объекты только в выборочной папке `objects/17`, сравнивает СТРОГО и по
//! умолчанию уходит в фон.
mod support;

use setfork_core::pb::git_core_server::GitCore;
use setfork_core::pb::{
    CommitToBranchRequest, CreateBranchRequest, ListContent, MergeBranchRequest, RepoRef, SnapshotStep,
};
use setfork_core::services::git_core::GitCoreSvc;
use tonic::Request;
use uuid::Uuid;

fn шаг(title: &str) -> SnapshotStep {
    SnapshotStep {
        n: 1,
        title: title.into(),
        desc: String::new(),
        command: String::new(),
        level: "required".into(),
        why: String::new(),
        section: String::new(),
        subtasks: vec![],
        refs: vec![],
        image_key: String::new(),
        r#type: String::new(),
        content_json: String::new(),
        block_id: String::new(),
        needs_human: false,
        needs_human_ask: String::new(),
        danger: false,
    }
}

fn содержимое(title: &str) -> ListContent {
    ListContent {
        title: title.into(),
        desc: String::new(),
        tags: vec![],
        ordered: true,
        version: 1,
        steps: vec![шаг(title)],
    }
}

/// Лежит ли объект РОССЫПЬЮ (`objects/ab/cdef…`). Упакованный — не лежит.
///
/// Считать паки бесполезно: `git gc` перепаковывает всё в ОДИН пак, и число не
/// меняется. Первая версия пробы считала именно паки — и была зелёной без починки,
/// что показала мутация. Здесь спрашивается ровно то, что нужно: попал ли коммит
/// слияния в пак.
fn лежит_россыпью(bare: &std::path::Path, sha: &str) -> bool {
    bare.join("objects").join(&sha[..2]).join(&sha[2..]).exists()
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn squash_слияние_пакует_объекты() {
    support::ensure_git_data_dir();
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "packer").await;
    let id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) \
         values ($1, 'gc-squash', '{\"en\":\"GC\"}', 1) returning id",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .expect("seed template");
    sqlx::query("insert into template_versions (template_id, version, note) values ($1, 1, 'v1')")
        .bind(id)
        .execute(&pool)
        .await
        .expect("seed v1");

    let svc = GitCoreSvc { pool: pool.clone() };
    let repo = || Some(RepoRef { owner: "packer".into(), slug: "gc-squash".into() });
    svc.create_branch(Request::new(CreateBranchRequest {
        repo: repo(),
        name: "pr".into(),
        from: String::new(),
    }))
    .await
    .expect("ветка");

    // Условия эвристики: порог, синхронный прогон и непустая выборочная папка.
    let bare = setfork_core::git::repo::repo_path(id);
    {
        let r = git2::Repository::open_bare(&bare).expect("open");
        let mut cfg = r.config().expect("config");
        cfg.set_i32("gc.auto", 1).expect("порог");
        cfg.set_bool("gc.autoDetach", false).expect("без фона");
        let mut i = 0;
        while std::fs::read_dir(bare.join("objects/17")).map(|d| d.count()).unwrap_or(0) < 3 {
            r.blob(format!("наполнитель {i}").as_bytes()).expect("blob");
            i += 1;
            assert!(i < 8000, "не удалось наполнить objects/17 — эвристика git изменилась?");
        }
    }

    svc.commit_to_branch(Request::new(CommitToBranchRequest {
        repo: repo(),
        branch: "pr".into(),
        content: Some(содержимое("Правка ветки")),
        message: "правка".into(),
        expected_tip: String::new(),
        author_name: String::new(),
        author_email: String::new(),
    }))
    .await
    .expect("правка в ветку");

    let слияние = svc
        .merge_branch(Request::new(MergeBranchRequest {
            repo: repo(),
            name: "pr".into(),
            mode: "squash".into(),
            message: "Сплющенное".into(),
        }))
        .await
        .expect("squash-слияние")
        .into_inner();

    assert!(
        !лежит_россыпью(&bare, &слияние.tip_sha),
        "коммит squash-слияния {} остался россыпью — значит gc на этом пути не звался",
        слияние.tip_sha
    );
}
