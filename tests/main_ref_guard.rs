//! Страж единой точки обновления main (Ф1, аналог Gitaly UpdaterWithHooks).
//!
//! Защита main (запрет удаления, запрет non-fast-forward, обязательный
//! list.json) живёт в pre-receive hook И в git::update::update_main. Хук ловит
//! только пуш; программную запись через git2 обязана ловить update_main — но
//! ТОЛЬКО если каждый путь записи действительно идёт через неё.
//!
//! Компилятором это не выразить (git2 не запретишь двигать ref), поэтому
//! конвенцию сторожит сканер исходников: прямые способы сдвинуть main вне
//! src/git/update.rs — ошибка сборки намерения, а не стилистика. Поймает и
//! новый RPC, написанный «по памяти» через repo.commit(Some(MAIN_REF), …).

use std::path::Path;

/// Паттерны, которыми в кодовой базе двигали main до Ф1. Список намеренно
/// широкий: ложное срабатывание чинится точечно, пропуск — дырой в защите.
const FORBIDDEN: &[&str] = &[
    "commit(Some(MAIN_REF)",
    "commit(Some(&MAIN_REF",
    "commit(Some(\"refs/heads/main\")",
    ".reference(MAIN_REF",
    ".reference(\"refs/heads/main\"",
    ".reference_matching(MAIN_REF",
    ".reference_matching(\"refs/heads/main\"",
];

fn scan(dir: &Path, violations: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read_dir src/") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            scan(&path, violations);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        // Единственное разрешённое место прямой записи main.
        if path.ends_with("update.rs") && path.parent().is_some_and(|p| p.ends_with("git")) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read source");
        for (i, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // Комментарии не считаются: в них паттерны упоминаются как документация.
            if trimmed.starts_with("//") || trimmed.starts_with("*") || trimmed.starts_with("/*") {
                continue;
            }
            for pat in FORBIDDEN {
                if line.contains(pat) {
                    violations.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                }
            }
        }
    }
}

/// Программная запись не обходит защиту main: прямые ref-операции над
/// refs/heads/main разрешены только внутри git/update.rs (update_main).
#[test]
fn main_двигается_только_через_update_main() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    scan(&src, &mut violations);
    assert!(
        violations.is_empty(),
        "прямое обновление refs/heads/main вне git/update.rs — защита main обходится:\n{}",
        violations.join("\n")
    );
}
