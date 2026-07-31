//! Материализация репо из истории версий (git2, шелл `git` для bundle/gc):
//! фиксированные автор/даты дают детерминированные SHA — golden-сверка
//! сравнивает их с inproc-реализацией фронта. Генерация файлов версии
//! (list.json/README/steps) — в соседнем serialize; типы ре-экспортируются
//! отсюда, чтобы исторические пути `bundle::VersionData` продолжали работать.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use git2::{ObjectType, Oid, Repository, Signature, Time};

use super::MAIN_REF;
use super::serialize::commit_message;
pub use super::serialize::{SerStep, StepRef, VersionData, version_files};
use super::update::{MainUpdateError, update_main};

// Идентичность коммитов — ОДИНАКОВО с TS (store.ts/bundle.ts) для детерминированных SHA.
// Переиспользуются merge-коммитами в services::git_core.
pub const AUTHOR_NAME: &str = "SetFork";
pub const AUTHOR_EMAIL: &str = "git@setfork.com";

fn run_git(args: &[&str]) -> io::Result<()> {
    let out = Command::new("git").args(args).output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn upd_io(e: MainUpdateError) -> io::Error {
    io::Error::other(e.to_string())
}

// Дерево версии из version_files (Ф2b: README.md, list.json, .gitattributes —
// плоский корень, steps/ из формата удалены). treebuilder.write() канонично
// сортирует записи — как git, поэтому SHA дерева совпадает.
fn build_tree(repo: &Repository, v: &VersionData) -> Result<Oid, git2::Error> {
    let mut root = repo.treebuilder(None)?;
    for (path, content) in version_files(v) {
        let blob = repo.blob(content.as_bytes())?;
        root.insert(path.as_str(), blob, 0o100644)?;
    }
    root.write()
}

// Один коммит версии с фиксированной идентичностью/датой (SHA-идентично `git commit`).
fn commit_version(repo: &Repository, parent: Option<Oid>, v: &VersionData) -> Result<Oid, git2::Error> {
    let tree = repo.find_tree(build_tree(repo, v)?)?;
    let sig = Signature::new(AUTHOR_NAME, AUTHOR_EMAIL, &Time::new(v.ts, 0))?; // offset 0 → +0000
    let msg = commit_message(v);
    let parents: Vec<git2::Commit> = match parent {
        Some(oid) => vec![repo.find_commit(oid)?],
        None => vec![],
    };
    let refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(None, &sig, &sig, &msg, &tree, &refs) // update_ref=None: main выставим в конце
}

// Строит историю версий: коммиты, refs/heads/main ЧЕРЕЗ update_main (та же
// валидация, что у pre-receive), затем теги vN + HEAD→main. Возвращает tip.
//
// Порядок намеренный: main двигается ДО тегов. Если бы теги ставились первыми,
// отказ update_main (например, CAS при гонке) оставил бы теги vN на осиротевших
// коммитах — и история версий начала бы врать.
fn build_history(
    repo: &Repository,
    versions: &[VersionData],
    mut parent: Option<Oid>,
) -> Result<Option<Oid>, MainUpdateError> {
    let expected_old = parent;
    let mut tagged: Vec<(i32, Oid)> = Vec::with_capacity(versions.len());
    for v in versions {
        let oid = commit_version(repo, parent, v)?;
        tagged.push((v.version, oid));
        parent = Some(oid);
    }
    if let Some(tip) = parent
        && parent != expected_old
    {
        update_main(repo, tip, expected_old, "setfork: versions")?;
        for (ver, oid) in tagged {
            let obj = repo.find_object(oid, Some(ObjectType::Commit))?;
            repo.tag_lightweight(&format!("v{ver}"), &obj, true)?;
        }
        let _ = repo.set_head(MAIN_REF);
    }
    Ok(parent)
}

/// Материализует историю версий в bare-репо (git2, детерминированные SHA) и возвращает путь.
/// ВЫЗЫВАЮЩИЙ обязан удалить каталог. Синхронно — вызывать через spawn_blocking.
pub fn materialize_repo(versions: &[VersionData]) -> io::Result<PathBuf> {
    if versions.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no versions"));
    }
    let work = std::env::temp_dir().join(format!("setfork-git-{}", uuid::Uuid::new_v4()));
    let build = (|| -> Result<(), MainUpdateError> {
        let repo = Repository::init_bare(&work)?;
        build_history(&repo, versions, None)?;
        Ok(())
    })();
    match build {
        Ok(()) => Ok(work),
        Err(e) => {
            let _ = fs::remove_dir_all(&work);
            Err(upd_io(e))
        }
    }
}

