//! Ф4: магический реф `refs/for/<base>` — «это не ветка, это предложение».
//!
//! # Зачем
//!
//! Терминал доезжал до git-объектов и обрывался: ветку запушил — а предложение
//! иди открывай в браузере. Приём взят у Gerrit: клиент пушит не в ветку, а в
//! `refs/for/main`, и это значит «предъявляю правку к main».
//!
//! Лицензионно чисто: ADR-0013 разрешает заимствование у Gerrit (Apache 2.0) —
//! берём идею, не код.
//!
//! # Почему не «любая ветка = предложение»
//!
//! Ветка это черновик. У человека должно остаться право запушить незаконченное,
//! не предъявляя его никому.
//!
//! # Куда кладутся коммиты
//!
//! В ветку автора `u/<handle>/<base>` — ОДНУ на пару (автор, база). Имя
//! детерминированное, и это не экономия, а механика ревизий: фронт держит одно
//! открытое предложение на ветку, поэтому повторный пуш в тот же магический реф
//! двигает ту же ветку и обновляет ТО ЖЕ предложение — как новый patchset у
//! Gerrit, но без Change-Id в сообщении коммита.
//!
//! Цена: одно предъявленное предложение на список от одного автора за раз. Кому
//! нужно второе — пушит обычную ветку и открывает предложение в интерфейсе.
//!
//! # Почему магический реф удаляется
//!
//! Если его оставить, второй пуш в `refs/for/main` окажется non-fast-forward и
//! git его отвергнет — то есть приём сработал бы ровно один раз.
use git2::Repository;

/// Префикс магических рефов.
pub const MAGIC_PREFIX: &str = "refs/for/";

/// Что сделал магический пуш: база, ветка автора и её новая вершина.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagicPush {
    pub base: String,
    pub branch: String,
    pub tip_sha: String,
}

/// Ветка автора для предложения к `base`.
///
/// `u/` — пространство пользователя: то же имя понадобится Ф5, где посторонним
/// разрешат писать только в свой префикс.
pub fn author_branch(handle: &str, base: &str) -> String {
    format!("u/{handle}/{base}")
}

/// Снимок магических рефов ДО приёма пака: имя → вершина.
///
/// Нужен, чтобы отличить «этот пуш создал» от «лежало раньше». Без снимка
/// разбор присваивал бы ЧУЖОЙ брошенный `refs/for/main` следующему пушащему, и
/// его коммиты уехали бы в чужое предложение (авто-ревью core#76, P1). Взяться
/// такому рефу есть откуда: до Ф4 хук пропускал произвольные не-main рефы, и
/// сервис мог упасть между приёмом пака и уборкой.
pub fn snapshot(repo: &Repository) -> Result<Vec<(String, git2::Oid)>, git2::Error> {
    let mut out = Vec::new();
    for name in repo.references_glob(&format!("{MAGIC_PREFIX}*"))?.names().flatten() {
        let name = name.to_string();
        if let Ok(oid) = repo.refname_to_id(&name) {
            out.push((name, oid));
        }
    }
    Ok(out)
}

