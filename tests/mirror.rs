//! Ф3: пуш зеркала — против ЛОКАЛЬНОГО bare как «удалённого» (file://).
//!
//! mirror_push принимает только https:// (токен встраивается в креды), поэтому
//! здесь гоняем внутренности тем же шелл-вызовом, что и он: refspec main+tags,
//! force, prune. Плюс отдельно — что валидация https не пускает file:// наружу.
//!
//! Без БД: чистый git. Запуск обычным cargo test.

use std::path::{Path, PathBuf};
use std::process::Command;

use setfork_core::git::bundle::{self, SerStep, VersionData};

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tmp_root(prefix: &str) -> Tmp {
    let p = std::env::temp_dir().join(format!("setfork-{prefix}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).expect("tmp");
    Tmp(p)
}

fn git(args: &[&str]) -> (bool, String) {
    // Ф5: зеркало пушит в СВОЙ bare без хука SetFork, но тесты гоняют и обычные
    // репозитории — роль владельца не мешает и снимает зависимость от умолчания.
    let out = Command::new("git").env("SETFORK_ROLE", "owner").args(args).output().expect("spawn git");
    (
        out.status.success(),
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
    )
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
        why: String::new(),
        section: String::new(),
        subtasks: vec![],
        refs: vec![],
    }
}

fn ver(n: i32) -> VersionData {
    VersionData {
        version: n,
        note: format!("v{n}"),
        ts: 1_700_000_000 + n as i64,
        title: "Mirror me".into(),
        desc: String::new(),
        tags: vec![],
        ordered: true,
        kind: None,
        steps: vec![ser_step(1, "First")],
    }
}

/// Тот же пуш, что делает mirror_push, но с произвольным URL (file:// для теста).
fn push_like_mirror(bare: &Path, url: &str) -> (bool, String) {
    git(&[
        "--git-dir",
        bare.to_str().unwrap(),
        "push",
        "--prune",
        url,
        "+refs/heads/main:refs/heads/main",
        "+refs/tags/*:refs/tags/*",
    ])
}

#[test]
fn зеркало_получает_main_и_теги_но_не_ветки_и_прунится() {
    let root = tmp_root("mirror");
    let src = root.0.join("src.git");
    let dst = root.0.join("dst.git");
    bundle::bootstrap_bare(&[ver(1), ver(2)], &src).expect("src");
    git(&["init", "--bare", dst.to_str().unwrap()]).0.then_some(()).expect("dst init");

    // Ветка-черновик в источнике: зеркалиться НЕ должна (решение трека: черновой
    // шум чужой фордже не нужен).
    {
        let repo = git2::Repository::open_bare(&src).expect("open src");
        let tip = repo.refname_to_id("refs/heads/main").expect("main");
        repo.reference("refs/heads/pr-draft", tip, true, "draft").expect("draft branch");
    }

    let url = format!("file:///{}", dst.to_str().unwrap().replace('\\', "/"));
    let (ok, log) = push_like_mirror(&src, &url);
    assert!(ok, "первый пуш: {log}");

    let (_, refs) = git(&["--git-dir", dst.to_str().unwrap(), "for-each-ref", "--format=%(refname)"]);
    assert!(refs.contains("refs/heads/main"), "main уехал: {refs}");
    assert!(refs.contains("refs/tags/v1") && refs.contains("refs/tags/v2"), "теги уехали: {refs}");
    assert!(!refs.contains("pr-draft"), "ветки предложений не зеркалятся: {refs}");

    // Повторный пуш идемпотентен.
    let (ok, log) = push_like_mirror(&src, &url);
    assert!(ok, "повторный пуш: {log}");

    // Новая версия и релизный тег доезжают, снятый тег прунится.
    bundle::append_versions(&src, &[ver(3)]).expect("v3");
    {
        let repo = git2::Repository::open_bare(&src).expect("open src");
        let tip = repo.refname_to_id("refs/heads/main").expect("main");
        let obj = repo.find_object(tip, None).expect("obj");
        repo.tag_lightweight("release-1", &obj, true).expect("release tag");
        repo.tag_delete("v1").expect("drop v1"); // симуляция снятого тега
    }
    let (ok, log) = push_like_mirror(&src, &url);
    assert!(ok, "инкрементальный пуш: {log}");
    let (_, refs) = git(&["--git-dir", dst.to_str().unwrap(), "for-each-ref", "--format=%(refname)"]);
    assert!(refs.contains("refs/tags/v3"), "новая версия доехала: {refs}");
    assert!(refs.contains("refs/tags/release-1"), "релизный тег доехал: {refs}");
    assert!(!refs.contains("refs/tags/v1"), "prune убрал снятый тег: {refs}");

    // Зеркало трогали руками (сдвинули main) — force возвращает истину.
    {
        let repo = git2::Repository::open_bare(&dst).expect("open dst");
        let v1 = repo.refname_to_id("refs/tags/v2").expect("v2");
        repo.reference("refs/heads/main", v1, true, "manual meddling").expect("meddle");
    }
    let (ok, log) = push_like_mirror(&src, &url);
    assert!(ok, "force-пуш поверх ручных правок зеркала: {log}");
    let src_main = git(&["--git-dir", src.to_str().unwrap(), "rev-parse", "main"]).1;
    let dst_main = git(&["--git-dir", dst.to_str().unwrap(), "rev-parse", "main"]).1;
    assert_eq!(src_main, dst_main, "зеркало догнало истину");
}

/// mirror_push наружу не пустит file:// — токен встраивается только в https.
#[tokio::test]
async fn file_url_отвергается_валидацией() {
    let root = tmp_root("mirror-val");
    let src = root.0.join("src.git");
    bundle::bootstrap_bare(&[ver(1)], &src).expect("src");
    let err = setfork_core::git::mirror::mirror_push(&src, "file:///tmp/x.git", "T")
        .await
        .expect_err("file:// запрещён");
    assert!(err.contains("https"), "{err}");
}