// pre-receive hook (порт store.ts PRE_RECEIVE): main защищён от удаления и
// non-fast-forward (канон версий; черновики force-push'абельны), каждый
// пушнутый коммит обязан нести list.json в корне, и состав дерева ограничен
// allowlist'ом (Ф0 трека git-surface; список путей — serialize::tree_path_allowed,
// правило одно на два пути записи).
//
// Состав проверяется у КАЖДОГО нового коммита, а не только у вершины: чистый tip
// пропускал бы мусор из промежуточных коммитов, а тот остаётся достижимым из
// истории и уезжает на зеркало.
//
// ⚠️ Перечисляем ПУТИ (`ls-tree -r --name-only` по коммиту), а НЕ объекты.
// Первая версия брала одну команду `rev-list --objects … --not --all`, и это была
// дыра (авто-ревью core#70, P1): `--objects` печатает каждый OID ОДИН раз, и если
// лишний файл содержит те же байты, что разрешённый, его имя не печатается вовсе.
// Проверено: два одинаковых файла `README.md` и `evil` дают в выводе только
// `README.md`, то есть `evil` проезжал незамеченным. `ls-tree -r` перечисляет все
// имена и заодно листает только листья — каталоги в выводе не появляются, а
// подмодули (gitlink) появляются и потому тоже судятся.
//
// Отказ ГОВОРЯЩИЙ и называет сами пути: линза 01 (ledger 28.07, «Угол 1»)
// показала, что лишний файл сегодня принимается и игнорируется без единого
// слова — человек узнаёт о потере, только если сам заметит. stderr хука
// доезжает до клиента строками `remote: …`.
//
// `</dev/null` у git-вызовов: stdin хука — это список рефов, который читает
// `while read`, и дочерний процесс не должен его подъедать.
const PRE_RECEIVE: &str = "#!/bin/sh\nzero=0000000000000000000000000000000000000000\nwhile read old new ref; do\n  if [ \"$ref\" = \"refs/heads/main\" ]; then\n    if [ \"$new\" = \"$zero\" ]; then\n      echo \"SetFork: ветка main защищена от удаления\" >&2\n      exit 1\n    fi\n    if [ \"$old\" != \"$zero\" ] && ! git merge-base --is-ancestor \"$old\" \"$new\"; then\n      echo \"SetFork: non-fast-forward push в main запрещён (перезапись истории)\" >&2\n      exit 1\n    fi\n  fi\n  case \"$new\" in *$zero) continue ;; esac\n  if ! git cat-file -e \"$new:list.json\" 2>/dev/null; then\n    echo \"SetFork: list.json is required at the repo root\" >&2\n    exit 1\n  fi\n  for c in $(git rev-list \"$new\" --not --all </dev/null); do\n    bad=$(git ls-tree -r --name-only \"$c\" </dev/null | grep -v -E '^(README\\.md|list\\.json|\\.gitattributes|steps/[^/]+\\.md)$' | sort -u | head -5)\n    if [ -n \"$bad\" ]; then\n      echo \"SetFork: в дереве списка разрешены только README.md, list.json и .gitattributes.\" >&2\n      echo \"Лишние пути (коммит $c):\" >&2\n      echo \"$bad\" | sed 's/^/  /' >&2\n      echo \"Уберите их из коммита: содержимое списка живёт в list.json.\" >&2\n      exit 1\n    fi\n  done\ndone\nexit 0\n";

/// Ставит pre-receive hook (защита main + list.json + состав дерева) и потолок
/// входящего пака; идемпотентно — обновления правил докатываются до старых репо.
pub fn install_hook(bare: &Path) -> io::Result<()> {
    let hooks = bare.join("hooks");
    fs::create_dir_all(&hooks)?;
    fs::write(hooks.join("pre-receive"), PRE_RECEIVE)?;
    // executable bit — только на unix; на Windows git-for-windows берёт хук через sh.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(hooks.join("pre-receive"), fs::Permissions::from_mode(0o755));
    }
    set_max_input_size(bare);
    Ok(())
}

/// Потолок входящего пака (`receive.maxInputSize`, SETFORK_MAX_PACK_MB, дефолт 16).
///
/// РОДНОЙ механизм git, а не самодельная проверка: receive-pack сверяет размер
/// потока и отказывает ДО распаковки, то есть мусорный пак не успевает стать
/// объектами на диске. Своя проверка после приёма такого свойства не даёт.
/// 0 = без ограничения (семантика самого git).
///
/// Это ДРУГАЯ граница, чем потолок gRPC-сообщения: та про транспорт ядро↔фронт,
/// эта — про продукт («какой пуш мы вообще готовы принять»).
fn set_max_input_size(bare: &Path) {
    let mb = std::env::var("SETFORK_MAX_PACK_MB").ok().and_then(|v| v.trim().parse::<u64>().ok());
    let bytes = mb.unwrap_or(16) * 1024 * 1024;
    let out = std::process::Command::new("git")
        .args(["--git-dir", &bare.to_string_lossy(), "config", "receive.maxInputSize", &bytes.to_string()])
        .output();
    // Обслуживание, не работа: не записалось — залогируем, но репо остаётся рабочим.
    match out {
        Ok(o) if !o.status.success() => {
            tracing::warn!(repo = %bare.display(), err = %String::from_utf8_lossy(&o.stderr),
                "receive.maxInputSize не выставлен");
        }
        Err(e) => tracing::warn!(repo = %bare.display(), error = %e, "receive.maxInputSize не выставлен"),
        _ => {}
    }
}

