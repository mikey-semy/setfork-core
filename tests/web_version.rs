//! Интеграционные тесты git-first записи версии (Ф1 — один путь записи):
//! веб-правка сначала становится коммитом main, потом строками БД; веб и push
//! дают одинаковый результат; отставшие/убежавшие репо выравниваются, глубокое
//! расхождение останавливает запись вместо тихой порчи.
//!
//! Запуск: TEST_DATABASE_URL=... cargo test -- --include-ignored
//! (или bash scripts/ci-local.sh).
mod support;

use std::path::Path;
use std::process::Command;

use setfork_core::db::{Level, StepRow};
use setfork_core::git::bundle::{self, SerStep, VersionData};
use setfork_core::git::version::{
    SyncOutcome, WebEdit, WebVersionError, commit_web_version, sync_repo_with_db,
};
use sqlx::postgres::PgPool;
use uuid::Uuid;

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(cwd)
        // Ф5: от лица ВЛАДЕЛЬЦА — без роли хук считает пушащего посторонним.
        .env("SETFORK_ROLE", "owner")
        .args(["-c", "user.email=test@setfork.com", "-c", "user.name=Tester"])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(out.status.success(), "git {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
}

/// Каталог-однодневка: удаляется на Drop (в т.ч. при panic внутри теста).
struct Tmp(std::path::PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tmp_root(prefix: &str) -> Tmp {
    let p = std::env::temp_dir().join(format!("setfork-{prefix}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&p).expect("create tmp root");
    Tmp(p)
}

/// Список в БД: v1 с одним шагом «First» (как после обычного веб-создания).
async fn seed_list(pool: &PgPool, handle: &str, slug: &str, title: &str) -> Uuid {
    let owner = support::seed_user(pool, handle).await;
    let list_id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) \
         values ($1, $2, $3::jsonb, 1) returning id",
    )
    .bind(owner)
    .bind(slug)
    .bind(serde_json::json!({ "en": title }))
    .fetch_one(pool)
    .await
    .expect("seed template");
    let ver_id: Uuid = sqlx::query_scalar(
        "insert into template_versions (template_id, version, note) values ($1, 1, 'initial') returning id",
    )
    .bind(list_id)
    .fetch_one(pool)
    .await
    .expect("seed v1");
    sqlx::query(
        "insert into steps (version_id, n, type, title, level) values ($1, 1, 'step', '{\"en\":\"First\"}', 'required')",
    )
    .bind(ver_id)
    .execute(pool)
    .await
    .expect("seed step");
    list_id
}

fn ser_step(n: i32, title: &str) -> SerStep {
    SerStep {
        n,
        block_type: None,
        content: serde_json::Value::Null,
        block_id: None,
        title: title.into(),
        desc: String::new(),
        command: String::new(),
        image_key: None,
        level: "required".into(),
        needs_human: false,
        needs_human_ask: None,
        danger: false,
        why: String::new(),
        section: String::new(),
        subtasks: vec![],
        refs: vec![],
    }
}

/// VersionData v1, байт-идентичный тому, что bootstrap соберёт из seed_list.
async fn v1_data(pool: &PgPool, list_id: Uuid, title: &str) -> VersionData {
    let ts: i64 = sqlx::query_scalar(
        "select floor(extract(epoch from created_at))::bigint from template_versions \
         where template_id = $1 and version = 1",
    )
    .bind(list_id)
    .fetch_one(pool)
    .await
    .expect("v1 ts");
    VersionData {
        version: 1,
        note: "initial".into(),
        ts,
        title: title.into(),
        desc: String::new(),
        tags: vec![],
        ordered: true,
        kind: None,
        steps: vec![ser_step(1, "First")],
    }
}

fn step_row(title: &str, command: &str, level: Level) -> StepRow {
    StepRow {
        block_type: "step".into(),
        content: serde_json::json!({}),
        block_id: None,
        title: serde_json::json!({ "en": title }),
        desc: serde_json::json!({}),
        command: command.into(),
        image_key: None,
        level,
        why: serde_json::json!({}),
        section: serde_json::json!({}),
        subtasks: serde_json::json!([]),
        refs: serde_json::json!([]),
        needs_human: false,
        needs_human_ask: serde_json::json!({}),
        danger: false,
    }
}

fn tip_list_json(bare: &Path) -> Vec<u8> {
    let repo = git2::Repository::open_bare(bare).expect("open bare");
    let tip = repo.refname_to_id("refs/heads/main").expect("main");
    let commit = repo.find_commit(tip).expect("commit");
    let entry = commit.tree().expect("tree").get_path(Path::new("list.json")).expect("list.json");
    repo.find_blob(entry.id()).expect("blob").content().to_vec()
}

fn main_tip(bare: &Path) -> String {
    git2::Repository::open_bare(bare)
        .expect("open bare")
        .refname_to_id("refs/heads/main")
        .expect("main")
        .to_string()
}

/// Строки версии из БД в сравнимой форме (общее для веб- и push-пути).
async fn db_steps(
    pool: &PgPool,
    list_id: Uuid,
    version: i32,
) -> Vec<(i32, String, serde_json::Value, String, String)> {
    sqlx::query_as(
        "select s.n, s.type, s.title, s.command, s.level::text from steps s \
         join template_versions tv on tv.id = s.version_id \
         where tv.template_id = $1 and tv.version = $2 order by s.n",
    )
    .bind(list_id)
    .bind(version)
    .fetch_all(pool)
    .await
    .expect("steps of version")
}

/// ВЕБ-ПРАВКА ИДЁТ GIT-FIRST: коммит vN на main + тег + строки БД одной операцией.
/// И воспроизводимость: репо, пересобранный из БД, даёт ТОТ ЖЕ SHA tip —
/// значит канон, собранный до вставки строк, равен канону из вставленных строк.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_web_version_commits_git_first_and_projects() {
    let pool = support::pool_with_schema().await;
    let list_id = seed_list(&pool, "webwriter", "web", "Web List").await;

    let root = tmp_root("webver");
    let bare = root.0.join("repo.git");
    let v1 = v1_data(&pool, list_id, "Web List").await;
    bundle::bootstrap_bare(&[v1], &bare).expect("bootstrap");

    let rows = vec![step_row("First", "", Level::Required), step_row("Second", "echo 2", Level::Optional)];
    let out =
        commit_web_version(&pool, list_id, &bare, WebEdit::new("add second", None, rows, Default::default()))
            .await
            .unwrap_or_else(|e| panic!("веб-версия: {e:?}"));

    assert_eq!(out.version, 2);
    // Git: main сдвинут, тег v2 на tip, sha из ответа — настоящий tip.
    assert_eq!(bundle::max_tag_version(&bare), 2);
    assert_eq!(out.commit_sha, main_tip(&bare));
    let canon = String::from_utf8(tip_list_json(&bare)).expect("utf8");
    assert!(canon.contains("\"Second\""), "новый шаг в каноне: {canon}");
    assert!(canon.contains("\"Web List\""), "мета списка в каноне: {canon}");

    // БД: проекция тем же движком — current_version поднят, строки на месте.
    let cur: i32 = sqlx::query_scalar("select current_version from templates where id = $1")
        .bind(list_id)
        .fetch_one(&pool)
        .await
        .expect("current");
    assert_eq!(cur, 2);
    let steps = db_steps(&pool, list_id, 2).await;
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[1].2["en"], "Second");
    assert_eq!(steps[1].3, "echo 2");
    assert_eq!(steps[1].4, "optional");

    // Воспроизводимость: снести репо и пересобрать из БД → тот же tip SHA.
    // Это и есть гарантия «канон до вставки == канон из строк БД» (байт в байт,
    // включая ts коммита из created_at строки версии).
    let tip_before = main_tip(&bare);
    std::fs::remove_dir_all(&bare).expect("drop repo");
    let versions = setfork_core::db::load_bundle_data(&pool, list_id).await.expect("history");
    bundle::bootstrap_bare(&versions, &bare).expect("re-bootstrap");
    assert_eq!(main_tip(&bare), tip_before, "бутстрап из БД воспроизводит историю байт-в-байт");
}

