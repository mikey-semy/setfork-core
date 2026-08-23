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
fn досыпка_восстанавливает_тег_а_не_плодит_двойника() {
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
fn досыпка_настоящей_версии_коммит_создаёт() {
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
async fn репо_без_главной_ветки_чинится_возвратом_ссылки() {
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
