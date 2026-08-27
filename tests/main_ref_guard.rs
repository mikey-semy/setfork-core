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

/// API git2, которыми ref МОЖНО СДВИНУТЬ. Это список ВОЗМОЖНОСТЕЙ, а не форм записи —
/// и в этом вся разница.
///
/// ⚠️ Прежняя версия перечисляла точные формы вызова, встречавшиеся в коде до Ф1
/// (`commit(Some(MAIN_REF)`, `.reference(MAIN_REF`, …). Проверено 27.08: она пропускала
/// `repo.branch("main", &commit, true)` — законный способ git2 передвинуть ветку, которого
/// в списке просто не было. Сторож оставался ЗЕЛЁНЫМ.
///
/// Урок пришёл со стороны фронта, где то же самое случилось с их счётчиком: правило,
/// привязанное к ТЕГУ, пропустило целую роль. Форма записи у каждого автора своя, а набор
/// API, способных сдвинуть ссылку, меняется вместе с git2 — то есть раз в годы.
const MUTATING_REF_API: &[&str] = &[
    "commit(Some(", // коммит с обновлением ref
    ".reference(",  // создать/переписать прямую ссылку
    ".reference_symbolic(",
    ".reference_matching(",
    ".branch(",     // создать ИЛИ передвинуть ветку (force = true)
    ".set_target(", // подвинуть уже найденную ссылку
    ".rename(",     // переименовать ветку/ссылку В main
    ".reset(",      // сдвинуть текущую ветку
];

/// Как в строке может быть назван main.
///
/// `set_head` намеренно НЕ считается мутацией: он двигает HEAD, а не цель ветки, и в bare-репо
/// лишь объявляет ветку по умолчанию. Целостность main от него не зависит.
const NAMES_MAIN: &[&str] = &["MAIN_REF", "\"refs/heads/main\"", "\"main\""];

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
            // Признак нарушения — ПАРА: мутирующий API И упоминание main в одной строке.
            // По отдельности каждое законно: `refname_to_id(MAIN_REF)` читает, а
            // `commit(Some(&format!("refs/heads/{branch}")))` пишет в чужую ветку.
            let mutates = MUTATING_REF_API.iter().any(|p| line.contains(p));
            let names_main = NAMES_MAIN.iter().any(|p| line.contains(p));
            if mutates && names_main {
                violations.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
            }
        }
    }
}

/// Программная запись не обходит защиту main: прямые ref-операции над
/// refs/heads/main разрешены только внутри git/update.rs (update_main).
#[test]
fn main_moves_only_through_update_main() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    scan(&src, &mut violations);
    assert!(
        violations.is_empty(),
        "прямое обновление refs/heads/main вне git/update.rs — защита main обходится:\n{}",
        violations.join("\n")
    );
}
