//! ПРОБА линзы проверки ядра: пользовательский тег релиза против служебных тегов версий.
//!
//! Каждая версия списка получает лёгкий тег `vN` — по ним `max_tag_version`
//! определяет, до какой версии материализован репозиторий, и с них же
//! `create_tag` берёт целевой коммит.
//!
//! При этом имя пользовательского релиза проверяется только `valid_branch`
//! (буквы/цифры/`-_.`), а `tag_lightweight(..., force = true)` ПЕРЕВЕШИВАЕТ
//! существующий тег. Значит имя вида `v2` формально разрешено. Проверяю, что
//! из этого выходит.
#![cfg(feature = "probes")]

use setfork_core::git::bundle::{self, SerStep, VersionData};

struct Tmp(std::path::PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ser(n: i32, title: &str) -> SerStep {
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
    }
}

fn ver(version: i32, steps: Vec<SerStep>) -> VersionData {
    VersionData {
        version,
        note: format!("v{version}"),
        ts: 1_700_000_000 + version as i64,
        title: "Tag Probe".into(),
        desc: String::new(),
        tags: vec![],
        ordered: true,
        steps,
    }
}

fn tag_target(bare: &std::path::Path, name: &str) -> String {
    let repo = git2::Repository::open_bare(bare).expect("open");
    repo.refname_to_id(&format!("refs/tags/{name}")).expect("tag").to_string()
}

/// Релиз с именем вида `vN` перевешивает служебный тег версии.
#[test]
#[ignore = "ПАДАЕТ (дефект): релиз v2 перевешивает служебный тег версии — клон по v2 отдаёт версию 3"]
fn релиз_с_именем_версии_перевешивает_служебный_тег() {
    let root = Tmp(std::env::temp_dir().join(format!("setfork-tag-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(&root.0).expect("mkdir");
    let bare = root.0.join("repo.git");

    bundle::bootstrap_bare(
        &[
            ver(1, vec![ser(1, "Первый")]),
            ver(2, vec![ser(1, "Первый"), ser(2, "Второй")]),
            ver(3, vec![ser(1, "Первый"), ser(2, "Второй"), ser(3, "Третий")]),
        ],
        &bare,
    )
    .expect("bootstrap");

    let v2_before = tag_target(&bare, "v2");
    let v3 = tag_target(&bare, "v3");
    assert_ne!(v2_before, v3, "версии на разных коммитах");
    println!("ДО:  v2={} v3={} max={}", &v2_before[..8], &v3[..8], bundle::max_tag_version(&bare));

    // Ровно то, что делает create_tag: имя релиза `v2`, цель — коммит версии 3.
    // valid_branch такое имя пропускает, force=true перевешивает.
    {
        let repo = git2::Repository::open_bare(&bare).expect("open");
        let target = repo.refname_to_id("refs/tags/v3").expect("v3");
        let obj = repo.find_object(target, None).expect("obj");
        repo.tag_lightweight("v2", &obj, true).expect("tag_lightweight force");
    }

    let v2_after = tag_target(&bare, "v2");
    println!("ПОСЛЕ: v2={} (был {}), max={}", &v2_after[..8], &v2_before[..8], bundle::max_tag_version(&bare));

    assert_eq!(
        v2_after, v2_before,
        "служебный тег версии v2 перевешен на чужой коммит: клон по тегу v2 отдаёт содержимое версии 3"
    );
}

/// Имя релиза формы `v<число>` вообще не должно приниматься — оно занято системой.
#[test]
#[ignore = "ПАДАЕТ (дефект): имя v1 проходит валидацию, хотя занято системой"]
fn имя_релиза_формы_vN_не_должно_проходить_валидацию() {
    // valid_branch приватная, повторяем её правило дословно (git_core.rs:23).
    let valid = |name: &str| {
        !name.is_empty()
            && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
            && !name.contains("..")
    };
    for name in ["v1", "v2", "v10"] {
        assert!(
            !valid(name),
            "имя релиза {name:?} проходит валидацию, хотя такие теги система использует для версий"
        );
    }
}
