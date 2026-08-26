//! Сборка коммита слияния: подпись, соавторы, разрешённый конфликт.
//!
//! Отдельно от транспорта потому, что здесь решается, ЧТО останется в истории
//! навсегда: подпись коммита и список соавторов переписать потом нельзя.

use tonic::{Code, Status};

use super::main_status;
use crate::git::update::update_main;
use crate::git::{MAIN_REF, bundle, project, serialize};
use crate::reason::{self, Reason};
use crate::services::util::internal;

/**
 * Сообщение squash-коммита с трейлерами `Co-authored-by`.
 *
 * При squash история ветки в main не попадает, поэтому авторство её коммитов
 * иначе исчезло бы совсем — а это единственная запись о том, кто на самом деле
 * делал работу. GitHub решает ровно так же.
 *
 * Авторы берутся из коммитов ВКЛАДА ветки (то, чего нет в main), без дублей и в
 * порядке появления. Ошибку обхода глушим: слияние не должно падать из-за
 * украшения сообщения.
 */
pub fn with_coauthors(
    repo: &git2::Repository,
    branch_tip: git2::Oid,
    main_tip: git2::Oid,
    title: &str,
) -> String {
    let mut seen: Vec<String> = Vec::new();
    if let Ok(mut walk) = repo.revwalk() {
        let _ = walk.push(branch_tip);
        let _ = walk.hide(main_tip);
        for oid in walk.flatten() {
            let Ok(c) = repo.find_commit(oid) else { continue };
            let a = c.author();
            // Подпись может быть не-UTF8 — такую пропускаем, а не падаем.
            let (Ok(n), Ok(e)) = (a.name(), a.email()) else { continue };
            let (n, e) = (n.trim().to_string(), e.trim().to_string());
            // Пустое имя или почта дали бы ломаную строку «Co-authored-by:  <>»,
            // которую git трейлером не считает, а человек читает как мусор.
            if n.is_empty() || e.is_empty() {
                continue;
            }
            // Служебная подпись самого сервиса соавторством не является.
            // Регистр не важен: почта регистронезависима, и «GIT@SetFork.com»
            // — та же служебная подпись, а не соавтор.
            if e.eq_ignore_ascii_case(bundle::AUTHOR_EMAIL) {
                continue;
            }
            let line = format!("Co-authored-by: {n} <{e}>");
            if !seen.contains(&line) {
                seen.push(line);
            }
        }
    }
    if seen.is_empty() {
        return title.to_string();
    }
    // Пустая строка перед трейлерами обязательна: иначе git не считает их
    // трейлерами, и `git interpret-trailers` их не увидит.
    format!(
        "{title}

{}",
        seen.join(
            "
"
        )
    )
}

// Подпись merge-коммитов — та же идентичность, что у детерминированных коммитов bundle.
pub(super) fn merge_sig() -> Result<git2::Signature<'static>, Status> {
    git2::Signature::now(bundle::AUTHOR_NAME, bundle::AUTHOR_EMAIL).map_err(internal)
}

