//! ЕДИНАЯ точка обновления `refs/heads/main` — аналог `updateref.UpdaterWithHooks`
//! из Gitaly: их OperationService прогоняет хуки вручную, поэтому программная
//! запись проходит ту же проверку, что и пуш человека.
//!
//! У нас pre-receive hook (защита от удаления, от non-fast-forward, обязательный
//! `list.json`) срабатывает только на receive-pack; вся запись через git2 шла бы
//! мимо него. Поэтому те же правила продублированы здесь кодом, и ЛЮБОЕ движение
//! main из кода обязано идти через `update_main` — прямые `repo.commit(Some(MAIN_REF))`
//! и `repo.reference(MAIN_REF, …)` вне этого модуля запрещены (страж-тест
//! `tests/main_ref_guard.rs` ищет такие вызовы в исходниках).
//!
//! Голосование/субтранзакции Praefect не переносим: это про кворум реплик
//! кластера Gitaly, у нас одно ядро и один том.
use git2::{ErrorCode, Oid, Repository, TreeWalkMode, TreeWalkResult};

use super::MAIN_REF;
use super::serialize::tree_path_allowed;

/// Первый путь дерева вне allowlist'а (или None — дерево чистое).
///
/// Обход pre-order: callback получает префикс каталога и запись, полный путь —
/// их склейка. Пропускаем ТОЛЬКО каталоги, в них спускаемся: пустых каталогов
/// git не хранит, поэтому лишний каталог всё равно будет пойман по содержимому —
/// зато в отказе окажется `assets/x.png`, а не голое `assets`, по которому
/// человеку неясно, что убирать.
///
/// Судить «только блобы» было НЕЛЬЗЯ: подмодуль (gitlink) libgit2 отдаёт как
/// `ObjectType::Commit`, и такая запись проезжала бы мимо правила целиком
/// (авто-ревью core#70, P2).
fn first_foreign_path(tree: &git2::Tree<'_>) -> Result<Option<String>, git2::Error> {
    let mut foreign: Option<String> = None;
    tree.walk(TreeWalkMode::PreOrder, |dir, entry| {
        if entry.kind() == Some(git2::ObjectType::Tree) {
            return TreeWalkResult::Ok;
        }
        let path = format!("{dir}{}", entry.name().unwrap_or("<не-utf8>"));
        if tree_path_allowed(&path) {
            TreeWalkResult::Ok
        } else {
            foreign = Some(path);
            TreeWalkResult::Abort
        }
    })
    // Abort из callback libgit2 отдаёт как ошибку GIT_EUSER — для нас это не
    // сбой, а найденная причина отказа; отличаем по уже заполненному foreign.
    .or_else(|e| if foreign.is_some() { Ok(()) } else { Err(e) })?;
    Ok(foreign)
}

/// Почему main не сдвинулся. Каждый вариант — то же правило, что у pre-receive.
#[derive(Debug, PartialEq, Eq)]
pub enum MainUpdateError {
    /// В дереве нового tip нет `list.json` — канон обязателен в каждом коммите main.
    MissingListJson,
    /// В дереве нового tip есть путь вне allowlist'а (Ф0): в git уходит только
    /// то, что обязано пережить clone и вернуться через push (ADR-0014).
    /// Несёт сам путь — отказ обязан называть причину, а не только факт.
    ForeignPath(String),
    /// Новый tip не потомок старого — переписывание истории main запрещено.
    NonFastForward,
    /// main уже не там, где ожидал вызывающий (CAS не сошёлся) — конкурентная запись.
    Stale,
    /// Ошибка git2 (текст для журнала).
    Git(String),
}

impl std::fmt::Display for MainUpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MainUpdateError::MissingListJson => write!(f, "list.json is required at the repo root"),
            MainUpdateError::ForeignPath(p) => write!(
                f,
                "в дереве списка разрешены только README.md, list.json и .gitattributes; лишний путь: {p}"
            ),
            MainUpdateError::NonFastForward => write!(f, "non-fast-forward update of main is forbidden"),
            MainUpdateError::Stale => write!(f, "main moved concurrently (stale expected tip)"),
            MainUpdateError::Git(e) => write!(f, "git2: {e}"),
        }
    }
}

