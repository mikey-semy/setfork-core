//! Страж предусловия записи (Ф1, ADR-0015).
//!
//! Правило «в замороженный или архивный список писать нельзя» знает приложение, а
//! принуждает ядро: каждый мутирующий RPC спрашивает вердикт через
//! `gate::ensure_writable` до любой работы. Компилятором это не выразить —
//! пропущенный вызов просто ничего не делает, — поэтому конвенцию сторожит сканер
//! исходников, как и единую точку обновления main.
//!
//! Пропуск здесь стоит дорого: ровно так уже случилось однажды. Линза 02
//! (security-ledger 29.07, F3) доказала живьём, что проверка, стоявшая только во
//! фронтовом роуте, обходится прод-путём, и замороженный список принимал push.
//!
//! Если метод добавлен в список осознанно как ЧТЕНИЕ — его место в `READ_ONLY`
//! с объяснением, а не в тишине.

use std::path::Path;

/// Мутирующие RPC `GitCore`: каждый обязан звать гейт.
const MUTATING: &[&str] = &[
    "receive_pack",
    "create_branch",
    "delete_branch",
    "merge_branch",
    "merge_resolved",
    "create_tag",
    "update_branch",
    "commit_to_branch",
];

/// Методы, которые ВЫГЛЯДЯТ как запись, но ею не являются, — чтобы отсутствие
/// гейта у них читалось как решение, а не как забывчивость.
///
/// Список полный НАРОЧНО: проверка ниже требует, чтобы КАЖДЫЙ RPC был назван либо
/// здесь, либо в `MUTATING`. Прежде страж знал только перечисленное, и новый метод,
/// пишущий мимо гейта, проходил его молча — проверено пробой: метод, сносящий
/// репозиторий целиком, не уронил ни одну из трёх проверок (линза 05 §3).
const READ_ONLY: &[(&str, &str)] = &[
    ("get_merge_state", "три материализации для сравнения; смотреть на замороженный список можно"),
    ("list_id", "резолв owner/slug → id"),
    ("get_capabilities", "объявление возможностей сервера"),
    ("info_refs_upload_pack", "объявление рефов для клона"),
    ("info_refs_receive_pack", "объявление рефов ПЕРЕД пушем; сам пуш судит receive_pack"),
    ("upload_pack", "отдача объектов клиенту"),
    ("create_bundle", "bundle — снимок для скачивания"),
    ("list_branches", "перечисление веток"),
    ("get_branch_snapshot", "чтение содержимого ветки"),
    ("list_tags", "перечисление тегов"),
    ("render_canon", "показ канона текстом"),
    ("parse_canon", "разбор присланного текста, ничего не пишет"),
    ("list_commits", "история коммитов"),
    ("mirror_push", "пишет на ЧУЖОЙ фордже копию того, что уже есть у нас; канон списка не меняет"),
    ("mirror_check", "проверка доступа к зеркалу без пуша"),
];

/// Тело метода `async fn <name>(` до начала следующего `async fn`.
/// Исходники зоны `git_core` целиком.
///
/// Читаем КАТАЛОГ, а не один файл: 26.08 зона разрезана на подмодули (линза 08), и
/// страж, прибитый к пути `git_core.rs`, покраснел на ровном месте. Это, впрочем,
/// сработало как надо — сторож обязан замечать, что его предмет уехал. Чтобы он
/// замечал ПЕРЕЕЗД, а не отсутствие файла, он теперь читает всё, что в каталоге:
/// разложение методов по подмодулям его больше не сломает.
fn исходники_git_core() -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services/git_core");
    let mut out = String::new();
    let entries = std::fs::read_dir(&dir).expect("каталог src/services/git_core");
    let mut files: Vec<_> =
        entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "rs")).collect();
    files.sort();
    assert!(!files.is_empty(), "в src/services/git_core нет ни одного .rs — страж потерял предмет");
    for f in files {
        out.push_str(&std::fs::read_to_string(&f).unwrap_or_else(|e| panic!("читаем {}: {e}", f.display())));
        out.push('\n');
    }
    out
}