/// ОДИН РЕЗУЛЬТАТ У ДВУХ ПУТЕЙ: веб-правка на списке A и push того же канона на
/// список B дают одинаковые строки БД. Формат один, движок вставки один — пути
/// физически не могут разойтись.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_web_edit_and_a_push_yield_the_same_result() {
    let pool = support::pool_with_schema().await;
    let a_id = seed_list(&pool, "sidea", "same", "Same List").await;
    let b_id = seed_list(&pool, "sideb", "same", "Same List").await;

    let root = tmp_root("parity");
    let bare_a = root.0.join("a.git");
    let bare_b = root.0.join("b.git");
    bundle::bootstrap_bare(&[v1_data(&pool, a_id, "Same List").await], &bare_a).expect("bootstrap A");
    bundle::bootstrap_bare(&[v1_data(&pool, b_id, "Same List").await], &bare_b).expect("bootstrap B");

    // A: веб-правка (git-first).
    let rows = vec![step_row("First", "", Level::Required), step_row("Second", "echo 2", Level::Optional)];
    let out =
        commit_web_version(&pool, a_id, &bare_a, WebEdit::new("same edit", None, rows, Default::default()))
            .await
            .unwrap_or_else(|e| panic!("веб-версия: {e:?}"));
    assert_eq!(out.version, 2);
    let canon_a = tip_list_json(&bare_a);

    // B: пользователь пушит РОВНО тот же канон (клон → замена файлов → push
    // через настоящий git с pre-receive) → проекция, как делает receive_pack.
    let work = root.0.join("work");
    git(&root.0, &["clone", "-q", bare_b.to_str().unwrap(), work.to_str().unwrap()]);
    std::fs::write(work.join("list.json"), &canon_a).expect("write canon");
    let _ = std::fs::remove_dir_all(work.join("steps")); // md-оверрайды не должны мешать сравнению
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "v2: same edit"]);
    git(&work, &["push", "-q", "origin", "main"]);
    let ver = setfork_core::git::project::project_pushed_commit(&pool, b_id, &bare_b)
        .await
        .expect("проекция")
        .expect("проецируемо");
    assert_eq!(ver, 2);

    // Каноны равны байт-в-байт, строки БД обеих версий — одинаковы.
    assert_eq!(tip_list_json(&bare_b), canon_a, "канон B — тот же файл");
    let steps_a = db_steps(&pool, a_id, 2).await;
    let steps_b = db_steps(&pool, b_id, 2).await;
    assert_eq!(steps_a, steps_b, "веб-правка и push спроецировались в одинаковые строки");

    // Мета тоже сошлась (push обновляет её из канона).
    let (ta, tb): (serde_json::Value, serde_json::Value) = (
        sqlx::query_scalar("select title from templates where id = $1")
            .bind(a_id)
            .fetch_one(&pool)
            .await
            .expect("ta"),
        sqlx::query_scalar("select title from templates where id = $1")
            .bind(b_id)
            .fetch_one(&pool)
            .await
            .expect("tb"),
    );
    assert_eq!(ta, tb);
}

