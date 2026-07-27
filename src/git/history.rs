//! Чтение истории коммитов bare-репо.
//!
//! Отдельный модуль, а не тело gRPC-метода: обход revwalk — это git-логика,
//! которую надо проверять на настоящем репо, а не через сервис с пулом БД.
//! Служит вкладке «Коммиты» у правки — что именно принесла ветка.

/// Коммит в форме, не зависящей от git2 (owned-данные, пересекают spawn_blocking).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub sha: String,
    /// Полное сообщение; первая строка — заголовок.
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    /// Время автора, секунды unix.
    pub at_unix: i64,
    /// 2 и больше — merge-коммит.
    pub parents: i32,
}

/// Коммиты `rev`, свежие первыми, не более `take`.
///
/// `not_in` (обычно `main`) скрывает всё, что и так есть в базе, — остаётся
/// ровно вклад ветки. Несуществующий `rev` — это `None`, а не ошибка: ветку
/// могли удалить, и UI показывает пусто, а не 500.
pub fn commits(
    repo: &git2::Repository,
    rev: &str,
    not_in: &str,
    take: usize,
) -> Result<Option<Vec<CommitInfo>>, git2::Error> {
    let Ok(head) = repo.revparse_single(rev).and_then(|o| o.peel_to_commit()) else {
        return Ok(None);
    };
    let mut walk = repo.revwalk()?;
    walk.set_sorting(git2::Sort::TIME)?;
    walk.push(head.id())?;
    if !not_in.is_empty() {
        // База может отсутствовать (свежий репо без main) — тогда просто не скрываем.
        if let Ok(base) = repo.revparse_single(not_in).and_then(|o| o.peel_to_commit()) {
            walk.hide(base.id())?;
        }
    }
    let mut out: Vec<CommitInfo> = Vec::new();
    for oid in walk.take(take) {
        let c = repo.find_commit(oid?)?;
        let author = c.author();
        out.push(CommitInfo {
            sha: c.id().to_string(),
            message: c.message().unwrap_or("").to_string(),
            author_name: author.name().unwrap_or("").to_string(),
            author_email: author.email().unwrap_or("").to_string(),
            at_unix: author.when().seconds(),
            parents: c.parent_count() as i32,
        });
    }
    Ok(Some(out))
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

    fn bare() -> (Tmp, git2::Repository) {
        let p = std::env::temp_dir().join(format!("setfork-hist-{}", uuid::Uuid::new_v4()));
        let repo = git2::Repository::init_bare(&p).expect("init bare");
        (Tmp(p), repo)
    }

    /// Пустой коммит с заданным сообщением на ref `refname`.
    fn commit(repo: &git2::Repository, refname: &str, msg: &str, parents: &[git2::Oid]) -> git2::Oid {
        let tree = repo.treebuilder(None).expect("tb").write().expect("write tree");
        let tree = repo.find_tree(tree).expect("find tree");
        let sig =
            git2::Signature::new("Кто-то", "s@example.com", &git2::Time::new(1_700_000_000, 0)).expect("sig");
        let parents: Vec<git2::Commit> =
            parents.iter().map(|o| repo.find_commit(*o).expect("parent")).collect();
        let refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some(refname), &sig, &sig, msg, &tree, &refs).expect("commit")
    }

    #[test]
    fn branch_commits_exclude_base() {
        let (_tmp, repo) = bare();
        let c1 = commit(&repo, "refs/heads/main", "первый", &[]);
        let c2 = commit(&repo, "refs/heads/main", "второй", &[c1]);
        let c3 = commit(&repo, "refs/heads/feature", "правка", &[c2]);
        let c4 = commit(&repo, "refs/heads/feature", "ещё правка", &[c3]);

        // Вклад ветки — только её коммиты, база не протекает.
        let got = commits(&repo, "feature", "main", 100).unwrap().expect("found");
        assert_eq!(
            got.iter().map(|c| c.sha.as_str()).collect::<Vec<_>>(),
            vec![c4.to_string(), c3.to_string()]
        );
        assert_eq!(got[0].message, "ещё правка");
        assert_eq!(got[0].author_name, "Кто-то");
        assert_eq!(got[0].at_unix, 1_700_000_000);
        assert_eq!(got[0].parents, 1);

        // Без not_in — вся история рефа.
        let all = commits(&repo, "feature", "", 100).unwrap().expect("found");
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn missing_rev_is_none_and_limit_holds() {
        let (_tmp, repo) = bare();
        let c1 = commit(&repo, "refs/heads/main", "первый", &[]);
        commit(&repo, "refs/heads/main", "второй", &[c1]);

        // Удалённая ветка — не ошибка: вкладка покажет пусто.
        assert!(commits(&repo, "deleted-branch", "", 10).unwrap().is_none());
        assert_eq!(commits(&repo, "main", "", 1).unwrap().expect("found").len(), 1);
    }

    #[test]
    fn merge_commit_reports_two_parents() {
        let (_tmp, repo) = bare();
        let c1 = commit(&repo, "refs/heads/main", "база", &[]);
        let side = commit(&repo, "refs/heads/side", "сбоку", &[c1]);
        let merge = commit(&repo, "refs/heads/main", "merge side", &[c1, side]);

        let got = commits(&repo, "main", "", 100).unwrap().expect("found");
        assert_eq!(got[0].sha, merge.to_string());
        assert_eq!(got[0].parents, 2);
    }
}
