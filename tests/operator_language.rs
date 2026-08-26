//! ГЕЙТ: операторский вывод ядра — по-английски.
//!
//! Решение владельца 2026-07-31 (трек `core-i18n`, И0): язык ядра по умолчанию
//! английский, правило общее для хука, логов и CLI. Русский показывается только по
//! явному сигналу — и только через каталог `git/messages.rs`, который двуязычен по
//! построению (И2).
//!
//! ⚠️ Решение было объявлено, подметено разово (core#75) и после этого разъехалось
//! ДВАЖДЫ: часть строк подметание не тронуло вовсе (`repo.rs`, `update.rs`,
//! `version.rs`, `mirror.rs` — они старше И0), а `gate.rs` и `main.rs` получили новые
//! русские строки уже ПОСЛЕ закрытия решения. Причина не в невнимательности:
//! правило держалось только памятью. Отсюда этот гейт — линза 10, находка 10-F1.
//!
//! Что судим: **строковые литералы боевого кода**. Комментарии по-русски — норма
//! проекта и его главный актив, их не трогаем. Тестовые модули не трогаем тоже:
//! имена тестов и сообщения `assert!` пишутся на языке того, кто их читает, — автора.

use std::path::{Path, PathBuf};

/// Файлы, где русский законен по построению.
const BILINGUAL: &[&str] = &[
    // Каталог сообщений хука: русский раздел — это и есть предмет решения И2,
    // текст выбирается по языку пушащего человека.
    "src/git/messages.rs",
];

/// ДОЛГ, а не разрешение (находка 10-F8).
///
/// Это не операторский вывод: пять текстов про зеркало доезжают до ВЛАДЕЛЬЦА списка —
/// в статус настроек (`record_mirror_result`) и в ответ кнопки «проверить доступ»
/// (`MirrorCheckResponse.error`, фронт показывает его как есть). Перевести их на
/// английский значило бы показать англоязычный текст русскому пользователю; правильный
/// ответ другой и он уже принят проектом — И1: ядро отдаёт **машинный код**, текст
/// подбирает фронт по языку человека. Это правка на два репозитория (сначала фронт
/// учится кодам, потом ядро перестаёт слать прозу), поэтому здесь она названа, а не
/// сделана молча.
///
/// Список закрыт на добавление: новая русская строка в боевом коде обязана быть
/// операторской и английской. Если она пользовательская — ей место в коде причины.
const USER_PROSE_DEBT: &[&str] = &[
    "SETFORK_MIRROR_SECRET не задан на сервере — зеркало не может расшифровать токен",
    "токен зеркала не расшифровался (секрет сменён?) — сохраните токен заново",
    "зеркало не настроено",
    "SETFORK_MIRROR_SECRET не задан на сервере",
    "токен зеркала не расшифровался (секрет сменён?) — сохраните заново",
];

/// Один литерал: где нашли и что именно.
struct Literal {
    file: String,
    line: usize,
    текст: String,
}