/// РЕПО, ОТСТАВШЕЕ ОТ БД (наследие ленивой досыпки): путь записи сначала
/// догоняет недостающие версии, потом коммитит новую — дыр в истории нет.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_lagging_repo_catches_up_before_the_commit() {
    let pool = support::pool_with_schema().await;
    let list_id = seed_list(&pool, "laggard", "behind", "Behind").await;
    // v2 существует только в БД (как оставляла ленивая модель).
    let v2_id: Uuid = sqlx::query_scalar(
        "insert into template_versions (template_id, version, note) values ($1, 2, 'web only') returning id",
    )
    .bind(list_id)
    .fetch_one(&pool)
    .await
    .expect("seed v2");
    sqlx::query(
        "insert into steps (version_id, n, type, title, level) values ($1, 1, 'step', '{\"en\":\"Second only in db\"}', 'required')",
    )
    .bind(v2_id)
    .execute(&pool)
    .await
    .expect("seed v2 step");
    sqlx::query("update templates set current_version = 2 where id = $1")
        .bind(list_id)
        .execute(&pool)
        .await
        .expect("bump");

    let root = tmp_root("behind");
    let bare = root.0.join("repo.git");
    bundle::bootstrap_bare(&[v1_data(&pool, list_id, "Behind").await], &bare).expect("bootstrap v1 only");
    assert_eq!(bundle::max_tag_version(&bare), 1, "предусловие: git отстал");

    let out = commit_web_version(
        &pool,
        list_id,
        &bare,
        WebEdit::new("third", None, vec![step_row("Third", "", Level::Required)], Default::default()),
    )
    .await
    .unwrap_or_else(|e| panic!("веб-версия: {e:?}"));

    assert_eq!(out.version, 3, "после догона до v2 новая версия — v3");
    assert_eq!(bundle::max_tag_version(&bare), 3, "в git есть и догнанная v2, и новая v3");
    let repo = git2::Repository::open_bare(&bare).expect("open");
    assert!(repo.refname_to_id("refs/tags/v2").is_ok(), "дыры в тегах нет");
}

