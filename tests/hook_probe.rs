//! ПРОБА линзы проверки ядра: защита main через pre-receive hook.
//!
//! `roundtrip.rs` проверяет УСПЕШНЫЙ путь (обычный push проходит). Сама защита —
//! это отказы, и ни один тест их не вызывает: ни force-push в main, ни удаление
//! main, ни push без list.json. Между тем цена ошибки максимальная: перезапись
//! истории списка необратима.
//!
//! Проверяем НАСТОЯЩИМ git, а не чтением текста хука.

#![cfg(feature = "probes")]

use setfork_core::git::bundle::{self, SerStep, VersionData};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn tmp_root() -> Tmp {
    let p = std::env::temp_dir().join(format!("setfork-hook-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&p).expect("mkdir");
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
        level: "required".into(),
        why: String::new(),
        section: String::new(),
        subtasks: vec![],
        refs: vec![],
        image_key: None,
        needs_human: false,
        needs_human_ask: None,
        danger: false,
    }
}

fn ver(version: i32, note: &str, steps: Vec<SerStep>) -> VersionData {
    VersionData {
        version,
        note: note.into(),
        ts: 1_700_000_000 + version as i64,
        title: "Probe".into(),
        desc: "d".into(),
        tags: vec![],
        ordered: true,
        kind: None,
        steps,
    }
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    let mut full = vec!["-c", "user.email=probe@setfork.com", "-c", "user.name=Probe"];
    full.extend_from_slice(args);
    Command::new("git").current_dir(cwd).args(&full).output().expect("spawn git")
}

fn ok(cwd: &Path, args: &[&str]) -> String {
    let o = run(cwd, args);
    assert!(o.status.success(), "git {args:?} упал:\n{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Готовит bare с v1 и рабочую копию-клон.
fn setup(root: &Path) -> (PathBuf, PathBuf) {
    let bare = root.join("repo.git");
    let work = root.join("work");
    bundle::bootstrap_bare(&[ver(1, "initial", vec![step(1, "Шаг")])], &bare).expect("bootstrap");
    ok(root, &["clone", bare.to_str().unwrap(), work.to_str().unwrap()]);
    (bare, work)
}

#[test]
fn защита_main_от_перезаписи_истории_и_удаления() {
    let root = tmp_root();
    let (bare, work) = setup(&root.0);

    // Хук вообще установлен? Без него всё остальное бессмысленно.
    let hook = bare.join("hooks").join("pre-receive");
    assert!(hook.is_file(), "bootstrap обязан ставить pre-receive: {hook:?}");

    let before = ok(&bare, &["rev-parse", "main"]);

    // 1. FORCE-PUSH В MAIN: переписываем историю (новый коммит на пустом корне).
    ok(&work, &["checkout", "--orphan", "rewritten"]);
    fs::write(work.join("list.json"), r#"{"version":1,"steps":[]}"#).expect("write");
    ok(&work, &["add", "-A"]);
    ok(&work, &["commit", "-m", "переписанная история"]);
    let forced = run(&work, &["push", "--force", "origin", "rewritten:main"]);
    let err = String::from_utf8_lossy(&forced.stderr).to_string();
    println!("FORCE-PUSH В MAIN: success={} stderr={}", forced.status.success(), err.trim());
    assert!(!forced.status.success(), "force-push в main обязан быть отвергнут");
    assert!(err.contains("non-fast-forward"), "отказ должен объяснять причину: {err}");
    assert_eq!(ok(&bare, &["rev-parse", "main"]), before, "main не сдвинулся");

    // 2. УДАЛЕНИЕ MAIN.
    let deleted = run(&work, &["push", "origin", "--delete", "main"]);
    let err = String::from_utf8_lossy(&deleted.stderr).to_string();
    println!("УДАЛЕНИЕ MAIN: success={} stderr={}", deleted.status.success(), err.trim());
    assert!(!deleted.status.success(), "удаление main обязано быть отвергнуто");
    // Язык отказа по умолчанию АНГЛИЙСКИЙ (И1/И2 трека core-i18n): русский приходит,
    // только если его попросили. Раньше здесь стоял русский текст — проба отстала от
    // локализации ещё до того, как перестала компилироваться.
    assert!(err.contains("protected from deletion"), "внятная причина: {err}");
    assert_eq!(ok(&bare, &["rev-parse", "main"]), before, "main на месте");

    // 3. PUSH БЕЗ list.json — канон обязателен в корне.
    ok(&work, &["checkout", "--orphan", "nojson"]);
    let _ = run(&work, &["rm", "-rf", "."]);
    fs::write(work.join("README.md"), "нет канона").expect("write");
    ok(&work, &["add", "-A"]);
    ok(&work, &["commit", "-m", "без list.json"]);
    let nojson = run(&work, &["push", "origin", "nojson"]);
    let err = String::from_utf8_lossy(&nojson.stderr).to_string();
    println!("PUSH БЕЗ list.json: success={} stderr={}", nojson.status.success(), err.trim());
    assert!(!nojson.status.success(), "коммит без list.json обязан быть отвергнут");
    assert!(err.contains("list.json is required"), "внятная причина: {err}");
}

/// Обещание из комментария к хуку: черновые ветки force-push'абельны —
/// защищён только main. Если бы отвергалось всё, работа с предложениями встала бы.
#[test]
fn черновые_ветки_остаются_force_push_абельными() {
    let root = tmp_root();
    let (bare, work) = setup(&root.0);

    ok(&work, &["checkout", "-b", "draft"]);
    fs::write(work.join("README.md"), "первая правка").expect("write");
    ok(&work, &["commit", "-am", "первая"]);
    let first = run(&work, &["push", "origin", "draft"]);
    assert!(first.status.success(), "обычный push черновика:\n{}", String::from_utf8_lossy(&first.stderr));
    let tip1 = ok(&bare, &["rev-parse", "draft"]);

    // Переписываем историю черновика — это законный сценарий (amend предложения).
    fs::write(work.join("README.md"), "переписанная правка").expect("write");
    ok(&work, &["commit", "-a", "--amend", "-m", "переписанная"]);
    let forced = run(&work, &["push", "--force", "origin", "draft"]);
    let err = String::from_utf8_lossy(&forced.stderr).to_string();
    println!("FORCE-PUSH ЧЕРНОВИКА: success={} stderr={}", forced.status.success(), err.trim());
    assert!(forced.status.success(), "черновик обязан оставаться force-push'абельным: {err}");
    assert_ne!(ok(&bare, &["rev-parse", "draft"]), tip1, "tip черновика переписан");
}