fn is_cyrillic(s: &str) -> bool {
    s.chars().any(|c| matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё'))
}

/// Разбор исходника на строковые литералы БОЕВОГО кода.
///
/// Одним проходом: комментарии (строчные и блочные, блочные в Rust вложенные),
/// обычные и сырые строки, символьные литералы, и — по глубине скобок — вырезание
/// модулей под `#[cfg(test)]`. Считать `#[cfg(test)]` «хвостом файла» было НЕЛЬЗЯ:
/// это верно по конвенции, но конвенция — не механизм, а гейт должен быть механизмом.
fn literals(текст: &str) -> Vec<(usize, String)> {
    let b: Vec<char> = текст.chars().collect();
    let n = b.len();
    let (mut i, mut line) = (0usize, 1usize);
    let mut found = Vec::new();
    // Глубина, с которой начинается вырезанный тестовый модуль (None — не в нём).
    let mut depth = 0usize;
    let mut test_body_at: Option<usize> = None;
    let mut awaiting_test_body = false;

    while i < n {
        let c = b[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        // Строчный комментарий.
        if c == '/' && i + 1 < n && b[i + 1] == '/' {
            while i < n && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // Блочный комментарий — вложенный, как в Rust.
        if c == '/' && i + 1 < n && b[i + 1] == '*' {
            let mut уровень = 1;
            i += 2;
            while i < n && уровень > 0 {
                if b[i] == '\n' {
                    line += 1;
                } else if b[i] == '/' && i + 1 < n && b[i + 1] == '*' {
                    уровень += 1;
                    i += 1;
                } else if b[i] == '*' && i + 1 < n && b[i + 1] == '/' {
                    уровень -= 1;
                    i += 1;
                }
                i += 1;
            }
            continue;
        }
        // Символьный литерал: 'a', '\n', но НЕ лайфтайм 'a.
        if c == '\'' && i + 2 < n {
            let end = if b[i + 1] == '\\' {
                (i + 2..n).find(|&j| b[j] == '\'')
            } else if b[i + 2] == '\'' {
                Some(i + 2)
            } else {
                None
            };
            if let Some(j) = end {
                i = j + 1;
                continue;
            }
        }
        // Сырая строка: r"…", r#"…"#, br#"…"#.
        let raw = {
            let mut j = i;
            if b[j] == 'b' {
                j += 1;
            }
            if j < n && b[j] == 'r' {
                j += 1;
                let hashes = {
                    let s = j;
                    while j < n && b[j] == '#' {
                        j += 1;
                    }
                    j - s
                };
                if j < n && b[j] == '"' { Some((j + 1, hashes)) } else { None }
            } else {
                None
            }
        };
        if let Some((start, hashes)) = raw {
            let closing: String = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
            let tail: String = b[start..].iter().collect();
            let len = tail.find(&closing).unwrap_or(tail.len());
            let body: String = tail.chars().take(tail[..len].chars().count()).collect();
            if test_body_at.is_none() {
                found.push((line, body.clone()));
            }
            line += body.matches('\n').count();
            i = start + body.chars().count() + closing.chars().count();
            continue;
        }
        // Обычная строка (в т.ч. b"…").
        if c == '"' || (c == 'b' && i + 1 < n && b[i + 1] == '"') {
            let start = if c == '"' { i + 1 } else { i + 2 };
            let mut j = start;
            let mut body = String::new();
            while j < n {
                if b[j] == '\\' {
                    body.push(b[j]);
                    if j + 1 < n {
                        body.push(b[j + 1]);
                    }
                    j += 2;
                    continue;
                }
                if b[j] == '"' {
                    break;
                }
                body.push(b[j]);
                j += 1;
            }
            if test_body_at.is_none() {
                found.push((line, body.clone()));
            }
            line += body.matches('\n').count();
            i = j + 1;
            continue;
        }
        // Пометка тестового модуля.
        if c == '#' && text_at(&b, i).starts_with("#[cfg(test)]") {
            awaiting_test_body = true;
            i += "#[cfg(test)]".chars().count();
            continue;
        }
        // Пометка без блока (`#[cfg(test)] use …;`) — ожидание тела ОТМЕНЯЕМ на `;`.
        // Иначе оно дожило бы до ближайшей чужой `{` и вырезало из проверки посторонний
        // кусок боевого кода — молча, то есть худшим для гейта способом.
        if c == ';' && awaiting_test_body {
            awaiting_test_body = false;
            i += 1;
            continue;
        }
        if c == '{' {
            depth += 1;
            if awaiting_test_body {
                test_body_at = Some(depth);
                awaiting_test_body = false;
            }
            i += 1;
            continue;
        }
        if c == '}' {
            if test_body_at == Some(depth) {
                test_body_at = None;
            }
            depth = depth.saturating_sub(1);
            i += 1;
            continue;
        }
        i += 1;
    }
    found
}

fn text_at(b: &[char], i: usize) -> String {
    b[i..(i + 12).min(b.len())].iter().collect()
}

fn walk(root: &Path, files: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(root).expect("read_dir src/").flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, files);
        } else if p.extension().is_some_and(|x| x == "rs") {
            files.push(p);
        }
    }
}

#[test]
fn operator_output_is_english() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    walk(&root.join("src"), &mut files);
    files.sort();
    assert!(files.len() > 20, "обход src/ нашёл всего {} файлов — гейт сломан", files.len());

    let mut bad: Vec<Literal> = Vec::new();
    for путь in &files {
        let rel = путь.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        if BILINGUAL.contains(&rel.as_str()) {
            continue;
        }
        let текст = std::fs::read_to_string(путь).expect("чтение исходника");
        for (line, лит) in literals(&текст) {
            if is_cyrillic(&лит) && !USER_PROSE_DEBT.contains(&лит.as_str()) {
                bad.push(Literal { file: rel.clone(), line, текст: лит });
            }
        }
    }

    // Список долга обязан быть живым: строка, которой в коде уже нет, — это забытая
    // запись, из-за которой гейт молча пропустил бы такую же новую. Такой список
    // перестаёт быть долгом и становится дырой.
    // Искать по СЫРОМУ тексту файла было нельзя: рядом с такой строкой обычно лежит
    // русский комментарий с тем же текстом, и запись «жила» бы вечно, даже когда сама
    // строка уже переведена (поймано мутацией: перевод строки гейт не заметил).
    // Поэтому сверяем со списком ЛИТЕРАЛОВ — тем же, по которому судим.
    let all_literals: Vec<String> = files
        .iter()
        .flat_map(|p| literals(&std::fs::read_to_string(p).unwrap_or_default()))
        .map(|(_, л)| л)
        .collect();
    for д in USER_PROSE_DEBT {
        assert!(
            all_literals.iter().any(|л| л == д),
            "в списке долга 10-F8 числится строка, которой в коде больше нет: {д:?}. \
             Убери её из списка — иначе гейт пропустит новую такую же."
        );
    }

    if !bad.is_empty() {
        let list_names = bad
            .iter()
            .map(|л| format!("  {}:{}  {}", л.file, л.line, л.текст.chars().take(80).collect::<String>()))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "операторский вывод ядра обязан быть английским (решение владельца 31.07, трек \
             core-i18n, И0), а по-русски написано {} строк:\n{list_names}\n\n\
             Комментарии по-русски — норма, судятся только строковые литералы боевого кода. \
             Если строка законно двуязычна, её место — в каталоге src/git/messages.rs.",
            bad.len()
        );
    }
}