/// GIT ВПЕРЕДИ НА ОДНУ ВЕРСИЮ (сбой прошлой проекции): tip проецируется в БД,
/// затем пишется новая версия — повтор сохранения долечивает сам.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_git_that_ran_ahead_is_healed_by_projection() {
    let pool = support::pool_with_schema().await;
    let list_id = seed_list(&pool, "runner", "ahead", "Ahead").await;

    let root = tmp_root("ahead");
    let bare = root.0.join("repo.git");
    bundle::bootstrap_bare(&[v1_data(&pool, list_id, "Ahead").await], &bare).expect("bootstrap");

    // Симуляция «git успел, БД нет»: v2 только в git.
    let orphan = VersionData {
        version: 2,
        note: "committed but not projected".into(),
        ts: 1_700_000_100,
        title: "Ahead".into(),
        desc: String::new(),
        tags: vec![],
        ordered: true,
        kind: None,
        steps: vec![ser_step(1, "First"), ser_step(2, "Orphan")],
    };
    bundle::append_versions(&bare, &[orphan]).expect("append orphan v2");
    assert_eq!(bundle::max_tag_version(&bare), 2);

    let out = commit_web_version(
        &pool,
        list_id,
        &bare,
        WebEdit::new("third", None, vec![step_row("Third", "", Level::Required)], Default::default()),
    )
    .await
    .unwrap_or_else(|e| panic!("веб-версия: {e:?}"));

    assert_eq!(out.version, 3, "хвост v2 спроецирован, новая — v3");
    let cur: i32 = sqlx::query_scalar("select current_version from templates where id = $1")
        .bind(list_id)
        .fetch_one(&pool)
        .await
        .expect("current");
    assert_eq!(cur, 3);
    let v2_steps = db_steps(&pool, list_id, 2).await;
    assert_eq!(v2_steps.len(), 2, "спроецированный хвост принёс шаги из канона");
    assert_eq!(v2_steps[1].2["en"], "Orphan");
}

/// ПОСТОРОННИЙ ТЕГ vN (двойник дефекта релизных имён): запись останавливается с
/// внятной ошибкой, git и БД не тронуты — не тихая порча.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_foreign_version_name_stops_the_write() {
    let pool = support::pool_with_schema().await;
    let list_id = seed_list(&pool, "rogue", "rogue", "Rogue").await;

    let root = tmp_root("rogue");
    let bare = root.0.join("repo.git");
    bundle::bootstrap_bare(&[v1_data(&pool, list_id, "Rogue").await], &bare).expect("bootstrap");
    {
        // Чужой тег v20 (создан до резервирования имён v<число> за версиями).
        let repo = git2::Repository::open_bare(&bare).expect("open");
        let tip = repo.refname_to_id("refs/heads/main").expect("main");
        let obj = repo.find_object(tip, None).expect("obj");
        repo.tag_lightweight("v20", &obj, true).expect("rogue tag");
    }

    let err = commit_web_version(
        &pool,
        list_id,
        &bare,
        WebEdit::new("won't happen", None, vec![step_row("Nope", "", Level::Required)], Default::default()),
    )
    .await
    .expect_err("запись обязана остановиться");

    match err {
        WebVersionError::OutOfSync { have, current } => {
            assert_eq!((have, current), (20, 1));
        }
        other => panic!("ожидался OutOfSync, получено {other:?}"),
    }
    let cur: i32 = sqlx::query_scalar("select current_version from templates where id = $1")
        .bind(list_id)
        .fetch_one(&pool)
        .await
        .expect("current");
    assert_eq!(cur, 1, "БД не тронута");

    // Диагностика для sync-repos: тот же случай виден как Conflict.
    let sync = sync_repo_with_db(&pool, list_id, &bare).await.expect("sync");
    assert_eq!(sync, SyncOutcome::Conflict { have: 20, current: 1 });
}

