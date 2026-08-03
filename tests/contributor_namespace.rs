//! Ф5: посторонний может только ПРЕДЪЯВИТЬ правку.
//!
//! Правило исполняет хук, а не приложение, и проверять его надо настоящим пушем:
//! отвергнуть приём задним числом нельзя, а «разрешено ли» решается ровно в тот
//! момент, когда git читает список рефов.
//!
//! Разделение обязанностей (ADR-0011 §2): КТО может пушить, решает фронт и
//! присылает готовую роль; ядро исполняет механическое следствие роли. Здесь
//! проверяется только следствие.
//!
//! Без БД: чистый git. Обычный `cargo test`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use setfork_core::git::bundle;

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tmp(prefix: &str) -> Tmp {
    let p = std::env::temp_dir().join(format!("setfork-{prefix}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).expect("create tmp");
    Tmp(p)
}

/// Пуш с явными ником и ролью — ровно то, что ядро выставляет receive-pack.
fn push_as(work: &Path, actor: &str, role: &str, refspec: &str) -> Output {
    Command::new("git")
        .current_dir(work)
        .env("SETFORK_ACTOR", actor)
        .env("SETFORK_ROLE", role)
        .args(["push", "origin", refspec])
        .output()
        .expect("git запустился")
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("SETFORK_ROLE", "owner") // подготовка идёт от владельца
        .args(args)
        .output()
        .expect("git запустился");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Bare с установленным хуком + рабочая копия с одной версией в main.
fn repo_pair(root: &Path) -> (PathBuf, PathBuf) {
    let bare = root.join("bare.git");
    git_ok(root, &["init", "-q", "--bare", "--initial-branch=main", bare.to_str().unwrap()]);
    bundle::install_hook(&bare).expect("hooks");

    let work = root.join("work");
    git_ok(root, &["init", "-q", "--initial-branch=main", work.to_str().unwrap()]);
    git_ok(&work, &["config", "user.email", "t@setfork.com"]);
    git_ok(&work, &["config", "user.name", "T"]);
    std::fs::write(work.join("list.json"), br#"{"title":"L","steps":[]}"#).expect("list.json");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-qm", "v1"]);
    git_ok(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git_ok(&work, &["push", "-q", "origin", "main"]);
    (bare, work)
}

#[test]
fn посторонний_не_трогает_main() {
    let root = tmp("f5-main");
    let (_bare, work) = repo_pair(&root.0);
    std::fs::write(work.join("list.json"), br#"{"title":"L2","steps":[]}"#).expect("edit");
    git_ok(&work, &["commit", "-aqm", "v2"]);

    let out = push_as(&work, "outsider-id", "contributor", "main");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "посторонний не пишет в main: {err}");
    // Отказ обязан назвать правило и путь наружу, иначе человек решит, что
    // доступа нет вовсе, и уйдёт.
    assert!(err.contains("not yours"), "отказ называет причину: {err}");
    assert!(err.contains("refs/for/main"), "отказ показывает, куда можно: {err}");
}

#[test]
fn посторонний_не_заводит_веток_вообще() {
    let root = tmp("f5-alien");
    let (_bare, work) = repo_pair(&root.0);

    // Ни чужую, ни «свою» — именованных веток у постороннего нет совсем. Имя
    // ветки для его правки придумывает сервер, и человек его не набирает.
    for refspec in ["main:refs/heads/u/mike/idea", "main:refs/heads/u/outsider/idea", "main:refs/heads/idea"]
    {
        let out = push_as(&work, "outsider-id", "contributor", refspec);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{refspec} должен быть закрыт: {err}");
        assert!(err.contains("not yours"), "{refspec}: {err}");
        assert!(err.contains("refs/for/main"), "{refspec}: отказ показывает, что делать: {err}");
    }
}

#[test]
fn посторонний_предъявляет_правку() {
    let root = tmp("f5-own");
    let (_bare, work) = repo_pair(&root.0);

    let magic = push_as(&work, "outsider-id", "contributor", "main:refs/for/main");
    let magic_err = String::from_utf8_lossy(&magic.stderr);
    assert!(magic.status.success(), "предложение принимается: {magic_err}");
    assert!(magic_err.contains("change accepted"), "человек видит, что дальше: {magic_err}");
}

#[test]
fn владелец_и_соавтор_пишут_куда_угодно() {
    let root = tmp("f5-owner");
    let (_bare, work) = repo_pair(&root.0);
    std::fs::write(work.join("list.json"), br#"{"title":"L3","steps":[]}"#).expect("edit");
    git_ok(&work, &["commit", "-aqm", "v3"]);

    let owner = push_as(&work, "mike", "owner", "main");
    assert!(owner.status.success(), "владелец пишет в main: {}", String::from_utf8_lossy(&owner.stderr));

    let collab = push_as(&work, "kate", "collaborator", "main:refs/heads/shared-idea");
    assert!(
        collab.status.success(),
        "соавтор ведёт общие ветки: {}",
        String::from_utf8_lossy(&collab.stderr)
    );
}

/// Окно выкатки: ядро приезжает раньше или позже фронта, и какое-то время роли
/// не приходит вовсе. Строгое умолчание («нет роли = посторонний») я поставил
/// первым и ошибся — оно объявляло посторонним ВСЕХ, включая владельца, и пуш в
/// main переставал работать у всех сразу (авто-ревью core#80).
///
/// Дыры лёгкое умолчание не открывает: пустая роль приходит только от нашего
/// фронта, канал закрыт токеном, а фронт без ролей и посторонних не впускает.
/// Ядро при этом пишет предупреждение в лог, чтобы состояние не было тихим.
#[test]
fn без_роли_работает_как_до_ф5() {
    let root = tmp("f5-norole");
    let (_bare, work) = repo_pair(&root.0);
    std::fs::write(work.join("list.json"), br#"{"title":"L4","steps":[]}"#).expect("edit");
    git_ok(&work, &["commit", "-aqm", "v4"]);

    let out = Command::new("git")
        .current_dir(&work)
        .env("SETFORK_ACTOR", "mike")
        .env_remove("SETFORK_ROLE")
        .args(["push", "origin", "main"])
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "старый фронт не должен ломать пуш владельца: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Посторонний без ника не может ничего: пространство определяется ником, и без
/// него правило неисполнимо. Молчать в этом случае нельзя — отказ объясняет.
#[test]
fn посторонний_без_ника_получает_объяснение() {
    let root = tmp("f5-noactor");
    let (_bare, work) = repo_pair(&root.0);

    let out = Command::new("git")
        .current_dir(&work)
        .env_remove("SETFORK_ACTOR")
        .env("SETFORK_ROLE", "contributor")
        .args(["push", "origin", "main:refs/heads/u/x/idea"])
        .output()
        .expect("git");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "без ника пространство неопределимо: {err}");
    assert!(err.contains("cannot tell who is pushing"), "отказ объясняет: {err}");
}

/// Незнакомое значение роли обязано ОГРАНИЧИВАТЬ, а не открывать.
///
/// Поле приходит строкой и протоколом не проверяется: опечатка, новая роль во
/// фронте, мусор — всё это «не contributor». Пока условие звучало как
/// «ограничиваем ровно contributor», любое такое значение давало право писать в
/// main (авто-ревью core#80). Судим по белому списку.
#[test]
fn незнакомая_роль_ограничивается() {
    let root = tmp("f5-unknown-role");
    let (_bare, work) = repo_pair(&root.0);
    std::fs::write(work.join("list.json"), br#"{"title":"L-unknown","steps":[]}"#).expect("edit");
    git_ok(&work, &["commit", "-aqm", "v2"]);

    for role in ["contributer", "guest", "Owner", "мусор"] {
        let out = push_as(&work, "someone-id", role, "main");
        assert!(
            !out.status.success(),
            "роль {role:?} не в белом списке — писать в main нельзя: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Правило обязано пережить чужой хук на диске.
///
/// Ядро отвечает `enforces_push_roles: true` — то есть ОБЕЩАЕТ, что посторонний
/// в main не запишет. Хук при этом файл на общем диске, и рядом может работать
/// ядро другой версии (окно выкатки): его хук правила ролей не знает. Поэтому
/// хук переустанавливается вплотную к приёму пака, под тем же локом.
///
/// Здесь проверяется само свойство установки: подменённый хук восстанавливается
/// и снова отвергает пуш постороннего в main.
#[test]
fn подменённый_хук_восстанавливается() {
    let root = tmp("f5-hook-replace");
    let (bare, work) = repo_pair(&root.0);
    std::fs::write(work.join("list.json"), br#"{"title":"L-hook","steps":[]}"#).expect("edit");
    git_ok(&work, &["commit", "-aqm", "v2"]);

    // Чужое ядро переписало хук на «пропускать всё».
    let hook = bare.join("hooks").join("pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh
exit 0
",
    )
    .expect("подмена хука");
    let out = push_as(&work, "outsider-id", "contributor", "main");
    assert!(out.status.success(), "подменённый хук пропускает — иначе тест ничего не проверяет");

    // Ядро ставит свой обратно (то же, что делает receive_pack под локом).
    bundle::install_hook(&bare).expect("переустановка");
    std::fs::write(work.join("list.json"), br#"{"title":"L-hook-2","steps":[]}"#).expect("edit");
    git_ok(&work, &["commit", "-aqm", "v3"]);
    let out = push_as(&work, "outsider-id", "contributor", "main");
    assert!(
        !out.status.success(),
        "правило вернулось — посторонний в main не пишет: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
