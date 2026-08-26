//! Выравнивание репо с БД: два состояния, которые ДО линзы 02 §4 не лечились.
//!
//! 1. Репо потеряло ветку `main` при целых тегах. Счётчики сходятся, `tag_on_tip`
//!    на нечитаемом рефе отвечает «всё на месте» — и репо оставалось пустым
//!    навсегда: клон и зеркало отдают пустоту, а содержимое цело в Postgres,
//!    поэтому сайт выглядит исправным.
//! 2. Досыпка при потерянном ТЕГЕ (коммит цел) клала пустой коммит-двойник и
//!    вешала тег на него — канон получал событие, которого не было.
//!
//! Запуск: TEST_DATABASE_URL=... cargo test --test sync_heal_probe -- --include-ignored
mod support;

use setfork_core::git::bundle::{self, SerStep, VersionData};
use setfork_core::git::version::{self, SyncOutcome};
use uuid::Uuid;

fn vdata(version: i32, title: &str) -> VersionData {
    VersionData {
        version,
        note: format!("v{version}"),
        ts: 1_700_000_000 + i64::from(version),
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
        }],
    }
}

fn commits_on_main(bare: &std::path::Path) -> Vec<git2::Oid> {
    let repo = git2::Repository::open_bare(bare).expect("open");
    let mut walk = repo.revwalk().expect("revwalk");
    walk.push_ref("refs/heads/main").expect("push main");
    walk.map(|o| o.expect("oid")).collect()
}