/// ПРАВКА НА УСТАРЕВШЕЙ ВЕРСИИ НЕ ВЫТЕСНЯЕТ ЧУЖУЮ: писавший назвал версию, на
/// которой основывался, и пока он готовил правку, список ушёл вперёд. Сверка
/// живёт в той же транзакции, где строка уже взята `for update`, — снаружи такое
/// обещание недостижимо (между чужой проверкой и записью есть окно).
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn an_edit_on_a_stale_version_is_rejected() {
    let pool = support::pool_with_schema().await;
    let list_id = seed_list(&pool, "casper", "cas", "CAS List").await;

    let root = tmp_root("cas");
    let bare = root.0.join("repo.git");
    bundle::bootstrap_bare(&[v1_data(&pool, list_id, "CAS List").await], &bare).expect("bootstrap");

    // Чужая правка успела лечь: список теперь на v2.
    commit_web_version(
        &pool,
        list_id,
        &bare,
        WebEdit::new("theirs", None, vec![step_row("Theirs", "", Level::Required)], Default::default()),
    )
    .await
    .expect("чужая версия");

    // Наша правка готовилась на v1 — её обязаны отклонить.
    let err = commit_web_version(
        &pool,
        list_id,
        &bare,
        WebEdit::new(
            "mine, based on v1",
            None,
            vec![step_row("Mine", "", Level::Required)],
            Default::default(),
        )
        .based_on(Some(1)),
    )
    .await
    .expect_err("устаревшая правка обязана быть отклонена");

    match err {
        WebVersionError::VersionConflict { expected, current } => assert_eq!((expected, current), (1, 2)),
        other => panic!("ожидался VersionConflict, получено {other:?}"),
    }

    // Ничего не записано: ни версия, ни коммит — чужая правка на месте.
    let cur: i32 = sqlx::query_scalar("select current_version from templates where id = $1")
        .bind(list_id)
        .fetch_one(&pool)
        .await
        .expect("current");
    assert_eq!(cur, 2, "версия не выросла");
    assert_eq!(bundle::max_tag_version(&bare), 2, "git-канон не тронут");
    let steps = db_steps(&pool, list_id, 2).await;
    assert_eq!(steps[0].2["en"], "Theirs", "содержимое осталось чужим");

    // Та же правка на актуальной версии проходит.
    let ok = commit_web_version(
        &pool,
        list_id,
        &bare,
        WebEdit::new("mine, rebased", None, vec![step_row("Mine", "", Level::Required)], Default::default())
            .based_on(Some(2)),
    )
    .await
    .expect("правка на актуальной версии");
    assert_eq!(ok.version, 3);
}

/// ЧИТАЮЩИЙ ПУТЬ ТОЖЕ ВЫРАВНИВАЕТ ЛЕГАСИ: ensure_repo_by_id догоняет отставшее
/// репо. Без этого в окне до sync-repos push пришёл бы поверх УСТАРЕВШЕГО main
/// и молча затёр веб-версии, которые раньше делали его non-fast-forward-отказом.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn ensure_aligns_a_lagging_repo_before_access() {
    support::ensure_git_data_dir();
    let pool = support::pool_with_schema().await;
    let list_id = seed_list(&pool, "reader", "stale-read", "Stale Read").await;
    let v2_id: Uuid = sqlx::query_scalar(
        "insert into template_versions (template_id, version, note) values ($1, 2, 'web only') returning id",
    )
    .bind(list_id)
    .fetch_one(&pool)
    .await
    .expect("seed v2");
    sqlx::query(
        "insert into steps (version_id, n, type, title, level) values ($1, 1, 'step', '{\"en\":\"Second\"}', 'required')",
    )
    .bind(v2_id)
    .execute(&pool)
    .await
    .expect("seed v2 step");
    sqlx::query("update templates set current_version = 2 where id = $1")
        .bind(list_id)
        .execute(&pool)
        .await
        .expect("bump");

    // Репо на «законном» месте (GIT_DATA_DIR/<id>.git), но только с v1 — легаси.
    let bare = setfork_core::git::repo::repo_path(list_id);
    bundle::bootstrap_bare(&[v1_data(&pool, list_id, "Stale Read").await], &bare).expect("bootstrap v1");
    assert_eq!(bundle::max_tag_version(&bare), 1, "предусловие: репо отстало");

    let got =
        setfork_core::git::repo::ensure_repo_by_id(&pool, list_id).await.expect("ensure").expect("репо есть");
    assert_eq!(got, bare);
    assert_eq!(bundle::max_tag_version(&bare), 2, "ensure выровнял репо с БД");

    let _ = std::fs::remove_dir_all(&bare);
}
