//! АВТОРСКИЕ ФАЙЛЫ СКИЛЛА в дереве списка: `scripts/<файл>` и `references/<файл>`.
//!
//! Решение владельца 23.09.2026 (исследование `2026-09-23-agent-skills`, §4): текст
//! скилла живёт в git-дереве списка — каждый байт скрипта покрыт SHA версии, — а
//! бинарь уходит в S3 по хешу. Отсюда три вещи, и каждая проверена здесь:
//!
//! 1. ОБЕ двери записи пускают эти файлы и держат лимиты одинаково: `pre-receive`
//!    ловит настоящий `git push`, `update_main` — программную запись через git2.
//!    Правило одно, реализаций две, и разъезжаться им нельзя.
//! 2. Правка с сайта файлы НЕ стирает. Веб-версия собирает дерево с нуля, и без
//!    переноса из родителя скрипты, пришедшие пушем, пропадали бы в следующей же
//!    правке — тихо, с законной на вид версией.
//! 3. Что по пути не видно — ссылки, подмодули, бинари, объём — отвергается и
//!    называется в отказе.
//!
//! Тесты не трогают БД — только git, поэтому идут в обычном `cargo test`.
use std::path::{Path, PathBuf};
use std::process::Command;

use setfork_core::git::bundle::{self, SerStep, VersionData, install_hook};
use setfork_core::git::serialize::{
    AUTHORED_MAX_BYTES, AUTHORED_MAX_FILES, authored_path, tree_path_allowed,
};
use setfork_core::git::update::{MainUpdateError, update_main};

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

/// Git от лица владельца (Ф5: без роли хук считает пушащего посторонним).
fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(dir)
        .env("SETFORK_ROLE", "owner")
        .args(args)
        .output()
        .expect("git запустился")
}
fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = git(dir, args);
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Bare с правилами + рабочая копия с одной версией в main.
fn repo_pair(root: &Path) -> (PathBuf, PathBuf) {
    let bare = root.join("bare.git");
    git_ok(root, &["init", "-q", "--bare", "--initial-branch=main", bare.to_str().unwrap()]);
    install_hook(&bare).expect("хук установлен");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("mkdir work");
    git_ok(root, &["init", "-q", "--initial-branch=main", work.to_str().unwrap()]);
    git_ok(&work, &["config", "user.email", "t@example.com"]);
    git_ok(&work, &["config", "user.name", "Тест"]);
    git_ok(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    std::fs::write(work.join("list.json"), b"{\"title\":\"t\",\"steps\":[]}").expect("list.json");
    std::fs::write(work.join("README.md"), b"# t\n").expect("README");
    git_ok(&work, &["add", "-A"]);
    git_ok(&work, &["commit", "-q", "-m", "v1"]);
    git_ok(&work, &["push", "-q", "origin", "main"]);
    (bare, work)
}

/// Закоммитить и запушить; вернуть (успех, stderr).
fn commit_push(work: &Path, msg: &str) -> (bool, String) {
    git_ok(work, &["add", "-A"]);
    git_ok(work, &["commit", "-q", "-m", msg]);
    let out = git(work, &["push", "origin", "main"]);
    (out.status.success(), String::from_utf8_lossy(&out.stderr).to_string())
}

fn write(work: &Path, path: &str, bytes: &[u8]) {
    let p = work.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).expect("mkdir");
    std::fs::write(p, bytes).expect("write");
}