impl From<git2::Error> for MainUpdateError {
    fn from(e: git2::Error) -> Self {
        MainUpdateError::Git(e.to_string())
    }
}

/// Сдвигает `refs/heads/main` на `new_tip`, прогнав те же проверки, что pre-receive:
///
/// * удаление невозможно по сигнатуре (нет варианта «нет нового tip»);
/// * `new_tip` обязан нести `list.json` в корне дерева;
/// * при существующем main — только fast-forward от `expected_old`;
/// * CAS: main обязан стоять ровно на `expected_old` (`None` = ref не существует),
///   сама запись атомарна (`reference_matching`), гонка → `Stale`.
///
/// `log_message` попадает в reflog — по нему видно, какой путь записи двигал main.
pub fn update_main(
    repo: &Repository,
    new_tip: Oid,
    expected_old: Option<Oid>,
    log_message: &str,
) -> Result<(), MainUpdateError> {
    // Обязательный list.json — правило хука «каждый пушнутый коммит несёт канон».
    let commit = repo.find_commit(new_tip)?;
    let tree = commit.tree()?;
    if tree.get_path(std::path::Path::new("list.json")).is_err() {
        return Err(MainUpdateError::MissingListJson);
    }
    // Состав дерева — второе правило хука, продублированное здесь по той же
    // причине, что и первое: git2-запись проходит мимо pre-receive.
    if let Some(path) = first_foreign_path(&tree)? {
        return Err(MainUpdateError::ForeignPath(path));
    }

    // CAS-проверка до записи — чтобы отличить Stale от NonFastForward в ошибке.
    let current = repo.refname_to_id(MAIN_REF).ok();
    if current != expected_old {
        return Err(MainUpdateError::Stale);
    }

    if let Some(old) = expected_old {
        // Fast-forward: старый tip обязан быть предком нового (или тем же коммитом —
        // повторная установка того же tip безвредна и идемпотентна).
        if new_tip != old && !repo.graph_descendant_of(new_tip, old)? {
            return Err(MainUpdateError::NonFastForward);
        }
    }

    // Атомарная запись с тем же CAS на уровне libgit2: между проверкой выше и
    // записью мог вклиниться другой процесс — reference_matching это ловит.
    // Нулевой OID = «ref не должен существовать» (создание main при bootstrap).
    let expected = expected_old.unwrap_or(Oid::ZERO_SHA1);
    match repo.reference_matching(MAIN_REF, new_tip, true, expected, log_message) {
        Ok(_) => Ok(()),
        Err(e) if e.code() == ErrorCode::Modified => Err(MainUpdateError::Stale),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Каталог-однодневка: удаляется на Drop (в т.ч. при panic внутри теста).
    struct Tmp(std::path::PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn bare() -> (Tmp, Repository) {
        let p = std::env::temp_dir().join(format!("setfork-update-{}", uuid::Uuid::new_v4()));
        let repo = Repository::init_bare(&p).expect("init bare");
        (Tmp(p), repo)
    }

    fn sig() -> git2::Signature<'static> {
        git2::Signature::new("Тест", "t@example.com", &git2::Time::new(1_700_000_000, 0)).expect("sig")
    }

    /// Коммит с деревом {list.json} (или без канона) от заданных родителей; ref не
    /// двигает. `msg` обязан отличаться у «разных» коммитов: подпись и дата тут
    /// фиксированы, и одинаковые дерево+родители+сообщение дали бы ОДИН SHA.
    fn commit(repo: &Repository, with_canon: bool, parents: &[Oid], msg: &str) -> Oid {
        let mut tb = repo.treebuilder(None).expect("tb");
        if with_canon {
            let blob = repo.blob(b"{\"steps\":[]}").expect("blob");
            tb.insert("list.json", blob, 0o100644).expect("insert");
        } else {
            // Дерево без канона, но непустое — чтобы отличался SHA.
            let blob = repo.blob(b"# readme only").expect("blob");
            tb.insert("README.md", blob, 0o100644).expect("insert");
        }
        let tree = repo.find_tree(tb.write().expect("tree")).expect("find tree");
        let ps: Vec<git2::Commit> = parents.iter().map(|o| repo.find_commit(*o).expect("parent")).collect();
        let refs: Vec<&git2::Commit> = ps.iter().collect();
        let s = sig();
        repo.commit(None, &s, &s, msg, &tree, &refs).expect("commit")
    }

    fn main_at(repo: &Repository) -> Option<Oid> {
        repo.refname_to_id(MAIN_REF).ok()
    }

    #[test]
    fn создание_main_и_fast_forward_проходят() {
        let (_t, repo) = bare();
        let a = commit(&repo, true, &[], "a");
        update_main(&repo, a, None, "bootstrap").expect("создание");
        assert_eq!(main_at(&repo), Some(a));

        let b = commit(&repo, true, &[a], "b");
        update_main(&repo, b, Some(a), "append").expect("fast-forward");
        assert_eq!(main_at(&repo), Some(b));
    }

    /// Главное правило: программная запись НЕ обходит защиту канона.
    /// Коммит без list.json не встанет на main никаким путём.
    #[test]
    fn коммит_без_канона_отвергается() {
        let (_t, repo) = bare();
        let a = commit(&repo, true, &[], "a");
        update_main(&repo, a, None, "bootstrap").expect("создание");

        let bad = commit(&repo, false, &[a], "bad");
        let err = update_main(&repo, bad, Some(a), "should fail").expect_err("без канона");
        assert_eq!(err, MainUpdateError::MissingListJson);
        assert_eq!(main_at(&repo), Some(a), "main не тронут");
    }

    /// Переписывание истории (non-fast-forward) запрещено — как в pre-receive.
    #[test]
    fn non_fast_forward_отвергается() {
        let (_t, repo) = bare();
        let a = commit(&repo, true, &[], "a");
        update_main(&repo, a, None, "bootstrap").expect("создание");
        let b = commit(&repo, true, &[a], "b");
        update_main(&repo, b, Some(a), "append").expect("ff");

        // Боковая линия от a — не потомок b.
        let side = commit(&repo, true, &[a], "side");
        let err = update_main(&repo, side, Some(b), "rewrite").expect_err("non-ff");
        assert_eq!(err, MainUpdateError::NonFastForward);
        assert_eq!(main_at(&repo), Some(b), "main не тронут");
    }

    /// CAS: вызывающий думал, что main на a, а он уже на b → Stale, ничего не пишем.
    #[test]
    fn устаревший_ожидаемый_tip_даёт_stale() {
        let (_t, repo) = bare();
        let a = commit(&repo, true, &[], "a");
        update_main(&repo, a, None, "bootstrap").expect("создание");
        let b = commit(&repo, true, &[a], "b");
        update_main(&repo, b, Some(a), "append").expect("ff");

        let c = commit(&repo, true, &[a], "c-stale");
        let err = update_main(&repo, c, Some(a), "stale write").expect_err("stale");
        assert_eq!(err, MainUpdateError::Stale);
        assert_eq!(main_at(&repo), Some(b), "main не тронут");
    }

    /// Повторное «создание» при существующем main — тоже Stale (ref уже есть).
    #[test]
    fn повторное_создание_даёт_stale() {
        let (_t, repo) = bare();
        let a = commit(&repo, true, &[], "a");
        update_main(&repo, a, None, "bootstrap").expect("создание");

        let b = commit(&repo, true, &[], "b-root");
        let err = update_main(&repo, b, None, "second bootstrap").expect_err("ref уже есть");
        assert_eq!(err, MainUpdateError::Stale);
        assert_eq!(main_at(&repo), Some(a));
    }

    /// Идемпотентность: установка того же tip повторно — не ошибка.
    #[test]
    fn тот_же_tip_повторно_проходит() {
        let (_t, repo) = bare();
        let a = commit(&repo, true, &[], "a");
        update_main(&repo, a, None, "bootstrap").expect("создание");
        update_main(&repo, a, Some(a), "noop").expect("идемпотентно");
        assert_eq!(main_at(&repo), Some(a));
    }
}