/// Разбирает магические рефы после приёма пака: переносит вершину в ветку
/// автора и УДАЛЯЕТ сам магический реф.
///
/// Обрабатывает ТОЛЬКО то, что создал или сдвинул этот пуш (сверка с `before`).
/// Реф, лежавший до пуша с той же вершиной, — мусор от прошлого сбоя: его
/// удаляем, но НЕ присваиваем текущему автору. Тихо оставить его нельзя — он
/// присвоится следующему; приписать этому — значит подсунуть человеку чужие
/// коммиты в его предложение.
///
/// Пустой `handle` сюда не доходит — такой пуш отвергает хук, у которого есть
/// текст для человека; здесь это лишь страховка от вызова мимо хука.
pub fn take_magic_pushes(
    repo: &Repository,
    handle: &str,
    before: &[(String, git2::Oid)],
) -> Result<Vec<MagicPush>, git2::Error> {
    if handle.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    // Собираем имена ОТДЕЛЬНО от изменений: удалять рефы во время обхода
    // reference-итератора — напрашиваться на неопределённое поведение.
    let names: Vec<String> = repo
        .references_glob(&format!("{MAGIC_PREFIX}*"))?
        .names()
        .filter_map(|n| n.ok().map(|s| s.to_string()))
        .collect();

    for name in names {
        let Some(base) = name.strip_prefix(MAGIC_PREFIX) else { continue };
        let tip = repo.refname_to_id(&name)?;
        // Лежал до пуша и не двигался → не наш. Убираем мусор молча для клиента,
        // но громко для оператора: он значит, что кто-то падал на полпути.
        if before.iter().any(|(n, o)| n == &name && o == &tip) {
            tracing::warn!(ref_name = %name, "stale magic ref from an earlier failed push, removing");
            repo.find_reference(&name)?.delete()?;
            continue;
        }
        let branch = author_branch(handle, base);
        let target = format!("refs/heads/{branch}");
        // force = true: ветка автора обязана догонять предъявленное. Это не
        // потеря истории — предыдущая ревизия того же предложения уже уехала в
        // обсуждение, а ветка здесь указатель на «что предъявлено сейчас».
        repo.reference(&target, tip, true, "setfork: magic push")?;
        repo.find_reference(&name)?.delete()?;
        out.push(MagicPush { base: base.to_string(), branch, tip_sha: tip.to_string() });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(std::path::PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn repo_with_commit() -> (Tmp, Repository, git2::Oid) {
        let p = std::env::temp_dir().join(format!("setfork-magic-{}", uuid::Uuid::new_v4()));
        let repo = Repository::init_bare(&p).expect("init");
        let sig =
            git2::Signature::new("Test", "t@example.com", &git2::Time::new(1_700_000_000, 0)).expect("sig");
        // TreeBuilder заимствует repo — держим его в блоке, иначе repo не уедет
        // из функции (borrow жив до конца области видимости билдера).
        let oid = {
            let mut b = repo.treebuilder(None).expect("tb");
            let blob = repo.blob(b"{}").expect("blob");
            b.insert("list.json", blob, 0o100644).expect("insert");
            let tree = repo.find_tree(b.write().expect("write")).expect("tree");
            repo.commit(None, &sig, &sig, "c", &tree, &[]).expect("commit")
        };
        (Tmp(p), repo, oid)
    }

    #[test]
    fn магический_реф_переезжает_в_ветку_автора_и_исчезает() {
        let (_t, repo, oid) = repo_with_commit();
        repo.reference("refs/for/main", oid, true, "push").expect("magic ref");

        let got = take_magic_pushes(&repo, "mike", &[]).expect("разбор");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].base, "main");
        assert_eq!(got[0].branch, "u/mike/main");
        assert_eq!(got[0].tip_sha, oid.to_string());

        assert_eq!(repo.refname_to_id("refs/heads/u/mike/main").expect("ветка"), oid);
        // Магический реф обязан исчезнуть: иначе второй пуш в него будет
        // non-fast-forward, и приём сработает ровно один раз.
        assert!(repo.refname_to_id("refs/for/main").is_err(), "магический реф остался");
    }

    #[test]
    fn повторный_пуш_двигает_ту_же_ветку() {
        let (_t, repo, first) = repo_with_commit();
        repo.reference("refs/for/main", first, true, "push").expect("ref");
        take_magic_pushes(&repo, "mike", &[]).expect("первый");

        // Вторая ревизия того же предложения.
        let sig =
            git2::Signature::new("Test", "t@example.com", &git2::Time::new(1_700_000_100, 0)).expect("sig");
        let parent = repo.find_commit(first).expect("parent");
        let second =
            repo.commit(None, &sig, &sig, "c2", &parent.tree().expect("tree"), &[&parent]).expect("commit");
        // Снимок берётся ДО пуша — как в жизни (ядро делает его перед приёмом пака).
        let before = snapshot(&repo).expect("снимок до пуша");
        repo.reference("refs/for/main", second, true, "push").expect("ref");

        let got = take_magic_pushes(&repo, "mike", &before).expect("второй");
        assert_eq!(got.len(), 1, "одна запись");
        assert_eq!(got[0].branch, "u/mike/main", "та же ветка — значит то же предложение");
        assert_eq!(repo.refname_to_id("refs/heads/u/mike/main").expect("ветка"), second);
    }

    /// Регрессия P1 авто-ревью core#76: брошенный чужой реф не должен
    /// присваиваться следующему пушащему.
    #[test]
    fn чужой_брошенный_реф_не_присваивается() {
        let (_t, repo, oid) = repo_with_commit();
        repo.reference("refs/for/main", oid, true, "чужой пуш, сервис упал").expect("ref");
        // Снимок сделан ДО «нашего» пуша — значит реф не наш.
        let before = snapshot(&repo).expect("снимок");

        let got = take_magic_pushes(&repo, "anna", &before).expect("разбор");
        assert!(got.is_empty(), "чужие коммиты не попадают в предложение anna");
        assert!(repo.refname_to_id("refs/heads/u/anna/main").is_err(), "ветка anna не создана");
        // Мусор при этом убран: оставь его — присвоится следующему.
        assert!(repo.refname_to_id("refs/for/main").is_err(), "мусорный реф не убран");
    }

    #[test]
    fn у_разных_авторов_разные_ветки() {
        assert_eq!(author_branch("mike", "main"), "u/mike/main");
        assert_ne!(author_branch("mike", "main"), author_branch("anna", "main"));
    }

    #[test]
    fn без_ника_ничего_не_трогаем() {
        let (_t, repo, oid) = repo_with_commit();
        repo.reference("refs/for/main", oid, true, "push").expect("ref");
        assert!(take_magic_pushes(&repo, "", &[]).expect("пусто").is_empty());
        // Реф остался на месте: решение об отказе принимает хук, у которого есть
        // текст для человека, а не молчаливая уборка здесь.
        assert!(repo.refname_to_id("refs/for/main").is_ok());
    }
}