/// Исполняемый файл — битом НА ДИСКЕ: `git add -A` берёт режим оттуда, и
/// `update-index --chmod=+x` перед ним тихо затирался.
fn write_exec(work: &Path, path: &str, bytes: &[u8]) {
    write(work, path, bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(work.join(path), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

// ── Правило путей ────────────────────────────────────────────────────────────

#[test]
fn authored_paths_are_one_level_under_three_dirs() {
    for ok in ["scripts/run.sh", "references/context.md", "assets/blocks.example.json", "scripts/шаг.sh"] {
        assert!(tree_path_allowed(ok) && authored_path(ok), "{ok} — законный авторский файл");
    }
    for bad in [
        "scripts",
        "scripts/",
        "scripts/lib/x.sh",
        "references/a/b.md",
        "assets/img/logo.svg",
        "assets",
        "scripts/..",
        "scripts/.",
        "images/x.png",
        "scriptsx/run.sh",
    ] {
        assert!(!tree_path_allowed(bad), "{bad} проходить не должен");
    }
}

// ── Дверь git push (pre-receive) ─────────────────────────────────────────────

#[test]
fn the_hook_accepts_scripts_and_references() {
    let root = tmp("skill-ok");
    let (_bare, work) = repo_pair(&root.0);
    write_exec(&work, "scripts/run.sh", b"#!/bin/sh\necho hi\n");
    write(&work, "references/context.md", "# Контекст\n".as_bytes());
    write(&work, "assets/template.md", "# Шаблон\n".as_bytes());
    let (ok, err) = commit_push(&work, "скилл");
    assert!(ok, "законные файлы скилла обязаны проходить: {err}");
}

#[test]
fn the_hook_rejects_a_nested_dir_and_names_it() {
    let root = tmp("skill-nested");
    let (_bare, work) = repo_pair(&root.0);
    write(&work, "scripts/lib/x.sh", b"echo\n");
    let (ok, err) = commit_push(&work, "вложенность");
    assert!(!ok, "вложенный каталог обязан быть отвергнут: {err}");
    assert!(err.contains("scripts/lib/x.sh"), "отказ обязан назвать путь: {err}");
}

#[cfg(unix)]
#[test]
fn the_hook_rejects_a_symlink() {
    let root = tmp("skill-link");
    let (_bare, work) = repo_pair(&root.0);
    std::fs::create_dir_all(work.join("scripts")).expect("mkdir");
    std::os::unix::fs::symlink("/etc/passwd", work.join("scripts/passwd")).expect("symlink");
    let (ok, err) = commit_push(&work, "ссылка");
    assert!(!ok, "ссылка ведёт за пределы папки скилла у распаковавшего: {err}");
    assert!(err.contains("scripts/passwd"), "{err}");
}

#[test]
fn the_hook_rejects_a_binary_and_names_it() {
    let root = tmp("skill-bin");
    let (_bare, work) = repo_pair(&root.0);
    write(&work, "references/logo.png", &[0x89, b'P', b'N', b'G', 0, 0, 0, 13]);
    let (ok, err) = commit_push(&work, "бинарь");
    assert!(!ok, "бинарю место в S3, а не в дереве: {err}");
    assert!(err.contains("references/logo.png"), "{err}");
}

#[test]
fn the_hook_counts_files_across_both_dirs() {
    let root = tmp("skill-many");
    let (_bare, work) = repo_pair(&root.0);
    // Ровно предел — законно; следующий файл — отказ. Половина в каждом каталоге:
    // лимит на дерево, а не на каталог.
    for i in 0..AUTHORED_MAX_FILES {
        let dir = ["scripts", "references", "assets"][i % 3];
        write(&work, &format!("{dir}/f{i}.txt"), format!("{i}\n").as_bytes());
    }
    let (ok, err) = commit_push(&work, "ровно предел");
    assert!(ok, "{AUTHORED_MAX_FILES} файлов — ещё в пределе: {err}");
    write(&work, "scripts/one-more.txt", b"x\n");
    let (ok, err) = commit_push(&work, "сверх предела");
    assert!(!ok, "файл сверх предела обязан быть отвергнут: {err}");
    assert!(err.contains(&(AUTHORED_MAX_FILES + 1).to_string()), "отказ называет число: {err}");
}

#[test]
fn the_hook_sums_bytes_across_both_dirs() {
    let root = tmp("skill-big");
    let (_bare, work) = repo_pair(&root.0);
    let half = (AUTHORED_MAX_BYTES / 2 + 1) as usize;
    write(&work, "scripts/a.txt", &vec![b'a'; half]);
    write(&work, "assets/b.txt", &vec![b'b'; half]);
    let (ok, err) = commit_push(&work, "два половинных файла");
    assert!(!ok, "сумма больше предела обязана быть отвергнута: {err}");
    assert!(err.contains(&AUTHORED_MAX_BYTES.to_string()), "отказ называет предел: {err}");
}

// ── Дверь git2 (update_main) — тот же набор правил ──────────────────────────

fn commit_tree(
    repo: &git2::Repository,
    build: impl Fn(&mut git2::TreeBuilder<'_>, &git2::Repository),
) -> git2::Oid {
    let sig = git2::Signature::new("Тест", "t@example.com", &git2::Time::new(1_700_000_000, 0)).expect("sig");
    let mut b = repo.treebuilder(None).expect("tb");
    b.insert("list.json", repo.blob(b"{}").expect("blob"), 0o100644).expect("list.json");
    build(&mut b, repo);
    let tree = repo.find_tree(b.write().expect("write")).expect("tree");
    repo.commit(None, &sig, &sig, "c", &tree, &[]).expect("commit")
}

fn dir_of(repo: &git2::Repository, files: &[(&str, &[u8], i32)]) -> git2::Oid {
    let mut d = repo.treebuilder(None).expect("tb");
    for (name, bytes, mode) in files {
        d.insert(name, repo.blob(bytes).expect("blob"), *mode).expect("insert");
    }
    d.write().expect("write")
}

#[test]
fn update_main_holds_the_same_rules() {
    let root = tmp("skill-git2");
    let repo = git2::Repository::init_bare(root.0.join("g.git")).expect("init");

    let ok = commit_tree(&repo, |b, r| {
        b.insert("scripts", dir_of(r, &[("run.sh", b"echo\n", 0o100755)]), 0o040000).expect("scripts");
    });
    assert!(update_main(&repo, ok, None, "t").is_ok(), "законное дерево обязано пройти");

    let bin = commit_tree(&repo, |b, r| {
        b.insert("references", dir_of(r, &[("x.png", b"\x89PNG\0", 0o100644)]), 0o040000).expect("r");
    });
    assert_eq!(
        update_main(&repo, bin, Some(ok), "t"),
        Err(MainUpdateError::AuthoredBinary("references/x.png".into()))
    );

    let link = commit_tree(&repo, |b, r| {
        b.insert("scripts", dir_of(r, &[("p", b"/etc/passwd", 0o120000)]), 0o040000).expect("s");
    });
    assert_eq!(
        update_main(&repo, link, Some(ok), "t"),
        Err(MainUpdateError::AuthoredNotFile("scripts/p".into()))
    );

    let many = commit_tree(&repo, |b, r| {
        let names: Vec<String> = (0..=AUTHORED_MAX_FILES).map(|i| format!("f{i}")).collect();
        let files: Vec<(&str, &[u8], i32)> =
            names.iter().map(|n| (n.as_str(), n.as_bytes(), 0o100644)).collect();
        b.insert("scripts", dir_of(r, &files), 0o040000).expect("s");
    });
    assert_eq!(
        update_main(&repo, many, Some(ok), "t"),
        Err(MainUpdateError::AuthoredTooMany(AUTHORED_MAX_FILES + 1))
    );

    let big = commit_tree(&repo, |b, r| {
        let half = vec![b'a'; (AUTHORED_MAX_BYTES / 2 + 1) as usize];
        b.insert("scripts", dir_of(r, &[("a", &half, 0o100644)]), 0o040000).expect("s");
        b.insert("references", dir_of(r, &[("b", &half, 0o100644)]), 0o040000).expect("r");
    });
    assert!(matches!(update_main(&repo, big, Some(ok), "t"), Err(MainUpdateError::AuthoredTooLarge(_))));
}

// ── Правка с сайта не стирает файлы, пришедшие пушем ────────────────────────

fn step(n: i32, title: &str) -> SerStep {
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
fn ver(version: i32, steps: Vec<SerStep>) -> VersionData {
    VersionData {
        version,
        note: "edit".into(),
        ts: 1_700_000_000 + version as i64,
        title: "Runbook".into(),
        desc: String::new(),
        tags: vec![],
        ordered: true,
        kind: None,
        steps,
    }
}

#[test]
fn a_web_version_keeps_pushed_scripts_byte_for_byte() {
    let root = tmp("skill-carry");
    let bare = bundle::materialize_repo(&[ver(1, vec![step(1, "Проверить сеть")])]).expect("materialize");
    let _bare_guard = Tmp(bare.clone());
    install_hook(&bare).expect("хук");
    let work = root.0.join("work");
    git_ok(&root.0, &["clone", "-q", bare.to_str().unwrap(), work.to_str().unwrap()]);
    git_ok(&work, &["config", "user.email", "t@example.com"]);
    git_ok(&work, &["config", "user.name", "Тест"]);

    write_exec(&work, "scripts/run.sh", b"#!/bin/sh\necho hi\n");
    write(&work, "references/context.md", "# Почему так\n".as_bytes());
    write(&work, "assets/template.md", "# Шаблон\n".as_bytes());
    let (ok, err) = commit_push(&work, "скрипт пушем");
    assert!(ok, "{err}");
    let pushed_scripts = git_ok(&bare, &["rev-parse", "main:scripts"]);
    let pushed_refs = git_ok(&bare, &["rev-parse", "main:references"]);
    let pushed_assets = git_ok(&bare, &["rev-parse", "main:assets"]);

    // Правка с сайта: новая версия собирается ядром заново.
    bundle::append_versions(&bare, &[ver(2, vec![step(1, "Проверить сеть"), step(2, "Проверить диск")])])
        .expect("веб-версия");

    assert_eq!(
        git_ok(&bare, &["rev-parse", "main:scripts"]),
        pushed_scripts,
        "правка с сайта стёрла или изменила скрипты, пришедшие пушем"
    );
    assert_eq!(git_ok(&bare, &["rev-parse", "main:references"]), pushed_refs);
    assert_eq!(git_ok(&bare, &["rev-parse", "main:assets"]), pushed_assets, "правка с сайта стёрла assets/");
    let mode = git_ok(&bare, &["ls-tree", "main", "scripts/run.sh"]);
    assert!(mode.starts_with("100755"), "скрипт потерял право на запуск: {mode}");
    assert!(git_ok(&bare, &["show", "main:list.json"]).contains("Проверить диск"), "версия легла");
}

// Обратная сторона: у списка без авторских файлов веб-версия не заводит пустых
// каталогов, и дерево остаётся прежним до байта — SHA старых списков не сдвигаются.
#[test]
fn a_list_without_scripts_keeps_its_tree_shape() {
    let bare =
        bundle::materialize_repo(&[ver(1, vec![step(1, "a")]), ver(2, vec![step(1, "b")])]).expect("m");
    let _g = Tmp(bare.clone());
    let names = git_ok(&bare, &["ls-tree", "--name-only", "main"]);
    assert_eq!(names.lines().collect::<Vec<_>>(), vec![".gitattributes", "README.md", "list.json"]);
}

// Выравнивание после потери тега узнаёт версию по дереву. С перенесёнными
// `scripts/` дерево версии — это сгенерированное ПЛЮС авторское из родителя, и
// сверка обязана собирать его так же, иначе своя же версия не узнаётся, и рядом
// ложится пустой коммит-двойник (тот самый дефект линзы 02 §4, только через скрипты).
#[test]
fn realignment_recognises_a_version_that_carries_scripts() {
    let root = tmp("skill-realign");
    let bare = bundle::materialize_repo(&[ver(1, vec![step(1, "a")])]).expect("materialize");
    let _g = Tmp(bare.clone());
    install_hook(&bare).expect("хук");
    let work = root.0.join("work");
    git_ok(&root.0, &["clone", "-q", bare.to_str().unwrap(), work.to_str().unwrap()]);
    git_ok(&work, &["config", "user.email", "t@example.com"]);
    git_ok(&work, &["config", "user.name", "Тест"]);
    write_exec(&work, "scripts/run.sh", b"echo hi\n");
    let (ok, err) = commit_push(&work, "скрипт");
    assert!(ok, "{err}");

    let v2 = ver(2, vec![step(1, "b")]);
    bundle::append_versions(&bare, std::slice::from_ref(&v2)).expect("v2");
    let tip = git_ok(&bare, &["rev-parse", "main"]);
    git_ok(&bare, &["tag", "-d", "v2"]);

    bundle::append_missing_versions(&bare, &[v2]).expect("выравнивание");
    assert_eq!(git_ok(&bare, &["rev-parse", "main"]), tip, "выравнивание положило двойника вместо тега");
    assert_eq!(git_ok(&bare, &["rev-parse", "v2^{commit}"]), tip, "тег вернулся не на свой коммит");
}

// Бинарю место в S3 и в `assets/` тоже: каталог «ассетов» соблазняет положить туда
// картинку, и правило обязано сработать ровно так же, как в двух других.
#[test]
fn the_hook_rejects_a_binary_in_assets() {
    let root = tmp("skill-asset-bin");
    let (_bare, work) = repo_pair(&root.0);
    write(&work, "assets/logo.png", &[0x89, b'P', b'N', b'G', 0, 0, 0, 13]);
    let (ok, err) = commit_push(&work, "картинка");
    assert!(!ok, "бинарь в assets/ обязан быть отвергнут: {err}");
    assert!(err.contains("assets/logo.png"), "{err}");
}

// Версия, чей коммит САМ добавил авторские файлы (пуш, совпавший с каноном
// побайтно), и тег которой потерялся. Сверка по авторским каталогам РОДИТЕЛЯ её не
// узнавала, и выравнивание клало рядом пустого двойника. Авторские каталоги не
// генерируются — сравнивать надо сгенерированную часть, а авторскую брать у самого
// коммита.
#[test]
fn realignment_recognises_a_version_whose_own_commit_added_scripts() {
    let root = tmp("skill-realign-own");
    let bare = bundle::materialize_repo(&[ver(1, vec![step(1, "a")])]).expect("materialize");
    let _g = Tmp(bare.clone());
    install_hook(&bare).expect("хук");
    let work = root.0.join("work");
    git_ok(&root.0, &["clone", "-q", bare.to_str().unwrap(), work.to_str().unwrap()]);
    git_ok(&work, &["config", "user.email", "t@example.com"]);
    git_ok(&work, &["config", "user.name", "Тест"]);

    // Пуш кладёт канон v2 ровно в той форме, в какой его собрало бы ядро, и скрипт рядом.
    let v2 = ver(2, vec![step(1, "b")]);
    for (path, content) in bundle::version_files(&v2) {
        std::fs::write(work.join(&path), content).expect("канон v2");
    }
    write_exec(&work, "scripts/run.sh", b"echo hi\n");
    let (ok, err) = commit_push(&work, "v2 пушем вместе со скриптом");
    assert!(ok, "{err}");
    let tip = git_ok(&bare, &["rev-parse", "main"]);

    bundle::append_missing_versions(&bare, &[v2]).expect("выравнивание");
    assert_eq!(git_ok(&bare, &["rev-parse", "main"]), tip, "выравнивание положило двойника вместо тега");
    assert_eq!(git_ok(&bare, &["rev-parse", "v2^{commit}"]), tip, "тег встал не на свой коммит");
}
