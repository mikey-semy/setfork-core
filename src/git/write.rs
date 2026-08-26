//! Запись list.json в ветку одним коммитом.
//!
//! Отдельный модуль, а не тело gRPC-метода — по той же причине, что и `history`:
//! это git-логика, и проверять её надо на настоящем репо, а не через сервис с
//! пулом БД. Служит «предложенным правкам»: рецензент даёт готовый текст пункта,
//! автор жмёт «Применить», и правка ложится в ветку без локального клона.
//!
//! Отличие от слияния (`merge_resolved`): пишем в ВЕТКУ, main не двигается,
//! родитель один — значит и проекции версии здесь нет. Версии рождаются только
//! из main; ветка остаётся черновиком.

/// Чем закончилась запись.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Коммит создан; sha — новый tip ветки.
    Committed(String),
    /// Содержимое совпало с текущим — коммита нет, sha прежний.
    Unchanged(String),
}

/// Почему записать не удалось. Отражается в gRPC-статусы вызывающим.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// Ветки нет (могли удалить, пока человек смотрел на дифф).
    NotFound,
    /// Ветку подвинули с момента чтения — писать поверх нельзя.
    Stale,
    /// Ошибка git2 (текст для журнала, наружу не показываем).
    Git(String),
}

impl From<git2::Error> for WriteError {
    fn from(e: git2::Error) -> Self {
        WriteError::Git(e.to_string())
    }
}

