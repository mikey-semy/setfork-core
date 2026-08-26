//! ПРОБА линзы проверки ядра: пуш с БИТЫМ list.json.
//!
//! Хук требует лишь НАЛИЧИЯ list.json (`git cat-file -e`), содержимое он не
//! разбирает. Значит коммит с невалидным JSON штатно доезжает до bare — и дальше
//! всё решает проекция. Тестами покрыт только валидный случай (projection.rs).
//!
//! Что должно быть: git-объекты сохранены (пуш пользователя не теряем), НОВАЯ
//! версия не создаётся, текущая версия НЕ затирается пустотой.
#![cfg(feature = "probes")]

mod support;

use std::path::Path;
use std::process::Command;

use setfork_core::git::bundle::{self, SerStep, VersionData};
use setfork_core::git::project;
use uuid::Uuid;

struct Tmp(std::path::PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(cwd)
        .args(["-c", "user.email=probe@setfork.com", "-c", "user.name=Probe"])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"))
}

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let o = git(cwd, args);
    assert!(o.status.success(), "git {args:?} упал:\n{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn v1(title: &str) -> VersionData {
    VersionData {
        version: 1,
        note: "initial".into(),
        ts: 1_700_000_001,
        title: title.into(),
        desc: String::new(),
        tags: vec![],
        ordered: true,
        kind: None,
        steps: vec![SerStep {
            n: 1,
            block_type: None,
            content: serde_json::Value::Null,
            block_id: None,
            image_key: None,
            needs_human: false,
            needs_human_ask: None,
            danger: false,
            title: "Первый".into(),
            desc: String::new(),
            command: String::new(),
            level: "required".into(),
            why: String::new(),
            section: String::new(),
            subtasks: vec![],
            refs: vec![],
        }],
    }
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres) и git в PATH"]
async fn broken_list_json_creates_no_version_and_keeps_the_current_one() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "pusher").await;
    let list_id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) \
         values ($1, 'broken', '{\"en\":\"Broken Probe\"}', 1) returning id",
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

    let root = Tmp(std::env::temp_dir().join(format!("setfork-broken-{}", Uuid::new_v4())));
    std::fs::create_dir_all(&root.0).expect("mkdir");
    let bare = root.0.join("repo.git");
    bundle::bootstrap_bare(&[v1("Broken Probe")], &bare).expect("bootstrap");

    let work = root.0.join("work");
    git_ok(&root.0, &["clone", "-q", bare.to_str().unwrap(), work.to_str().unwrap()]);

    // Пользователь ломает канон и пушит. Файл на месте — хук пропустит.
    std::fs::write(work.join("list.json"), "{ это не json, а заметка руками").expect("write");
    git_ok(&work, &["commit", "-am", "правка руками"]);
    let pushed = git_ok(&work, &["rev-parse", "HEAD"]);
    let push = git(&work, &["push", "origin", "main"]);
    println!(
        "PUSH БИТОГО JSON: success={} stderr={}",
        push.status.success(),
        String::from_utf8_lossy(&push.stderr).trim()
    );
    assert!(push.status.success(), "хук проверяет только наличие файла — пуш проходит");

    // Проекция.
    let res = project::project_pushed_commit(&pool, list_id, &bare).await;
    println!("ПРОЕКЦИЯ: {res:?}");

    let versions: Vec<i32> =
        sqlx::query_scalar("select version from template_versions where template_id = $1 order by version")
            .bind(list_id)
            .fetch_all(&pool)
            .await
            .expect("versions");
    let current: i32 = sqlx::query_scalar("select current_version from templates where id = $1")
        .bind(list_id)
        .fetch_one(&pool)
        .await
        .expect("current");
    println!("ВЕРСИИ: {versions:?}, current_version={current}");

    // 1. Пуш пользователя не потерян — git-объекты на месте.
    assert_eq!(git_ok(&bare, &["rev-parse", "main"]), pushed, "коммит пользователя в bare");

    // 2. Битый канон НЕ должен порождать версию.
    assert_eq!(versions, vec![1], "новая версия из битого JSON не создаётся: {versions:?}");
    assert_eq!(current, 1, "текущая версия не сдвинулась");

    // 3. И это не молчаливая «успешная» проекция.
    match res {
        Ok(None) => println!("ВЕРДИКТ: проецировать нечего — корректно"),
        Ok(Some(v)) => panic!("проекция отчиталась об успехе (v{v}) на битом JSON"),
        Err(e) => println!("ВЕРДИКТ: ошибка проекции — {e}"),
    }
}
