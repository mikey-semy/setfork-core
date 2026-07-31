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
const READ_ONLY: &[(&str, &str)] =
    &[("get_merge_state", "три материализации для сравнения; смотреть на замороженный список можно")];

/// Тело метода `async fn <name>(` до начала следующего `async fn`.
fn method_body<'a>(src: &'a str, name: &str) -> Option<&'a str> {
    let start = src.find(&format!("async fn {name}("))?;
    let rest = &src[start..];
    let end = rest[1..].find("\n    async fn ").map(|i| i + 1).unwrap_or(rest.len());
    Some(&rest[..end])
}

#[test]
fn каждый_мутирующий_rpc_спрашивает_вердикт() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services/git_core.rs");
    let src = std::fs::read_to_string(&path).expect("читаем git_core.rs");

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
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services/git_core.rs");
    let src = std::fs::read_to_string(&path).expect("читаем git_core.rs");

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
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services/git_core.rs");
    let src = std::fs::read_to_string(&path).expect("читаем git_core.rs");

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
