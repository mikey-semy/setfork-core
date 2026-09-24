//! Файлы автора С ЗАПИСИ (ADR-0028): `ListWrite.AddVersion` и `Create` с `AuthoredFileSet`.
//!
//! Ради чего: блоки и файлы скилла — одной версией. Раньше файлы приходили только пушем,
//! и между «текст с сайта» и «файлы пушем» жила версия без файлов. Проверяется то, что
//! ломается молча:
//!
//! * нет поля — файлы родителя переносятся (все прежние пути записи), есть — набор
//!   заменяет прежний, есть и пусто — убирает все; перепутать «нет» и «пусто» значит
//!   стереть скрипты любой правкой с сайта;
//! * набор судится ТЕМ ЖЕ правилом, что дерево на push, до записи: отказ ничего не пишет;
//! * рождение с файлами — сразу репозиторий с тегом v1 и файлами: ленивый бутстрап
//!   собирает историю из базы, а файлов в базе нет — они потерялись бы;
//! * эхо `authored_applied` и флаг возможности — по ним фронт отличает «положил» от
//!   «ядро поле не знает и выбросило».
mod support;

use setfork_core::git::update::{AuthoredInput, MainUpdateError, authored_input_violation};
use setfork_core::pb::git_core_server::GitCore;
use setfork_core::pb::{AuthoredFilesRequest, CapabilitiesRequest, RepoRef};
use setfork_core::pb_domain::list_write_server::ListWrite;
use setfork_core::pb_domain::{
    AddVersionRequest, AuthoredFileInput, AuthoredFileSet, CreateListRequest, LocaleText, NewStep,
};
use setfork_core::services::git_core::GitCoreSvc;
use setfork_core::services::list::ListWriteSvc;
use tonic::Request;

fn lt(s: &str) -> Option<LocaleText> {
    Some(LocaleText { v: [("en".to_string(), s.to_string())].into_iter().collect() })
}

fn input(path: &str, content: &[u8]) -> AuthoredInput {
    AuthoredInput { path: path.into(), content: content.to_vec(), executable: false }
}

fn wire(files: &[(&str, &str, bool)]) -> Option<AuthoredFileSet> {
    Some(AuthoredFileSet {
        files: files
            .iter()
            .map(|(p, c, x)| AuthoredFileInput {
                path: (*p).into(),
                content: c.as_bytes().to_vec(),
                executable: *x,
            })
            .collect(),
    })
}

// ── Правило набора — без базы ─────────────────────────────────────────

#[test]
fn input_set_is_judged_by_the_tree_rule() {
    assert_eq!(authored_input_violation(&[input("scripts/run.sh", b"echo")]), None);
    assert_eq!(authored_input_violation(&[]), None, "пустой набор законен — он убирает файлы");
    assert_eq!(
        authored_input_violation(&[input("scripts/sub/x.sh", b"x")]),
        Some(MainUpdateError::ForeignPath("scripts/sub/x.sh".into())),
        "вложенный каталог пропущен"
    );
    assert_eq!(
        authored_input_violation(&[input("secrets.txt", b"x")]),
        Some(MainUpdateError::ForeignPath("secrets.txt".into()))
    );
    assert_eq!(
        authored_input_violation(&[input("references/a.md", b"a"), input("references/a.md", b"b")]),
        Some(MainUpdateError::AuthoredDuplicate("references/a.md".into())),
        "один путь дважды — какой из двух положить, решать некому"
    );
    assert_eq!(
        authored_input_violation(&[input("assets/logo.png", b"\x89PNG\0\x01")]),
        Some(MainUpdateError::AuthoredBinary("assets/logo.png".into()))
    );
    let many: Vec<_> = (0..51).map(|i| input(&format!("references/{i}.md"), b"x")).collect();
    assert_eq!(authored_input_violation(&many), Some(MainUpdateError::AuthoredTooMany(51)));
    // Имя — тоже строго: длина по полю ustar архива, без `.`-имён, управляющих символов,
    // обратного слеша и символов направления текста.
    let long = format!("references/{}.md", "я".repeat(49)); // 98 + 3 = 101 байт имени
    assert!(matches!(authored_input_violation(&[input(&long, b"x")]), Some(MainUpdateError::ForeignPath(_))));
    let fits = format!("references/{}.md", "я".repeat(48)); // 99 байт — проходит
    assert_eq!(authored_input_violation(&[input(&fits, b"x")]), None);
    for bad in [
        "scripts/.git",
        "references/.hidden.md",
        "scripts/run\u{202e}hs.sh",
        "scripts/a\\b.sh",
        "scripts/a\nb.sh",
    ] {
        assert!(
            matches!(authored_input_violation(&[input(bad, b"x")]), Some(MainUpdateError::ForeignPath(_))),
            "имя {bad:?} принято"
        );
    }
    // Исполняемый — только в scripts/: проверку на опасное проходят только они.
    let exec = |p: &str| AuthoredInput { path: p.into(), content: b"rm -rf ~".to_vec(), executable: true };
    assert_eq!(
        authored_input_violation(&[exec("assets/setup.sh")]),
        Some(MainUpdateError::AuthoredExecutable("assets/setup.sh".into()))
    );
    assert_eq!(authored_input_violation(&[exec("scripts/setup.sh")]), None);
    // Байты имени входят в предел: иначе предел обходился бы именем.
    let near = vec![b'x'; 1024 * 1024 - 5];
    assert!(matches!(
        authored_input_violation(&[input("references/a.md", &near)]),
        Some(MainUpdateError::AuthoredTooLarge(_))
    ));
    let big = vec![b'x'; 1024 * 1024 + 1];
    assert_eq!(
        authored_input_violation(&[input("references/big.md", &big)]),
        Some(MainUpdateError::AuthoredTooLarge(1024 * 1024 + 1 + "references/big.md".len() as u64))
    );
}