/// Коммит ручного резолва: дерево main с заменённым `list.json` и БЕЗ `steps/`
/// (md-оверрайды сбрасываются — канон разрешённых шагов один, см. proto).
///
/// `squash` определяет РОДИТЕЛЕЙ, а не дерево: дерево здесь всегда одно — то,
/// что человек разрешил руками. При squash родитель один (main), и история
/// ветки в main не уезжает; вклад авторов сохраняется трейлерами, как в
/// merge_branch. Раньше режима не было вовсе, и фронт на remote-пути просто
/// ОТКАЗЫВАЛ в резолве squash-списков, выдавая отказ за 'conflict'.
///
/// Вынесено из RPC отдельной функцией, чтобы поведение проверялось тестами на
/// настоящем git-репо, без Postgres и транспорта.
pub(super) fn commit_resolved(
    repo: &git2::Repository,
    branch: &str,
    list_json: &[u8],
    squash: bool,
    message: &str,
) -> Result<String, Status> {
    let branch_tip = repo
        .refname_to_id(&format!("refs/heads/{branch}"))
        .map_err(|_| reason::status(Code::NotFound, Reason::NotFound, "branch not found"))?;
    let main_tip = repo.refname_to_id(MAIN_REF).map_err(internal)?;
    if main_tip == branch_tip {
        return Err(reason::status(
            Code::FailedPrecondition,
            Reason::NothingToMerge,
            "branch is not ahead of base",
        ));
    }
    let ours = repo.find_commit(main_tip).map_err(internal)?;
    let theirs = repo.find_commit(branch_tip).map_err(internal)?;
    let blob = repo.blob(list_json).map_err(internal)?;
    let mut tb = repo.treebuilder(Some(&ours.tree().map_err(internal)?)).map_err(internal)?;
    tb.insert("list.json", blob, 0o100644).map_err(internal)?;
    // Витрина соответствует канону (Ф2b): README перегенерируется из нового
    // list.json — раньше он тащился старым блобом и протухал до веб-версии.
    if let Some(readme) = project::readme_from_canon(list_json) {
        let rb = repo.blob(readme.as_bytes()).map_err(internal)?;
        tb.insert("README.md", rb, 0o100644).map_err(internal)?;
    }
    let ab = repo.blob(serialize::GITATTRIBUTES.as_bytes()).map_err(internal)?;
    tb.insert(".gitattributes", ab, 0o100644).map_err(internal)?;
    if tb.get("steps").map_err(internal)?.is_some() {
        tb.remove("steps").map_err(internal)?;
    }
    let tree_id = tb.write().map_err(internal)?;
    let tree = repo.find_tree(tree_id).map_err(internal)?;
    let sig = merge_sig()?;

    let (msg, parents): (String, Vec<&git2::Commit>) = if squash {
        let title = if message.trim().is_empty() {
            format!("Squashed branch '{branch}'")
        } else {
            message.trim().to_string()
        };
        (with_coauthors(repo, branch_tip, main_tip, &title), vec![&ours])
    } else {
        (format!("Merge branch '{branch}' (resolved)"), vec![&ours, &theirs])
    };
    // Коммит без ref-обновления; main двигает ТОЛЬКО update_main (валидация как у pre-receive).
    let merged = repo.commit(None, &sig, &sig, &msg, &tree, &parents).map_err(internal)?;
    update_main(repo, merged, Some(main_tip), &format!("merge {branch}: resolved")).map_err(main_status)?;
    // Упаковка и на пути СЛИЯНИЯ: git2-запись авто-gc не триггерит, а merge-коммит с
    // деревьями объекты создаёт. Ниже порога `gc --auto` почти no-op, поэтому цена
    // вызова нулевая, а без него репо, живущее одними предложениями и слияниями,
    // не паковалось бы никогда (замер линзы 06 §4).
    crate::git::bundle::gc_auto(repo.path());
    Ok(merged.to_string())
}

#[cfg(test)]
mod squash_tests {
    use super::{commit_resolved, with_coauthors};

    /// Каталог-однодневка: удаляется на Drop (в т.ч. при panic внутри теста).
    struct Tmp(std::path::PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn bare() -> (Tmp, git2::Repository) {
        let p = std::env::temp_dir().join(format!("setfork-squash-{}", uuid::Uuid::new_v4()));
        let repo = git2::Repository::init_bare(&p).expect("init bare");
        (Tmp(p), repo)
    }

    /// Пустой коммит от заданного автора на ref.
    fn commit(
        repo: &git2::Repository,
        refname: &str,
        msg: &str,
        who: (&str, &str),
        parents: &[git2::Oid],
    ) -> git2::Oid {
        let tree = repo.treebuilder(None).expect("tb").write().expect("tree");
        let tree = repo.find_tree(tree).expect("find tree");
        let sig = git2::Signature::new(who.0, who.1, &git2::Time::new(1_700_000_000, 0)).expect("sig");
        let ps: Vec<git2::Commit> = parents.iter().map(|o| repo.find_commit(*o).expect("parent")).collect();
        let refs: Vec<&git2::Commit> = ps.iter().collect();
        repo.commit(Some(refname), &sig, &sig, msg, &tree, &refs).expect("commit")
    }

    /**
     * При squash история ветки в main не попадает, поэтому Co-authored-by —
     * ЕДИНСТВЕННАЯ запись о том, кто делал работу. Ошибка тут молча стирает
     * авторство.
     */
    #[test]
    fn trailers_come_from_branch_contribution_without_duplicates() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("Мика", "m@example.com"), &[]);
        let a = commit(&repo, "refs/heads/pr", "первый", ("Аня", "a@example.com"), &[base]);
        let b = commit(&repo, "refs/heads/pr", "второй", ("Аня", "a@example.com"), &[a]);
        let c = commit(&repo, "refs/heads/pr", "третий", ("Боря", "b@example.com"), &[b]);