/// Размер bare-репо на диске, байты (рекурсивный обход). Нужен метрике и порогу
/// `SETFORK_REPO_LIMIT_MB`: per-push потолок не мешает вырастить репо серией
/// мелких пушей, а квоты размера у git нет вовсе — это уровень приложения
/// Так же устроено у GitLab: «Repository size limit» — настройка приложения
/// (инстанс/группа/проект), и при превышении пуш ОТКЛОНЯЕТСЯ
/// (docs.gitlab.com/administration/settings/account_and_limit_settings).
pub fn repo_size_bytes(bare: &Path) -> u64 {
    fn walk(dir: &Path, acc: &mut u64) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => walk(&e.path(), acc),
                Ok(t) if t.is_file() => *acc += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut total = 0;
    walk(bare, &mut total);
    total
}

/// `git gc --auto` на bare: упаковывает loose-объекты при превышении порога
/// gc.auto (иначе почти no-op). git2-запись версий/мержей не триггерит авто-gc
/// (в отличие от receive-pack/worktree-commit), поэтому зовём вручную после
/// материализации/дозаписи. Ошибки глушим — это обслуживание, не критично.
pub fn gc_auto(bare: &Path) {
    let _ = std::process::Command::new("git")
        .args(["--git-dir", &bare.to_string_lossy(), "gc", "--auto", "--quiet"])
        .status();
}

/// Бутстрап персистентного bare-репо из полной истории версий (git2) + pre-receive hook.
/// Больше нет temp-репо и `git clone --bare` — коммиты пишутся прямо в bare через git2.
pub fn bootstrap_bare(versions: &[VersionData], bare: &Path) -> io::Result<()> {
    if versions.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no versions"));
    }
    if let Some(parent) = bare.parent() {
        fs::create_dir_all(parent)?;
    }
    (|| -> Result<(), MainUpdateError> {
        let repo = Repository::init_bare(bare)?;
        build_history(&repo, versions, None)?;
        Ok(())
    })()
    .map_err(upd_io)?;
    install_hook(bare)?;
    gc_auto(bare); // упаковать объекты стартовой истории
    Ok(())
}

/// Дописывает версии поверх текущего main (git2), сохраняя запушенные коммиты.
/// `versions` — только те, что добавить (version > have). Без worktree.
/// Возвращает hex-sha нового tip main (None — дописывать было нечего).
///
/// Это НЕ «ленивая досыпка» (её больше нет): функцию зовут единый путь записи
/// версии (git::version::commit_web_version) и одноразовый догон sync-repos.
pub fn append_versions(bare: &Path, versions: &[VersionData]) -> io::Result<Option<String>> {
    if versions.is_empty() {
        return Ok(None);
    }
    let tip = (|| -> Result<Option<Oid>, MainUpdateError> {
        let repo = Repository::open_bare(bare)?;
        let parent = repo.refname_to_id(MAIN_REF).ok();
        build_history(&repo, versions, parent)
    })()
    .map_err(upd_io)?;
    gc_auto(bare); // loose-объекты дозаписанных версий → упаковка при пороге
    Ok(tip.map(|o| o.to_string()))
}

/// Максимальный номер версии среди тегов v* (git2).
pub fn max_tag_version(bare: &Path) -> i32 {
    let repo = match Repository::open_bare(bare) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let names = match repo.tag_names(Some("v*")) {
        Ok(n) => n,
        Err(_) => return 0,
    };
    let mut max = 0i32;
    // git2 0.21: iter() отдаёт Result (не-UTF8 имена больше не глотаются молча).
    for name in names.iter().flatten().flatten() {
        if let Some(num) = name.strip_prefix('v')
            && let Ok(n) = num.parse::<i32>()
        {
            max = max.max(n);
        }
    }
    max
}

/// Материализует репо (git2) и возвращает bundle всех рефов.
/// `git bundle` — через шелл (libgit2 не умеет формат bundle). Синхронно — через spawn_blocking.
pub fn build_bundle(versions: &[VersionData]) -> io::Result<Vec<u8>> {
    let work = materialize_repo(versions)?;
    let work_s = work.to_string_lossy().to_string();
    let bundle_path = std::env::temp_dir().join(format!("setfork-{}.bundle", uuid::Uuid::new_v4()));
    let bundle_s = bundle_path.to_string_lossy().to_string();

    let result = (|| -> io::Result<Vec<u8>> {
        run_git(&["-C", &work_s, "bundle", "create", &bundle_s, "--all"])?;
        fs::read(&bundle_path)
    })();

    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_file(&bundle_path);
    result
}
