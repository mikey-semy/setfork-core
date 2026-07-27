//! Интеграционный тест обратной проекции git → БД — сердца Фазы 0
//! (project_pushed_commit): полный прод-путь «пользователь запушил коммит с
//! изменённым list.json → в БД появилась новая версия с шагами + git-тег vN».
//!
//! Появился по итогам cargo-mutants 2026-07-20: вся цепочка read_tip →
//! parse_steps → add_version → tag_version была покрыта только косвенно
//! (38 пропущенных мутантов, почти все в project.rs).
//!
//! Запуск: TEST_DATABASE_URL=... cargo test -- --include-ignored
//! (или bash scripts/ci-local.sh).
mod support;

use std::path::Path;
use std::process::Command;

use setfork_core::git::bundle::{self, SerStep, VersionData};
use setfork_core::git::project;
use uuid::Uuid;

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["-c", "user.email=test@setfork.com", "-c", "user.name=Tester"])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(out.status.success(), "git {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn pushed_commit_projects_new_version_and_tag() {
    let pool = support::pool_with_schema().await;

    // Список в БД: v1 с одним шагом (как после обычного веб-создания).
    let owner = support::seed_user(&pool, "pusher").await;
    let list_id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) \
         values ($1, 'pushed', '{\"en\":\"Pushed List\"}', 1) returning id",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .expect("seed template");
    sqlx::query("insert into template_versions (template_id, version, note) values ($1, 1, 'initial')")
        .bind(list_id)
        .execute(&pool)
        .await
        .expect("seed v1");

    // Bare-репо из v1 (тот же материализатор, что у ensure_repo).
    let root = std::env::temp_dir().join(format!("setfork-proj-{}", Uuid::new_v4()));
    let bare = root.join("repo.git");
    let v1 = VersionData {
        version: 1,
        note: "initial".into(),
        ts: 1_700_000_001,
        title: "Pushed List".into(),
        desc: String::new(),
        tags: vec![],
        ordered: true,
        steps: vec![SerStep {
            n: 1,
            block_type: None,
            content: serde_json::Value::Null,
            block_id: None,
            title: "First".into(),
            desc: String::new(),
            command: String::new(),
            level: "required".into(),
            why: String::new(),
            section: String::new(),
            subtasks: vec![],
            refs: vec![],
        }],
    };
    bundle::bootstrap_bare(&[v1], &bare).expect("bootstrap");

    // «Пользовательский push»: клон → правка list.json (новый title + второй шаг
    // + md-оверрайд первого) → commit → push (file-транспорт, через pre-receive).
    let work = root.join("work");
    git(&root, &["clone", "-q", bare.to_str().unwrap(), work.to_str().unwrap()]);
    let list_json = serde_json::json!({
        "title": "Pushed List v2",
        "desc": "now with desc",
        "tags": ["pushed"],
        "ordered": true,
        "version": 2,
        "steps": [
            { "n": 1, "title": "First", "desc": "", "command": "", "level": "required",
              "why": "", "section": "", "subtasks": [], "refs": [] },
            { "n": 2, "title": "Second", "desc": "added by push", "command": "echo 2",
              "level": "optional", "why": "", "section": "", "subtasks": [], "refs": [] }
        ]
    });
    std::fs::write(work.join("list.json"), serde_json::to_string_pretty(&list_json).unwrap())
        .expect("write list.json");
    // md-оверрайд шага 1: title из .md должен перекрыть list.json (правило parse_steps).
    std::fs::write(
        work.join("steps").join("01-first.md"),
        "---\ntitle: \"First (edited)\"\nlevel: required\n---\n\noverridden desc\n",
    )
    .expect("write step md");
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "v2: user push"]);
    git(&work, &["push", "-q", "origin", "main"]);

    // Проекция — тот же вызов, что делает receive_pack после сдвига main.
    let ver = project::project_pushed_commit(&pool, list_id, &bare)
        .await
        .expect("проекция не должна падать")
        .expect("есть что проецировать");
    assert_eq!(ver, 2, "v1 + push = версия 2");

    // БД: current_version поднят, мета обновлена, шаги совпадают с деревом.
    let (cur, title): (i32, serde_json::Value) =
        sqlx::query_as("select current_version, title from templates where id = $1")
            .bind(list_id)
            .fetch_one(&pool)
            .await
            .expect("template row");
    assert_eq!(cur, 2);
    assert_eq!(title["en"], "Pushed List v2", "update_meta применил title из list.json");

    let steps: Vec<(i32, serde_json::Value, String, String)> = sqlx::query_as(
        "select s.n, s.title, s.command, s.level::text from steps s \
         join template_versions tv on tv.id = s.version_id \
         where tv.template_id = $1 and tv.version = 2 order by s.n",
    )
    .bind(list_id)
    .fetch_all(&pool)
    .await
    .expect("steps of v2");
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].1["en"], "First (edited)", "md-оверрайд перекрыл title из list.json");
    assert_eq!(steps[1].1["en"], "Second");
    assert_eq!(steps[1].2, "echo 2");
    assert_eq!(steps[1].3, "optional");

    // Git: на запушенный tip поставлен тег v2 (важно для ленивой досыпки ensure_repo).
    assert_eq!(bundle::max_tag_version(&bare), 2, "tag_version поставил v2");

    // Снапшоты читают тот же tip (branch_snapshot/commit_snapshot — один код-пас).
    let snap = project::branch_snapshot(&bare, "refs/heads/main").expect("snapshot");
    assert_eq!(snap.title, "Pushed List v2");
    assert_eq!(snap.steps.len(), 2);
    assert_eq!(snap.steps[0].title, "First (edited)");

    // Повторная проекция того же tip'а — создаёт v3 (семантика reproject:
    // инструмент оператора не проверяет дубли; фиксируем осознанно).
    let again = project::project_pushed_commit(&pool, list_id, &bare)
        .await
        .expect("повторная проекция")
        .expect("проецируемо");
    assert_eq!(again, 3);

    let _ = std::fs::remove_dir_all(&root);
}