fn method_body<'a>(src: &'a str, name: &str) -> Option<&'a str> {
    let start = src.find(&format!("async fn {name}("))?;
    let rest = &src[start..];
    let end = rest[1..].find("\n    async fn ").map(|i| i + 1).unwrap_or(rest.len());
    Some(&rest[..end])
}

#[test]
fn каждый_мутирующий_rpc_спрашивает_вердикт() {
    let src = исходники_git_core();

    let mut missing = Vec::new();
    for name in MUTATING {
        let body =
            method_body(&src, name).unwrap_or_else(|| panic!("метод {name} не найден — переименован?"));
        if !body.contains("gate::ensure_writable") {
            missing.push(*name);
        }
    }
    assert!(
        missing.is_empty(),
        "мутирующие RPC пишут мимо предусловия записи (ADR-0015): {}\n\
         Добавьте `crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;` \
         до любой работы — или, если метод на самом деле читающий, внесите его в READ_ONLY.",
        missing.join(", ")
    );
}

#[test]
fn читающие_методы_не_обвешаны_гейтом_записи() {
    let src = исходники_git_core();

    for (name, why) in READ_ONLY {
        let body = method_body(&src, name).unwrap_or_else(|| panic!("метод {name} не найден"));
        assert!(
            !body.contains("gate::ensure_writable"),
            "у читающего {name} появился гейт записи, хотя {why}"
        );
    }
}

/// Список MUTATING обязан покрывать все методы, которые пишут. Проверяем со
/// стороны кода: если у метода есть вызов гейта, он должен быть в списке —
/// иначе список тихо разойдётся с реальностью.
#[test]
fn список_мутирующих_не_отстал_от_кода() {
    let src = исходники_git_core();

    let mut current: Option<String> = None;
    let mut unlisted = Vec::new();
    for line in src.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("async fn ")
            && let Some(name) = rest.split('(').next()
        {
            current = Some(name.to_string());
        }
        if line.contains("gate::ensure_writable")
            && let Some(name) = &current
            && !MUTATING.contains(&name.as_str())
        {
            unlisted.push(name.clone());
        }
    }
    assert!(unlisted.is_empty(), "гейт стоит в методах, которых нет в MUTATING: {unlisted:?}");
}

/// КАЖДЫЙ RPC обязан быть назван — пишущим или читающим.
///
/// Это и есть то, чего страж не умел: он проверял перечисленное, а молчал про
/// НЕПЕРЕЧИСЛЕННОЕ. Новый метод, пишущий мимо гейта, проходил все три проверки
/// (проба линзы 05 §3: метод, сносящий репозиторий, не уронил ничего). Теперь
/// новый RPC роняет страж, пока автор не решит, к какому он классу.
#[test]
fn каждый_rpc_отнесён_к_пишущим_или_читающим() {
    let src = исходники_git_core();
    // Только блок реализации трейта: наружу торчит он, а внутренние помощники —
    // не RPC и классификации не требуют.
    let start = src.find("impl GitCore for GitCoreSvc").expect("блок реализации трейта");
    let impl_block = &src[start..];

    let mut unclassified = Vec::new();
    for line in impl_block.lines() {
        let trimmed = line.trim_start();
        // Отступ ровно четыре пробела — это метод трейта, а не вложенная функция.
        if !line.starts_with("    async fn ") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("async fn ")
            && let Some(name) = rest.split('(').next()
            && !MUTATING.contains(&name)
            && !READ_ONLY.iter().any(|(n, _)| *n == name)
        {
            unclassified.push(name.to_string());
        }
    }
    assert!(
        unclassified.is_empty(),
        "RPC не отнесён ни к пишущим, ни к читающим: {unclassified:?}\n\
         Пишет — в MUTATING и зовёт `gate::ensure_writable`; читает — в READ_ONLY с объяснением. \
         Молчаливого третьего класса быть не должно: именно так пишущий метод однажды пройдёт мимо гейта.",
    );
}