// ── По проводу сервиса — с базой ──────────────────────────────────────

fn create_req(owner: &str, slug: &str, authored: Option<AuthoredFileSet>) -> CreateListRequest {
    CreateListRequest {
        authored,
        owner_id: owner.into(),
        slug: slug.into(),
        title: lt("Skill"),
        desc: lt("d"),
        tags: vec![],
        ordered: true,
        visibility: String::new(),
        status: String::new(),
        origin: String::new(),
        forked_from_id: String::new(),
        moderation: String::new(),
        note: "v1".into(),
        steps: vec![NewStep { title: lt("Шаг"), ..Default::default() }],
    }
}

fn add_req(list_id: &str, authored: Option<AuthoredFileSet>) -> AddVersionRequest {
    AddVersionRequest {
        authored,
        list_id: list_id.into(),
        note: "next".into(),
        steps: vec![NewStep { title: lt("Шаг"), ..Default::default() }],
        author_id: String::new(),
        meta: None,
        expected_version: None,
    }
}

async fn files_of(git: &GitCoreSvc, handle: &str, slug: &str, version: i32) -> Vec<(String, bool)> {
    let res = git
        .get_authored_files(Request::new(AuthoredFilesRequest {
            repo: Some(RepoRef { owner: handle.into(), slug: slug.into() }),
            version,
        }))
        .await
        .expect("get_authored_files")
        .into_inner();
    assert!(res.found, "версии v{version} нет");
    let mut out: Vec<_> = res.files.into_iter().map(|f| (f.path, f.executable)).collect();
    out.sort();
    out
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn capabilities_say_the_core_takes_author_files() {
    let pool = support::pool_with_schema().await;
    let caps =
        GitCoreSvc { pool }.get_capabilities(Request::new(CapabilitiesRequest {})).await.expect("caps");
    assert!(caps.into_inner().accepts_authored_files);
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn create_with_files_is_born_with_them_in_v1() {
    let _dir = support::own_git_data_dir("authored-create").await;
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "skiller").await;
    let svc = ListWriteSvc { pool: pool.clone() };
    let git = GitCoreSvc { pool: pool.clone() };

    let list = svc
        .create(Request::new(create_req(
            &owner.to_string(),
            "born",
            wire(&[("scripts/run.sh", "echo hi\n", true), ("references/guide.md", "# g\n", false)]),
        )))
        .await
        .expect("create")
        .into_inner();
    assert!(list.authored_applied, "эха нет — фронт счёл бы файлы потерянными");
    assert_eq!(
        files_of(&git, "skiller", "born", 1).await,
        vec![("references/guide.md".into(), false), ("scripts/run.sh".into(), true)]
    );

    // Следующая правка без набора обязана их сохранить: если бы ядро не признало
    // репозиторий своим и пересобрало его из базы (где файлов нет), они исчезли бы здесь.
    svc.add_version(Request::new(add_req(&list.id, None))).await.expect("v2 без набора");
    assert_eq!(
        files_of(&git, "skiller", "born", 2).await,
        vec![("references/guide.md".into(), false), ("scripts/run.sh".into(), true)],
        "файлы рождения потерялись на первой же правке"
    );

    // Без файлов — как раньше: эха нет, файлов нет.
    let plain = svc
        .create(Request::new(create_req(&owner.to_string(), "plain", None)))
        .await
        .expect("create plain")
        .into_inner();
    assert!(!plain.authored_applied);
    assert!(files_of(&git, "skiller", "plain", 1).await.is_empty());
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn add_version_replaces_carries_and_clears() {
    let _dir = support::own_git_data_dir("authored-add").await;
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "writer").await;
    let svc = ListWriteSvc { pool: pool.clone() };
    let git = GitCoreSvc { pool: pool.clone() };
    let list = svc
        .create(Request::new(create_req(
            &owner.to_string(),
            "kit",
            wire(&[("scripts/a.sh", "echo a\n", true)]),
        )))
        .await
        .expect("create")
        .into_inner();

    // Замена: прежнего scripts/a.sh больше нет.
    let v2 = svc
        .add_version(Request::new(add_req(&list.id, wire(&[("references/b.md", "b\n", false)]))))
        .await
        .expect("v2")
        .into_inner();
    assert!(v2.authored_applied);
    assert_eq!(files_of(&git, "writer", "kit", 2).await, vec![("references/b.md".into(), false)]);

    // Правка без поля — перенос: файлы v2 доезжают в v3 как есть.
    let v3 = svc.add_version(Request::new(add_req(&list.id, None))).await.expect("v3").into_inner();
    assert!(!v3.authored_applied, "эхо без набора врало бы, что файлы положены");
    assert_eq!(files_of(&git, "writer", "kit", 3).await, vec![("references/b.md".into(), false)]);

    // Пустой набор — убрать все.
    svc.add_version(Request::new(add_req(&list.id, wire(&[])))).await.expect("v4");
    assert!(files_of(&git, "writer", "kit", 4).await.is_empty(), "пустой набор ничего не убрал");
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn bad_set_is_refused_as_input_and_writes_nothing() {
    let _dir = support::own_git_data_dir("authored-bad").await;
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "refused").await;
    let svc = ListWriteSvc { pool: pool.clone() };
    let list = svc
        .create(Request::new(create_req(&owner.to_string(), "kit", None)))
        .await
        .expect("create")
        .into_inner();

    let err = svc
        .add_version(Request::new(add_req(&list.id, wire(&[("scripts/x/deep.sh", "x", false)]))))
        .await
        .expect_err("вложенный путь принят");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert_eq!(err.metadata().get("sf-reason").and_then(|v| v.to_str().ok()), Some("AUTHORED_INVALID"));
    assert!(err.message().contains("scripts/x/deep.sh"), "отказ не назвал файл: {}", err.message());

    let current: i32 = sqlx::query_scalar("select current_version from templates where id = $1::uuid")
        .bind(&list.id)
        .fetch_one(&pool)
        .await
        .expect("current");
    assert_eq!(current, 1, "отказ записал версию");

    // `.git` — отказ ВВОДА, а не сбой git: libgit2 отвергает такое имя в treebuilder, и
    // без правила имени агент получал бы INTERNAL «git commit failed».
    let dot = svc
        .add_version(Request::new(add_req(&list.id, wire(&[("scripts/.git", "x", false)]))))
        .await
        .expect_err(".git принят");
    assert_eq!(dot.metadata().get("sf-reason").and_then(|v| v.to_str().ok()), Some("AUTHORED_INVALID"));

    let born = svc
        .create(Request::new(create_req(
            &owner.to_string(),
            "never",
            wire(&[("assets/x.bin", "a\0b", false)]),
        )))
        .await
        .expect_err("двоичный файл принят на рождении");
    assert_eq!(born.metadata().get("sf-reason").and_then(|v| v.to_str().ok()), Some("AUTHORED_INVALID"));
    let exists: bool = sqlx::query_scalar("select exists(select 1 from templates where slug = 'never')")
        .fetch_one(&pool)
        .await
        .expect("exists");
    assert!(!exists, "отказ на рождении оставил список");
}