#[test]
fn catch_up_restores_the_tag_instead_of_making_a_twin() {
    let root = std::env::temp_dir().join(format!("setfork-heal-{}", Uuid::new_v4()));
    let bare = root.join("repo.git");
    bundle::bootstrap_bare(&[vdata(1, "ПЕРВЫЙ"), vdata(2, "ВТОРОЙ")], &bare).expect("bootstrap");

    let before = commits_on_main(&bare);
    assert_eq!(before.len(), 2, "исходная история — два коммита");

    // Теряем ТЕГ (коммит цел): ровно то, что делает частичное восстановление тома.
    {
        let repo = git2::Repository::open_bare(&bare).expect("open");
        repo.tag_delete("v2").expect("delete tag");
    }

    let tip = bundle::append_missing_versions(&bare, &[vdata(2, "ВТОРОЙ")]).expect("append");

    let after = commits_on_main(&bare);
    assert_eq!(after, before, "коммит-двойник не создан, вершина не двинулась");
    assert_eq!(tip.as_deref(), Some(before[0].to_string().as_str()), "вернулась прежняя вершина");
    let repo = git2::Repository::open_bare(&bare).expect("open");
    assert_eq!(repo.refname_to_id("refs/tags/v2").expect("тег v2 восстановлен"), before[0]);
    assert_eq!(repo.refname_to_id("refs/tags/v1").expect("тег v1 цел"), before[1]);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn catching_up_a_real_version_does_create_a_commit() {
    // Обратная сторона той же проверки: если версии в истории НЕТ, досыпка обязана
    // её закоммитить. Иначе «не плодить двойника» выродилось бы в «не писать вовсе».
    let root = std::env::temp_dir().join(format!("setfork-heal-{}", Uuid::new_v4()));
    let bare = root.join("repo.git");
    bundle::bootstrap_bare(&[vdata(1, "ПЕРВЫЙ")], &bare).expect("bootstrap");

    bundle::append_missing_versions(&bare, &[vdata(2, "ВТОРОЙ")]).expect("append");

    assert_eq!(commits_on_main(&bare).len(), 2, "недостающая версия дописана коммитом");
    let repo = git2::Repository::open_bare(&bare).expect("open");
    assert_eq!(
        repo.refname_to_id("refs/tags/v2").expect("тег v2"),
        repo.refname_to_id("refs/heads/main").expect("main"),
        "тег новой версии — на вершине"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_repo_without_main_is_healed_by_restoring_the_ref() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "healer").await;
    let list_id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) \
         values ($1, 'healed', '{\"en\":\"Healed\"}', 2) returning id",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .expect("seed template");
    for v in 1..=2 {
        sqlx::query("insert into template_versions (template_id, version, note) values ($1, $2, $3)")
            .bind(list_id)
            .bind(v)
            .bind(format!("v{v}"))
            .execute(&pool)
            .await
            .expect("seed version");
    }

    let root = std::env::temp_dir().join(format!("setfork-heal-{}", Uuid::new_v4()));
    let bare = root.join("repo.git");
    let first = version::sync_repo_with_db(&pool, list_id, &bare).await.expect("первое выравнивание");
    assert_eq!(first, SyncOutcome::Bootstrapped { versions: 2 }, "репо материализовано из истории БД");
    let tip_before = commits_on_main(&bare);

    // МЕТА СПИСКА ПРАВИТСЯ ПОСЛЕ материализации — и это главное в пробе.
    // `load_bundle_data` берёт сегодняшние title/desc/tags для ВСЕХ версий, поэтому
    // пересборка из БД дала бы ДРУГИЕ деревья и другие SHA. Без этой правки проба
    // не различала бы «вернули ссылку» и «переписали историю»: с неизменной метой
    // SHA совпадают, и уничтожение канона выглядело бы зелёным (P1 авто-ревью #100).
    sqlx::query(
        "update templates set title = '{\"en\":\"Переименован после материализации\"}' where id = $1",
    )
    .bind(list_id)
    .execute(&pool)
    .await
    .expect("правка меты");

    // Главная ветка исчезла, теги целы — состояние, которое выравнивание считало синхронным.
    {
        let repo = git2::Repository::open_bare(&bare).expect("open");
        repo.find_reference("refs/heads/main").expect("main есть").delete().expect("удалить main");
        assert!(repo.refname_to_id("refs/heads/main").is_err(), "main действительно удалён");
        assert!(repo.refname_to_id("refs/tags/v2").is_ok(), "теги на месте — счётчики сойдутся");
    }

    let healed = version::sync_repo_with_db(&pool, list_id, &bare).await.expect("выравнивание");
    assert_eq!(
        healed,
        SyncOutcome::MainRestored { version: 2 },
        "ссылка возвращена, а не история пересобрана"
    );
    assert_eq!(commits_on_main(&bare), tip_before, "ТЕ ЖЕ коммиты: канон не переписан");
    {
        let repo = git2::Repository::open_bare(&bare).expect("open");
        assert_eq!(
            repo.refname_to_id("refs/heads/main").expect("main вернулся"),
            repo.refname_to_id("refs/tags/v2").expect("тег v2 цел"),
            "main стоит на коммите своего тега"
        );
    }

    // Идемпотентность: повтор ничего не пересобирает и не плодит.
    let again = version::sync_repo_with_db(&pool, list_id, &bare).await.expect("повтор");
    assert_eq!(again, SyncOutcome::InSync, "вылеченное репо считается синхронным");
    assert_eq!(commits_on_main(&bare), tip_before, "повтор историю не трогает");

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_foreign_tag_above_the_version_does_not_become_a_version() {
    // Второй P1 авто-ревью на #100. Если main пропал, а старший тег `v<N+1>` —
    // ПОСТОРОННИЙ (до починки #59 такое имя мог занять релиз), восстановление «по
    // старшему тегу» подняло бы на него main, и следующая же проверка сочла бы это
    // хвостом прерванной записи: чужой коммит стал бы версией N+1 в базе, причём на
    // обычном ЧТЕНИИ, без единого запроса на запись.
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "foreigntag").await;
    let list_id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) \
         values ($1, 'foreign', '{\"en\":\"Foreign\"}', 1) returning id",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .expect("seed template");
    sqlx::query("insert into template_versions (template_id, version, note) values ($1, 1, 'v1')")
        .bind(list_id)
        .execute(&pool)
        .await
        .expect("seed v1");

    let root = std::env::temp_dir().join(format!("setfork-foreign-{}", Uuid::new_v4()));
    let bare = root.join("repo.git");
    assert_eq!(
        version::sync_repo_with_db(&pool, list_id, &bare).await.expect("материализация"),
        SyncOutcome::Bootstrapped { versions: 1 }
    );
    let v1 = commits_on_main(&bare)[0];

    // ПОСТОРОННИЙ коммит с именем версии: дерево валидное (иначе его отвергнет сама
    // точка обновления main), но версией он не является — это чужой релиз.
    {
        let repo = git2::Repository::open_bare(&bare).expect("open");
        let base = repo.find_commit(v1).expect("v1");
        let sig = git2::Signature::new("Кто-то", "someone@example.com", &git2::Time::new(1_800_000_000, 0))
            .unwrap();
        let oid = repo
            .commit(None, &sig, &sig, "release 1.0", &base.tree().expect("tree"), &[&base])
            .expect("посторонний коммит");
        let obj = repo.find_object(oid, Some(git2::ObjectType::Commit)).expect("obj");
        repo.tag_lightweight("v2", &obj, true).expect("чужой тег v2");
        repo.find_reference("refs/heads/main").expect("main").delete().expect("удалить main");
    }

    let outcome = version::sync_repo_with_db(&pool, list_id, &bare).await.expect("выравнивание");

    let cur: i32 = sqlx::query_scalar("select current_version from templates where id = $1")
        .bind(list_id)
        .fetch_one(&pool)
        .await
        .expect("current");
    let rows: i64 = sqlx::query_scalar("select count(*) from template_versions where template_id = $1")
        .bind(list_id)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(cur, 1, "чужой коммит НЕ стал версией");
    assert_eq!(rows, 1, "строк версий по-прежнему одна");
    assert_eq!(
        outcome,
        SyncOutcome::Conflict { have: 2, current: 1 },
        "расхождение названо конфликтом, а не вылечено"
    );
    {
        let repo = git2::Repository::open_bare(&bare).expect("open");
        assert_eq!(
            repo.refname_to_id("refs/heads/main").expect("main вернулся"),
            v1,
            "main стоит на СВОЁМ v1"
        );
    }

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn an_unreadable_repo_is_not_healed_by_rebuilding() {
    // Каталог есть, а репозиторий не открывается: обрубленный HEAD после сбоя,
    // частичное восстановление тома, права. Раньше это состояние было НЕОТЛИЧИМО от
    // «ветки и тегов нет» — и лечение приняло бы битое репо за пустое, переписав
    // историю из БД поверх принятых пушей и веток предложений (находка своего
    // прохода ревью по #100).
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "brokenrepo").await;
    let list_id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) \
         values ($1, 'broken', '{\"en\":\"Broken\"}', 1) returning id",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .expect("seed template");
    sqlx::query("insert into template_versions (template_id, version, note) values ($1, 1, 'v1')")
        .bind(list_id)
        .execute(&pool)
        .await
        .expect("seed v1");

    let root = std::env::temp_dir().join(format!("setfork-broken-{}", Uuid::new_v4()));
    let bare = root.join("repo.git");
    std::fs::create_dir_all(bare.join("objects")).expect("каталог");
    std::fs::write(bare.join("HEAD"), "мусор вместо ссылки\n").expect("битый HEAD");

    let res = version::sync_repo_with_db(&pool, list_id, &bare).await;

    assert!(res.is_err(), "нечитаемое репо обязано ОТКАЗАТЬ, а не пересобраться молча");
    assert!(!bare.join("refs/heads/main").exists(), "лечение не трогало каталог");

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn a_gap_in_tags_is_not_healed_by_rebuilding() {
    // Теги есть, но нужного нет: `v1, v2` при `current_version = 3` и пропавшем main.
    // Пересборка переписала бы целую историю и форсом сдвинула бы теги — под видом
    // лечения. Такое обязано дойти до человека, а не «вылечиться».
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "taggap").await;
    let list_id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) \
         values ($1, 'gap', '{\"en\":\"Gap\"}', 1) returning id",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .expect("seed template");
    sqlx::query("insert into template_versions (template_id, version, note) values ($1, 1, 'v1')")
        .bind(list_id)
        .execute(&pool)
        .await
        .expect("seed v1");

    let root = std::env::temp_dir().join(format!("setfork-gap-{}", Uuid::new_v4()));
    let bare = root.join("repo.git");
    version::sync_repo_with_db(&pool, list_id, &bare).await.expect("материализация");
    let history = commits_on_main(&bare);

    // База ушла вперёд, а в репо появился ПОСТОРОННИЙ старший тег: нужного `v3` нет.
    sqlx::query("update templates set current_version = 3 where id = $1")
        .bind(list_id)
        .execute(&pool)
        .await
        .expect("сдвиг current_version");
    {
        let repo = git2::Repository::open_bare(&bare).expect("open");
        let obj = repo.find_object(history[0], Some(git2::ObjectType::Commit)).expect("obj");
        repo.tag_lightweight("v5", &obj, true).expect("тег v5");
        repo.find_reference("refs/heads/main").expect("main").delete().expect("удалить main");
    }

    let outcome = version::sync_repo_with_db(&pool, list_id, &bare).await.expect("выравнивание");

    assert_eq!(outcome, SyncOutcome::Conflict { have: 5, current: 3 }, "дыра названа конфликтом");
    let repo = git2::Repository::open_bare(&bare).expect("open");
    assert_eq!(repo.refname_to_id("refs/tags/v1").expect("тег v1"), history[0], "теги не сдвинуты");

    std::fs::remove_dir_all(&root).ok();
}

