//! Чтение авторских файлов версии (ADR-0028) — то, что отдаёт `GetAuthoredFiles` экспорту
//! скилла. Проверяется на настоящем репозитории с настоящим хуком: файлы приходят ПУШЕМ,
//! версия после них — ВЕБ-ПРАВКОЙ, и отдать надо байты, покрытые SHA именно этой версии.
use std::path::{Path, PathBuf};
use std::process::Command;

use setfork_core::git::authored::{AuthoredFile, authored_files, version_commit};
use setfork_core::git::bundle::{self, SerStep, VersionData, install_hook};

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out =
        Command::new("git").current_dir(dir).env("SETFORK_ROLE", "owner").args(args).output().expect("git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

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
fn ver(version: i32) -> VersionData {
    VersionData {
        version,
        note: "edit".into(),
        ts: 1_700_000_000 + version as i64,
        title: "Runbook".into(),
        desc: String::new(),
        tags: vec![],
        ordered: true,
        kind: None,
        skill_header: None,
        steps: vec![step(1, &format!("шаг v{version}"))],
    }
}

/// v1 без файлов → пуш со скриптом и справкой → веб-правка v2 поверх.
fn repo_with_scripts() -> (Tmp, Tmp, PathBuf) {
    let bare = bundle::materialize_repo(&[ver(1)]).expect("materialize");
    let bare_guard = Tmp(bare.clone());
    install_hook(&bare).expect("хук");
    let root = std::env::temp_dir().join(format!("setfork-authored-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("tmp");
    let work = root.join("w");
    git(&root, &["clone", "-q", bare.to_str().unwrap(), work.to_str().unwrap()]);
    git(&work, &["config", "user.email", "t@example.com"]);
    git(&work, &["config", "user.name", "Тест"]);
    std::fs::create_dir_all(work.join("scripts")).expect("mkdir");
    std::fs::create_dir_all(work.join("references")).expect("mkdir");
    std::fs::write(work.join("scripts/run.sh"), "#!/bin/sh\necho hi\n").expect("script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(work.join("scripts/run.sh"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    std::fs::write(work.join("references/why.md"), "# Почему\n").expect("ref");
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "скрипт"]);
    git(&work, &["push", "-q", "origin", "main"]);
    bundle::append_versions(&bare, &[ver(2)]).expect("веб-версия v2");
    (bare_guard, Tmp(root), bare)
}

#[test]
fn a_version_returns_its_authored_files_byte_for_byte() {
    let (_b, _r, bare) = repo_with_scripts();
    let repo = git2::Repository::open_bare(&bare).expect("open");
    let v2 = version_commit(&repo, 2).expect("v2 есть");
    let files = authored_files(&repo, v2).expect("чтение");
    assert_eq!(
        files,
        vec![
            AuthoredFile {
                path: "scripts/run.sh".into(),
                content: b"#!/bin/sh\necho hi\n".to_vec(),
                executable: true
            },
            AuthoredFile {
                path: "references/why.md".into(),
                content: "# Почему\n".as_bytes().to_vec(),
                executable: false
            },
        ],
        "отдано не то, что покрыто SHA версии"
    );
}

// Версия ДО пуша файлов не имеет — отдать их значило бы приписать v1 чужие байты.
#[test]
fn an_earlier_version_does_not_get_later_files() {
    let (_b, _r, bare) = repo_with_scripts();
    let repo = git2::Repository::open_bare(&bare).expect("open");
    let v1 = version_commit(&repo, 1).expect("v1 есть");
    assert!(authored_files(&repo, v1).expect("чтение").is_empty(), "v1 получила файлы, появившиеся позже");
}

#[test]
fn version_zero_is_main_and_a_missing_version_is_none() {
    let (_b, _r, bare) = repo_with_scripts();
    let repo = git2::Repository::open_bare(&bare).expect("open");
    let main = repo.refname_to_id("refs/heads/main").expect("main");
    assert_eq!(version_commit(&repo, 0), Some(main));
    assert_eq!(version_commit(&repo, 99), None, "несуществующая версия выдана за существующую");
    assert_eq!(version_commit(&repo, -1), None, "отрицательная версия выдана за вершину main");
}

// Выдача описывает то же множество, что приём: ссылку правило путей не пускает, и
// отдать её экспорту (а дальше — в чужой архив) нельзя, даже если она как-то оказалась
// в дереве (записано до правила, собрано руками).
#[test]
fn a_symlink_in_the_tree_is_not_handed_out() {
    let root = std::env::temp_dir().join(format!("setfork-authored-link-{}", uuid::Uuid::new_v4()));
    let _g = Tmp(root.clone());
    let repo = git2::Repository::init_bare(&root).expect("init");
    let sig = git2::Signature::new("Тест", "t@example.com", &git2::Time::new(1_700_000_000, 0)).expect("sig");
    let mut dir = repo.treebuilder(None).expect("tb");
    dir.insert("ok.sh", repo.blob(b"echo ok\n").expect("blob"), 0o100644).expect("ok");
    dir.insert("passwd", repo.blob(b"/etc/passwd").expect("blob"), 0o120000).expect("link");
    let mut top = repo.treebuilder(None).expect("tb");
    top.insert("list.json", repo.blob(b"{}").expect("blob"), 0o100644).expect("list");
    top.insert("scripts", dir.write().expect("dir"), 0o040000).expect("scripts");
    let tree = repo.find_tree(top.write().expect("top")).expect("tree");
    let commit = repo.commit(None, &sig, &sig, "c", &tree, &[]).expect("commit");
    let paths: Vec<String> =
        authored_files(&repo, commit).expect("чтение").into_iter().map(|f| f.path).collect();
    assert_eq!(paths, vec!["scripts/ok.sh"], "ссылка ушла экспорту");
}
