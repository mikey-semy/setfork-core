//! ПРОБНИК (не для коммента): что делает пуш, правящий только `steps/*.md`.
//!
//! Правило (project.rs:270): `list.json` — источник истины для НАБОРА и порядка
//! блоков, `steps/NN-*.md` — пер-шаговые оверрайды title/desc/command по номеру NN.
//! `projection.rs` покрывает один случай: правка .md существующего шага доезжает.
//!
//! Проверяю края, где правка пользователя может пропасть МОЛЧА — а пуш при этом
//! успешен и человек уверен, что сохранил.
mod support;

use std::collections::HashMap;
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
        .expect("spawn git")
}

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let o = git(cwd, args);
    assert!(o.status.success(), "git {args:?} упал:\n{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn ser(n: i32, title: &str) -> SerStep {
    SerStep {
        n,
        block_type: None,
        content: serde_json::Value::Null,
        block_id: None,
        title: title.into(),
        desc: String::new(),
        command: String::new(),
        level: "required".into(),
        why: String::new(),
        section: String::new(),
        subtasks: vec![],
        refs: vec![],
    }
}

fn ver(steps: Vec<SerStep>) -> VersionData {
    VersionData {
        version: 1,
        note: "initial".into(),
        ts: 1_700_000_001,
        title: "MD Probe".into(),
        desc: String::new(),
        tags: vec![],
        ordered: true,
        steps,
    }
}

/// Список в БД + bare + рабочая копия.
async fn setup(pool: &sqlx::PgPool, slug: &str, steps: Vec<SerStep>) -> (Uuid, Tmp, std::path::PathBuf, std::path::PathBuf) {
    let owner = support::seed_user(pool, &format!("md{}", &Uuid::new_v4().to_string()[..8])).await;
    let list_id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) values ($1, $2, '{\"en\":\"MD\"}', 1) returning id",
    )
    .bind(owner)
    .bind(slug)
    .fetch_one(pool)
    .await
    .expect("seed template");
    sqlx::query("insert into template_versions (template_id, version, note) values ($1, 1, 'initial')")
        .bind(list_id)
        .execute(pool)
        .await
        .expect("seed v1");

    let root = Tmp(std::env::temp_dir().join(format!("setfork-md-{}", Uuid::new_v4())));
    std::fs::create_dir_all(&root.0).expect("mkdir");
    let bare = root.0.join("repo.git");
    bundle::bootstrap_bare(&[ver(steps)], &bare).expect("bootstrap");
    let work = root.0.join("work");
    git_ok(&root.0, &["clone", "-q", bare.to_str().unwrap(), work.to_str().unwrap()]);
    (list_id, root, bare, work)
}

async fn titles(pool: &sqlx::PgPool, list_id: Uuid, version: i32) -> Vec<String> {
    // steps привязаны к версии через version_id, а title — jsonb-локаль.
    sqlx::query_scalar(
        "select coalesce(s.title->>'en', s.title::text) from steps s          join template_versions v on v.id = s.version_id          where v.template_id = $1 and v.version = $2 order by s.n",
    )
    .bind(list_id)
    .bind(version)
    .fetch_all(pool)
    .await
    .expect("titles")
}

/// Пользователь правит существующий .md — правка обязана доехать (контроль,
/// что стенд собран верно).
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres) и git"]
async fn правка_существующего_md_доезжает() {
    let pool = support::pool_with_schema().await;
    let (list_id, _root, bare, work) = setup(&pool, "md-ok", vec![ser(1, "Первый"), ser(2, "Второй")]).await;

    let f = std::fs::read_dir(work.join("steps"))
        .expect("steps/")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("01"))
        .expect("01-*.md");
    let text = std::fs::read_to_string(&f).expect("read");
    std::fs::write(&f, text.replacen("Первый", "Первый (из md)", 1)).expect("write");
    git_ok(&work, &["commit", "-am", "правка md"]);
    assert!(git(&work, &["push", "origin", "main"]).status.success(), "push");

    let v = project::project_pushed_commit(&pool, list_id, &bare).await.expect("projection");
    let v = v.expect("версия создана");
    println!("КОНТРОЛЬ: версия v{v}, шаги {:?}", titles(&pool, list_id, v).await);
    assert!(titles(&pool, list_id, v).await.iter().any(|t| t.contains("(из md)")), "правка .md доезжает");
}