        let msg = with_coauthors(&repo, c, base, "Заголовок");

        assert!(msg.starts_with("Заголовок\n\n"), "трейлеры отделены пустой строкой: {msg:?}");
        assert_eq!(msg.matches("Co-authored-by: Аня <a@example.com>").count(), 1, "дубли схлопнуты");
        assert!(msg.contains("Co-authored-by: Боря <b@example.com>"));
        // Автор коммита ИЗ MAIN не соавтор этой правки.
        assert!(!msg.contains("m@example.com"), "автор базы не должен попасть в соавторы");
    }

    #[test]
    fn the_service_signature_is_not_counted_as_co_authorship() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("Мика", "m@example.com"), &[]);
        let a = commit(
            &repo,
            "refs/heads/pr",
            "авто",
            (crate::git::bundle::AUTHOR_NAME, crate::git::bundle::AUTHOR_EMAIL),
            &[base],
        );

        let msg = with_coauthors(&repo, a, base, "Заголовок");

        assert_eq!(msg, "Заголовок", "нечего приписывать — заголовок остаётся как есть");
    }

    #[test]
    fn a_branch_without_own_commits_adds_nothing() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("Мика", "m@example.com"), &[]);
        assert_eq!(with_coauthors(&repo, base, base, "Заголовок"), "Заголовок");
    }

    // ── Перенос покрытия из TS (coauthors.test.ts) перед Ф0b ────────────────
    // Сверка реализаций показала, что TS фильтровал больше: пустые подписи и
    // регистр служебной почты. Здесь это чинится в ядре — оно остаётся одно.

    /// Подпись с пробелами по краям не должна давать кривой трейлер.
    ///
    /// Пустое имя/почту здесь не проверить: git2 отказывается создавать такую
    /// подпись вовсе («Signature cannot have an empty name or email»), и обычным
    /// путём такой коммит не появится. Отбраковка пустых в `with_coauthors`
    /// оставлена как защита от коммитов, приехавших пушем из импортированных
    /// репозиториев (формат коммита сам по себе `author  <>` допускает).
    #[test]
    fn a_signature_with_edge_spaces_is_trimmed() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("SetFork", "git@setfork.com"), &[]);
        let a = commit(&repo, "refs/heads/pr", "a", ("  Гость  ", " g@example.com "), &[base]);

        let msg = with_coauthors(&repo, a, base, "Заголовок");
        assert!(msg.contains("Co-authored-by: Гость <g@example.com>"), "подпись обрезана: {msg}");
        assert!(!msg.contains("  Гость"), "лишние пробелы не доехали: {msg}");
    }

    /// Почта регистронезависима: «GIT@SetFork.com» — та же служебная подпись.
    #[test]
    fn the_service_email_is_recognized_in_any_case() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("SetFork", "git@setfork.com"), &[]);
        let a = commit(&repo, "refs/heads/pr", "a", ("SetFork", "GIT@SetFork.COM"), &[base]);
        assert_eq!(with_coauthors(&repo, a, base, "Заголовок"), "Заголовок");
    }

    /// Перед трейлерами обязана быть пустая строка — иначе git не считает их
    /// трейлерами и `git interpret-trailers` их не видит.
    #[test]
    fn an_empty_line_precedes_the_trailers() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("SetFork", "git@setfork.com"), &[]);
        let a = commit(&repo, "refs/heads/pr", "a", ("Гость", "g@example.com"), &[base]);
        let msg = with_coauthors(&repo, a, base, "Заголовок");
        assert_eq!(msg, "Заголовок\n\nCo-authored-by: Гость <g@example.com>");
    }

    // ── Ручной резолв конфликта: режим слияния ───────────────────────────────
    // Раньше режима не было, и фронт на remote-пути (а это ПРОД) отказывал в
    // резолве squash-списков, выдавая отказ за 'conflict'.

    /// main + ветка с одним своим коммитом от стороннего автора.
    fn main_and_branch(repo: &git2::Repository) -> (git2::Oid, git2::Oid) {
        let base = commit(repo, "refs/heads/main", "base", ("SetFork", "git@setfork.com"), &[]);
        let theirs = commit(repo, "refs/heads/pr-1", "их правка", ("Гость", "guest@example.com"), &[base]);
        // main уходит вперёд — иначе это не расхождение, а перемотка.
        commit(repo, "refs/heads/main", "наша правка", ("SetFork", "git@setfork.com"), &[base]);
        (repo.refname_to_id("refs/heads/main").expect("main"), theirs)
    }

    fn commit_at<'a>(repo: &'a git2::Repository, sha: &str) -> git2::Commit<'a> {
        repo.find_commit(git2::Oid::from_str(sha).expect("oid")).expect("commit")
    }

    #[test]
    fn squash_resolve_yields_one_parent_and_trailers() {
        let (_t, repo) = bare();
        main_and_branch(&repo);
        let sha = commit_resolved(&repo, "pr-1", br#"{"steps":[]}"#, true, "Свели руками").expect("резолв");
        let c = commit_at(&repo, &sha);
        assert_eq!(c.parent_count(), 1, "squash не тянет историю ветки в main");
        let msg = c.message().expect("сообщение");
        assert!(msg.starts_with("Свели руками"), "заголовок из запроса: {msg}");
        assert!(msg.contains("Co-authored-by: Гость <guest@example.com>"), "авторство ветки: {msg}");
        // Разрешённый канон на месте, steps/ сброшены.
        let tree = c.tree().expect("дерево");
        assert!(tree.get_path(std::path::Path::new("list.json")).is_ok());
        assert!(tree.get_path(std::path::Path::new("steps")).is_err());
    }

    #[test]
    fn squash_resolve_without_a_message_takes_the_branch_name() {
        let (_t, repo) = bare();
        main_and_branch(&repo);
        let sha = commit_resolved(&repo, "pr-1", br#"{"steps":[]}"#, true, "   ").expect("резолв");
        assert!(commit_at(&repo, &sha).message().expect("msg").starts_with("Squashed branch 'pr-1'"));
    }

    #[test]
    fn plain_merge_resolve_yields_two_parents() {
        let (_t, repo) = bare();
        main_and_branch(&repo);
        let sha = commit_resolved(&repo, "pr-1", br#"{"steps":[]}"#, false, "").expect("резолв");
        let c = commit_at(&repo, &sha);
        assert_eq!(c.parent_count(), 2, "обычный резолв сохраняет обе линии");
        assert_eq!(c.message().expect("msg"), "Merge branch 'pr-1' (resolved)");
    }

    #[test]
    fn resolve_moves_main_and_knows_a_missing_branch() {
        let (_t, repo) = bare();
        main_and_branch(&repo);
        let sha = commit_resolved(&repo, "pr-1", br#"{"steps":[]}"#, true, "x").expect("резолв");
        assert_eq!(repo.refname_to_id(super::MAIN_REF).expect("main").to_string(), sha, "main переехал");

        let err = commit_resolved(&repo, "нет-такой", br#"{"steps":[]}"#, true, "x").expect_err("нет ветки");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    /// Ф2b: ручной резолв оставляет витрину свежей — README из нового канона.
    #[test]
    fn resolve_regenerates_readme_from_the_canon() {
        let (_t, repo) = bare();
        main_and_branch(&repo);
        let canon = crate::git::serialize::list_json(&crate::git::bundle::VersionData {
            version: 3,
            note: String::new(),
            ts: 0,
            title: "Resolved title".into(),
            desc: String::new(),
            tags: vec![],
            ordered: true,
            kind: None,
            steps: vec![],
        });
        let sha = commit_resolved(&repo, "pr-1", canon.as_bytes(), true, "x").expect("резолв");
        let tree = commit_at(&repo, &sha).tree().expect("tree");
        let readme = tree.get_path(std::path::Path::new("README.md")).expect("README есть");
        let readme =
            String::from_utf8(repo.find_blob(readme.id()).expect("blob").content().to_vec()).expect("utf8");
        assert!(readme.contains("# Resolved title"), "витрина из нового канона: {readme}");
        assert!(tree.get_path(std::path::Path::new(".gitattributes")).is_ok());
    }

    #[test]
    fn resolving_a_branch_on_the_same_commit_has_nothing_to_merge() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("SetFork", "git@setfork.com"), &[]);
        repo.reference("refs/heads/pr-1", base, true, "ветка на main").expect("ref");
        let err = commit_resolved(&repo, "pr-1", br#"{"steps":[]}"#, false, "").expect_err("nothing");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        // Сверяем ПРИЧИНУ, а не текст (И1): текст — для логов и человека, его
        // можно менять свободно; контракт с клиентом держит трейлер.
        assert_eq!(
            err.metadata().get(crate::reason::REASON_KEY).and_then(|v| v.to_str().ok()),
            Some("NOTHING_TO_MERGE")
        );
    }
}