/// Положить `list_json` в `branch` одним коммитом поверх её tip.
///
/// `expected_tip` — оптимистичная блокировка: вызывающий читал ветку, показал
/// человеку дифф и вернулся с решением, а за это время в ветку мог прийти пуш.
/// Без сверки мы бы молча его перезаписали. Пустая строка — не сверять.
///
/// Одинаковое содержимое коммитом НЕ становится: пустые коммиты замусоривают
/// вкладку «Коммиты» и сбивают счётчик вклада ветки.
pub fn commit_list_json(
    repo: &git2::Repository,
    branch: &str,
    list_json: &[u8],
    message: &str,
    expected_tip: &str,
    author: Option<(&str, &str)>,
) -> Result<WriteOutcome, WriteError> {
    let branch_ref = format!("refs/heads/{branch}");
    let tip = repo.refname_to_id(&branch_ref).map_err(|_| WriteError::NotFound)?;
    if !expected_tip.is_empty() && expected_tip != tip.to_string() {
        return Err(WriteError::Stale);
    }
    let parent = repo.find_commit(tip)?;
    let tree = parent.tree()?;
    if let Ok(entry) = tree.get_path(std::path::Path::new("list.json"))
        && let Ok(blob) = repo.find_blob(entry.id())
        && blob.content() == list_json
    {
        return Ok(WriteOutcome::Unchanged(tip.to_string()));
    }

    let blob = repo.blob(list_json)?;
    let mut tb = repo.treebuilder(Some(&tree))?;
    tb.insert("list.json", blob, 0o100644)?;
    // Витрина обязана соответствовать канону (Ф2b): раньше README тащился старым
    // блобом и протухал до следующей веб-версии. Битый канон README не трогает —
    // но наши пути пишут только собранный ядром, он всегда разбирается.
    if let Some(readme) = super::project::readme_from_canon(list_json) {
        let rb = repo.blob(readme.as_bytes())?;
        tb.insert("README.md", rb, 0o100644)?;
    }
    let ab = repo.blob(super::serialize::GITATTRIBUTES.as_bytes())?;
    tb.insert(".gitattributes", ab, 0o100644)?;
    // steps/ — материализация старой формы (удалена из формата в Ф2b); у старых
    // деревьев каталог вычищается при первой же записи.
    if tb.get("steps")?.is_some() {
        tb.remove("steps")?;
    }
    let tree = repo.find_tree(tb.write()?)?;
    // Авторство человека, если его передали: иначе вкладка «Коммиты» показала бы
    // служебного автора там, где правку применил пользователь.
    let sig = match author {
        Some((name, email)) => git2::Signature::now(name, email)?,
        None => git2::Signature::now(super::bundle::AUTHOR_NAME, super::bundle::AUTHOR_EMAIL)?,
    };
    let msg = if message.trim().is_empty() { "Apply suggested edit" } else { message.trim() };
    let oid = repo.commit(Some(&branch_ref), &sig, &sig, msg, &tree, &[&parent])?;
    // Упаковка — и на ЭТОМ пути тоже. git2-запись авто-gc не триггерит (в отличие от
    // receive-pack), а веточные правки объекты создают: замер 25.08 — 4 loose-объекта
    // на коммит, 165 после сорока правок. Пока репо получает ещё и версии, их пакует
    // досыпка; репо, живущее ОДНИМИ предложениями, не паковалось бы никогда.
    // `gc --auto` ниже порога — почти no-op, поэтому цена вызова нулевая.
    super::bundle::gc_auto(repo.path());
    Ok(WriteOutcome::Committed(oid.to_string()))
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
        let p = std::env::temp_dir().join(format!("setfork-write-{}", uuid::Uuid::new_v4()));
        let repo = git2::Repository::init_bare(&p).expect("init bare");
        (Tmp(p), repo)
    }

    fn sig() -> git2::Signature<'static> {
        git2::Signature::new("Кто-то", "s@example.com", &git2::Time::new(1_700_000_000, 0)).expect("sig")
    }

    /// Ветка с list.json заданного содержимого (и, по желанию, каталогом steps/).
    fn seed(repo: &git2::Repository, branch: &str, list: &[u8], with_steps: bool) -> git2::Oid {
        let blob = repo.blob(list).expect("blob");
        let mut tb = repo.treebuilder(None).expect("tb");
        tb.insert("list.json", blob, 0o100644).expect("insert");
        if with_steps {
            let inner = repo.blob(b"# step").expect("blob");
            let mut sub = repo.treebuilder(None).expect("sub");
            sub.insert("1.md", inner, 0o100644).expect("insert md");
            let sub = sub.write().expect("write sub");
            tb.insert("steps", sub, 0o040000).expect("insert steps");
        }
        let tree = repo.find_tree(tb.write().expect("write tree")).expect("tree");
        let s = sig();
        repo.commit(Some(&format!("refs/heads/{branch}")), &s, &s, "seed", &tree, &[]).expect("commit")
    }

    /// Веточная запись ПАКУЕТ объекты, а не копит их вечно.
    ///
    /// git2-запись авто-gc не триггерит (в отличие от receive-pack), и на этом пути `gc`
    /// не звался вовсе: замер линзы 06 §4 — 4 loose-объекта на коммит, 165 после сорока
    /// правок, ни одного пака. Пока репо получает ещё и версии, его пакует досыпка; репо,
    /// живущее ОДНИМИ предложениями, не паковалось бы никогда.
    ///
    /// ⚠️ Условие подобрано под НАСТОЯЩУЮ эвристику git, и подбиралось ЗАМЕРОМ, а не
    /// догадкой — три раза подряд догадка оказывалась неверной:
    /// первое — `gc --auto` считает loose-объекты только в ОДНОЙ выборочной папке
    /// `objects/17` и умножает на 256, поэтому низкий порог сам по себе ничего не даёт;
    /// второе — сравнение СТРОГОЕ, и одного объекта в этой папке не хватает;
    /// третье — `gc.autoDetach` по умолчанию истина, gc уходит в фон, и синхронная
    /// проверка была бы гонкой (в проде фон как раз уместен).
    /// Плюс: gc пакует только ДОСТИЖИМОЕ, поэтому наполнители роли пака не играют —
    /// пакуется дерево ветки.
    #[test]
    fn branch_write_packs_objects() {
        let (tmp, repo) = bare();
        {
            // `gc.autoDetach` по умолчанию ИСТИНА: `gc --auto` уходит в фон и
            // возвращается сразу, поэтому синхронная проверка «пак появился» была бы
            // гонкой. В проде фон — то, что нужно; в пробе нужен синхронный прогон.
            let mut cfg = repo.config().expect("config");
            cfg.set_i32("gc.auto", 1).expect("порог");
            cfg.set_bool("gc.autoDetach", false).expect("без фона");
        }
        seed(&repo, "work", br#"{"title":"a","steps":[]}"#, false);

        // Наполняем выборочную папку эвристики. Порог сравнивается СТРОГО, поэтому
        // одного объекта мало — нужно больше, чем `(gc.auto + 255) / 256`.
        let сколько_в_17 = || std::fs::read_dir(tmp.0.join("objects/17")).map(|d| d.count()).unwrap_or(0);
        let mut попыток = 0;
        while сколько_в_17() < 3 {
            repo.blob(format!("наполнитель {попыток}").as_bytes()).expect("blob");
            попыток += 1;
            assert!(попыток < 8000, "не удалось наполнить objects/17 — эвристика git изменилась?");
        }

        let паков = || {
            std::fs::read_dir(tmp.0.join("objects/pack"))
                .map(|d| {
                    d.filter_map(Result::ok)
                        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
                        .count()
                })
                .unwrap_or(0)
        };
        assert_eq!(паков(), 0, "до правки паков нет");

        commit_list_json(&repo, "work", br#"{"title":"b","steps":[]}"#, "правка", "", None).expect("коммит");

        assert!(паков() > 0, "объекты обязаны упаковаться: без вызова gc на этом пути пака не будет");
    }

    fn list_json_at(repo: &git2::Repository, sha: &str) -> Vec<u8> {
        let commit = repo.find_commit(git2::Oid::from_str(sha).expect("oid")).expect("commit");
        let entry =
            commit.tree().expect("tree").get_path(std::path::Path::new("list.json")).expect("list.json");
        repo.find_blob(entry.id()).expect("blob").content().to_vec()
    }

    #[test]
    fn writes_a_commit_and_moves_the_branch() {
        let (_t, repo) = bare();
        let before = seed(&repo, "pr-1", b"{\"steps\":[]}", false);

        let out =
            commit_list_json(&repo, "pr-1", b"{\"steps\":[1]}", "Применить правку", "", None).expect("write");

        let WriteOutcome::Committed(sha) = out else { panic!("ожидался коммит: {out:?}") };
        assert_ne!(sha, before.to_string(), "ветка должна сдвинуться");
        assert_eq!(repo.refname_to_id("refs/heads/pr-1").expect("tip").to_string(), sha);
        assert_eq!(list_json_at(&repo, &sha), b"{\"steps\":[1]}");
        // Родитель один: это не слияние, main не участвует.
        let commit = repo.find_commit(git2::Oid::from_str(&sha).expect("oid")).expect("commit");
        assert_eq!(commit.parent_count(), 1);
        assert_eq!(commit.message().unwrap_or_default(), "Применить правку");
    }

    #[test]
    fn identical_content_creates_no_empty_commit() {
        let (_t, repo) = bare();
        let before = seed(&repo, "pr-1", b"{\"steps\":[]}", false);

        let out = commit_list_json(&repo, "pr-1", b"{\"steps\":[]}", "ничего не менялось", "", None)
            .expect("write");

        assert_eq!(out, WriteOutcome::Unchanged(before.to_string()));
        assert_eq!(repo.refname_to_id("refs/heads/pr-1").expect("tip"), before, "ветка не двигается");
    }

    #[test]
    fn a_foreign_push_is_not_overwritten() {
        let (_t, repo) = bare();
        let tip = seed(&repo, "pr-1", b"{\"steps\":[]}", false);
        let stale = "0".repeat(40);

        let err = commit_list_json(&repo, "pr-1", b"{\"steps\":[9]}", "поверх чужого", &stale, None)
            .expect_err("должно отказать");

        assert_eq!(err, WriteError::Stale);
        assert_eq!(repo.refname_to_id("refs/heads/pr-1").expect("tip"), tip, "ветка нетронута");
    }

    #[test]
    fn matching_tip_skips_the_write() {
        let (_t, repo) = bare();
        let tip = seed(&repo, "pr-1", b"{\"steps\":[]}", false).to_string();

        let out = commit_list_json(&repo, "pr-1", b"{\"steps\":[7]}", "по актуальному tip", &tip, None)
            .expect("write");

        assert!(matches!(out, WriteOutcome::Committed(_)));
    }

    #[test]
    fn steps_dir_follows_the_canon_when_removed() {
        let (_t, repo) = bare();
        seed(&repo, "pr-1", b"{\"steps\":[]}", true);

        let out = commit_list_json(&repo, "pr-1", b"{\"steps\":[2]}", "", "", None).expect("write");

        let WriteOutcome::Committed(sha) = out else { panic!("ожидался коммит") };
        let tree = repo.find_commit(git2::Oid::from_str(&sha).expect("oid")).expect("c").tree().expect("t");
        assert!(
            tree.get_path(std::path::Path::new("steps")).is_err(),
            "steps/ разошёлся бы с list.json — его быть не должно"
        );
    }

    #[test]
    fn human_authorship_reaches_the_commit() {
        let (_t, repo) = bare();
        seed(&repo, "pr-1", b"{\"steps\":[]}", false);

        let out =
            commit_list_json(&repo, "pr-1", b"{\"steps\":[3]}", "", "", Some(("Мика", "m@example.com")))
                .expect("write");

        let WriteOutcome::Committed(sha) = out else { panic!("ожидался коммит") };
        let commit = repo.find_commit(git2::Oid::from_str(&sha).expect("oid")).expect("commit");
        assert_eq!(commit.author().email().expect("email"), "m@example.com");
    }

    /// Ф2b: витрина не протухает — после записи канона в ветку README и
    /// .gitattributes соответствуют новому list.json, steps/ вычищен.
    #[test]
    fn readme_is_regenerated_with_the_canon() {
        let (_t, repo) = bare();
        seed(&repo, "pr-1", b"{\"steps\":[]}", true);

        let canon = crate::git::serialize::list_json(&crate::git::bundle::VersionData {
            version: 2,
            note: String::new(),
            ts: 0,
            title: "Fresh title".into(),
            desc: String::new(),
            tags: vec![],
            ordered: true,
            kind: None,
            steps: vec![],
        });
        let out = commit_list_json(&repo, "pr-1", canon.as_bytes(), "", "", None).expect("write");
        let WriteOutcome::Committed(sha) = out else { panic!("ожидался коммит") };
        let tree = repo.find_commit(git2::Oid::from_str(&sha).unwrap()).unwrap().tree().unwrap();

        let readme_entry = tree.get_path(std::path::Path::new("README.md")).expect("README есть");
        let readme =
            String::from_utf8(repo.find_blob(readme_entry.id()).unwrap().content().to_vec()).unwrap();
        assert!(readme.contains("# Fresh title"), "витрина из НОВОГО канона: {readme}");
        assert_eq!(
            readme,
            crate::git::project::readme_from_canon(canon.as_bytes()).expect("канон читается"),
            "README в дереве — ровно перегенерированный из канона"
        );
        let ga = tree.get_path(std::path::Path::new(".gitattributes")).expect(".gitattributes есть");
        let ga = String::from_utf8(repo.find_blob(ga.id()).unwrap().content().to_vec()).unwrap();
        assert!(ga.contains("linguist-generated"), "{ga}");
        assert!(tree.get_path(std::path::Path::new("steps")).is_err(), "steps/ вычищен");
    }

    #[test]
    fn missing_branch_is_not_found() {
        let (_t, repo) = bare();
        seed(&repo, "pr-1", b"{}", false);

        let err = commit_list_json(&repo, "pr-нет", b"{\"a\":1}", "", "", None).expect_err("нет ветки");

        assert_eq!(err, WriteError::NotFound);
    }
}