/// Пользователь ДОБАВЛЯЕТ файл steps/03-новый.md, не тронув list.json.
/// Ожидание человека: «добавил шаг». Что на самом деле?
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres) и git"]
async fn новый_md_без_записи_в_list_json() {
    let pool = support::pool_with_schema().await;
    let (list_id, _root, bare, work) = setup(&pool, "md-extra", vec![ser(1, "Первый"), ser(2, "Второй")]).await;

    std::fs::write(
        work.join("steps").join("03-tretij.md"),
        "# Третий шаг\n\nДобавил руками через git.\n",
    )
    .expect("write");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-m", "добавил шаг файлом"]);
    let push = git(&work, &["push", "origin", "main"]);
    println!(
        "PUSH: success={} stderr={}",
        push.status.success(),
        String::from_utf8_lossy(&push.stderr).trim()
    );

    let v = project::project_pushed_commit(&pool, list_id, &bare).await.expect("projection");
    match v {
        Some(v) => {
            let t = titles(&pool, list_id, v).await;
            println!("ВЕРСИЯ v{v}, шаги: {t:?}");
            assert_eq!(t.len(), 2, "шаг из одного .md в набор НЕ попадает — набор задаёт list.json");
        }
        None => println!("ВЕРСИЯ НЕ СОЗДАНА (list.json не менялся)"),
    }
}

/// Номера шагов в list.json не подряд (1 и 5). Оверрайды ищутся по номеру шага,
/// а файлы при материализации называются по нему же — проверяем, что связь не рвётся.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres) и git"]
async fn разреженная_нумерация_шагов_не_рвёт_оверрайд() {
    let pool = support::pool_with_schema().await;
    let (list_id, _root, bare, work) = setup(&pool, "md-sparse", vec![ser(1, "Первый"), ser(5, "Пятый")]).await;

    let files: HashMap<String, std::path::PathBuf> = std::fs::read_dir(work.join("steps"))
        .expect("steps/")
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
        .collect();
    println!("ФАЙЛЫ steps/: {:?}", files.keys().collect::<Vec<_>>());

    // Правим файл, отвечающий шагу с n=5.
    let f = files
        .iter()
        .find(|(name, _)| name.starts_with("05"))
        .map(|(_, p)| p.clone())
        .expect("файл шага n=5 должен называться 05-*");
    let text = std::fs::read_to_string(&f).expect("read");
    std::fs::write(&f, text.replacen("Пятый", "Пятый (из md)", 1)).expect("write");
    git_ok(&work, &["commit", "-am", "правка пятого"]);
    assert!(git(&work, &["push", "origin", "main"]).status.success(), "push");

    let v = project::project_pushed_commit(&pool, list_id, &bare).await.expect("projection").expect("версия");
    let t = titles(&pool, list_id, v).await;
    println!("ШАГИ ПОСЛЕ ПРАВКИ: {t:?}");
    assert!(t.iter().any(|x| x.contains("(из md)")), "оверрайд при разреженной нумерации потерян: {t:?}");
}

/// Пуш, который НЕ трогает ни list.json, ни steps/ (правка README).
/// Создаётся ли новая версия списка? История версий должна отражать изменения
/// содержания, а не каждый коммит.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres) и git"]
async fn пуш_только_в_readme() {
    let pool = support::pool_with_schema().await;
    let (list_id, _root, bare, work) = setup(&pool, "md-readme", vec![ser(1, "Первый")]).await;

    std::fs::write(work.join("README.md"), "# Заметка\n\nправка только README\n").expect("write");
    git_ok(&work, &["commit", "-am", "правка README"]);
    assert!(git(&work, &["push", "origin", "main"]).status.success(), "push");

    let v = project::project_pushed_commit(&pool, list_id, &bare).await.expect("projection");
    let count: i64 = sqlx::query_scalar("select count(*) from template_versions where template_id = $1")
        .bind(list_id)
        .fetch_one(&pool)
        .await
        .expect("count");
    println!("ПУШ В README → проекция {v:?}, всего версий {count}");
}