/// ДОСЫПКА (`Appended`) — единственная ветка выравнивания, у которой до 26.08 не
/// было ни одного прогона (линза 02 §4 требует воспроизвести КАЖДУЮ).
///
/// Состояние: в БД версий больше, чем тегов в репо. Так выглядит наследие ленивой
/// досыпки — репо материализовали на v1, а список тем временем уехал вперёд.
///
/// Проверяем не только исход, но и ГЛАВНОЕ свойство: досыпка ДОПИСЫВАЕТ поверх
/// существующей истории, а не пересобирает её. Пересборка сменила бы SHA уже
/// запушенных коммитов — это ADR-0014, исключение 2, и ровно тот дефект, который
/// линза 02 ловила у лечения пропавшей ветки.
#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn catch_up_appends_on_top_instead_of_rebuilding() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "appender").await;
    let list_id: Uuid = sqlx::query_scalar(
        "insert into templates (owner_id, slug, title, current_version) \
         values ($1, 'appended', '{\"en\":\"Appended\"}', 1) returning id",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .expect("seed template");
    sqlx::query("insert into template_versions (template_id, version, note) values ($1, 1, 'v1')")
        .bind(list_id)
        .execute(&pool)
        .await
        .expect("seed v1");

    let root = std::env::temp_dir().join(format!("setfork-append-{}", Uuid::new_v4()));
    let bare = root.join("repo.git");
    let first = version::sync_repo_with_db(&pool, list_id, &bare).await.expect("материализация");
    assert_eq!(first, SyncOutcome::Bootstrapped { versions: 1 }, "репо родилось на одной версии");
    let sha_v1 = commits_on_main(&bare);
    assert_eq!(sha_v1.len(), 1, "одна версия — один коммит");

    // Список уехал вперёд БЕЗ участия репо: так и выглядит наследие ленивой досыпки.
    for v in 2..=3 {
        sqlx::query("insert into template_versions (template_id, version, note) values ($1, $2, $3)")
            .bind(list_id)
            .bind(v)
            .bind(format!("v{v}"))
            .execute(&pool)
            .await
            .expect("seed version");
    }
    sqlx::query("update templates set current_version = 3 where id = $1")
        .bind(list_id)
        .execute(&pool)
        .await
        .expect("сдвиг current_version");

    let outcome = version::sync_repo_with_db(&pool, list_id, &bare).await.expect("досыпка");
    assert_eq!(
        outcome,
        SyncOutcome::Appended { from: 2, to: 3 },
        "дописаны v2..v3 — `from` это ПЕРВАЯ дописанная версия, а не последняя имевшаяся"
    );

    // 1. История ВЫРОСЛА, а не переписана: первый коммит тот же самый.
    let sha_v3 = commits_on_main(&bare);
    assert_eq!(sha_v3.len(), 3, "три версии — три коммита");
    assert_eq!(
        sha_v3.last(),
        sha_v1.last(),
        "корневой коммит обязан остаться ТЕМ ЖЕ: пересборка сменила бы SHA запушенного \
         (ADR-0014, исключение 2)"
    );

    // 2. Теги проставлены на свои коммиты, а не свалены на вершину.
    let repo = git2::Repository::open_bare(&bare).expect("open");
    for (v, sha) in [(1, sha_v3[2]), (2, sha_v3[1]), (3, sha_v3[0])] {
        let tag = repo.refname_to_id(&format!("refs/tags/v{v}")).unwrap_or_else(|e| panic!("тег v{v}: {e}"));
        assert_eq!(tag, sha, "тег v{v} обязан указывать на СВОЙ коммит, а не на вершину");
    }

    // 3. Повтор идемпотентен — рунбук обещает «повторный запуск безопасен».
    let again = version::sync_repo_with_db(&pool, list_id, &bare).await.expect("повтор");
    assert_eq!(again, SyncOutcome::InSync, "после досыпки нечего делать");
    assert_eq!(commits_on_main(&bare), sha_v3, "повтор ничего не добавил");

    let _ = std::fs::remove_dir_all(&root);
}
