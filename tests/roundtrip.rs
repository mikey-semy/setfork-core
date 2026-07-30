//! Интеграционный тест полного git-цикла против материализованного bare-репо:
//! bootstrap → smart-http advertise → `git clone` → правка+`git push` →
//! `append_versions` (веб-версия поверх пуша) → `git pull`.
//!
//! Использует НАСТОЯЩИЙ `git` (есть в CI ubuntu-latest и в git-for-windows),
//! проверяя, что git2-материализация байт-совместима с git и что запушенная
//! пользователем история сохраняется при досыпке версий.

use setfork_core::git::bundle::{self, SerStep, VersionData};
use setfork_core::git::smart_http;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Каталог-однодневка: удаляется на Drop (в т.ч. при panic внутри теста).
struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn tmp_root() -> Tmp {
    let p = std::env::temp_dir().join(format!("setfork-rt-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&p).expect("create tmp root");
    Tmp(p)
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
        why: String::new(),
        section: String::new(),
        subtasks: vec![],
        refs: vec![],
    }
}
fn ver(version: i32, note: &str, steps: Vec<SerStep>) -> VersionData {
    VersionData {
        version,
        note: note.into(),
        ts: 1_700_000_000 + version as i64,
        title: "Redis Caching".into(),
        desc: "Set up Redis".into(),
        tags: vec!["redis".into()],
        ordered: true,
        kind: None,
        steps,
    }
}

/// Запуск git в каталоге `cwd`; паника со stderr при ненулевом коде. Возвращает stdout (trim).
fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(out.status.success(), "git {args:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// git с идентичностью коммиттера, чтобы не зависеть от глобального git config в CI.
fn git_as(cwd: &Path, args: &[&str]) -> String {
    let mut full = vec!["-c", "user.email=test@setfork.com", "-c", "user.name=Tester"];
    full.extend_from_slice(args);
    git(cwd, &full)
}

/// Возвращает true, если git завершился с кодом 0 (для проверок-предикатов).
fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git").current_dir(cwd).args(args).output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn clone_push_pull_roundtrip() {
    let root = tmp_root();
    let bare = root.0.join("repo.git");
    let work = root.0.join("work");

    // 1. Бутстрап bare-репо из версии v1.
    let v1 = ver(1, "initial", vec![step(1, "Install Redis")]);
    bundle::bootstrap_bare(&[v1], &bare).expect("bootstrap_bare");
    assert_eq!(bundle::max_tag_version(&bare), 1, "v1 tag present");

    // 2. smart-http advertise — валидный pkt-line с service-заголовком и ветками.
    let up = smart_http::upload_pack_advertise(&bare, None).expect("upload advertise");
    let up = String::from_utf8_lossy(&up);
    assert!(up.contains("# service=git-upload-pack"), "upload service header");
    assert!(up.contains("refs/heads/main"), "advertises main");
    assert!(up.contains("refs/tags/v1"), "advertises v1 tag");
    let rp = smart_http::receive_pack_advertise(&bare, None).expect("receive advertise");
    assert!(String::from_utf8_lossy(&rp).contains("# service=git-receive-pack"), "receive service header");

    // 3. Клонируем как обычный git-клиент.
    git(&root.0, &["clone", bare.to_str().unwrap(), work.to_str().unwrap()]);
    assert!(work.join("list.json").is_file(), "list.json cloned");
    assert!(work.join("README.md").is_file(), "README cloned");
    assert!(work.join("steps").is_dir(), "steps/ cloned");
    assert!(git(&work, &["tag"]).contains("v1"), "clone sees v1 tag");

    // 4. Пользователь правит и пушит (list.json остаётся → проходит pre-receive hook).
    fs::write(work.join("README.md"), "# Redis Caching\n\nlocal user edit\n").unwrap();
    git_as(&work, &["commit", "-am", "user edit"]);
    let pushed = git(&work, &["rev-parse", "HEAD"]);
    git_as(&work, &["push", "origin", "main"]);

    // 5. Пуш дошёл до bare (tip = наш коммит).
    assert_eq!(git(&bare, &["log", "-1", "--format=%s", "main"]), "user edit", "bare got the push");

    // 6. Веб-версия v2 досыпается ПОВЕРХ пуша — история пользователя сохраняется.
    let v2 = ver(2, "add config step", vec![step(1, "Install Redis"), step(2, "Configure")]);
    bundle::append_versions(&bare, &[v2]).expect("append_versions");
    assert_eq!(bundle::max_tag_version(&bare), 2, "v2 tag added");
    assert!(
        git_ok(&bare, &["merge-base", "--is-ancestor", &pushed, "main"]),
        "pushed commit is preserved in history"
    );

    // 7. Клиент подтягивает изменения: fast-forward + новый тег v2.
    git_as(&work, &["pull", "origin", "main"]);
    git(&work, &["fetch", "--tags", "origin"]);
    assert!(git(&work, &["tag"]).contains("v2"), "pull delivers v2 tag");
    let log = git(&work, &["log", "--format=%s"]);
    assert!(log.contains("user edit"), "user commit still in history after pull");
    assert!(log.lines().any(|l| l.starts_with("v2")), "v2 commit pulled");
}
