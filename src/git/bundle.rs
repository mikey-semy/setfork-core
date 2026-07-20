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

fn git_io(e: git2::Error) -> io::Error {
    io::Error::other(format!("git2: {e}"))
}

// Дерево версии из version_files (README.md, list.json, steps/NN.md); поддерево steps/.
// treebuilder.write() канонично сортирует записи — как git, поэтому SHA дерева совпадает.
fn build_tree(repo: &Repository, v: &VersionData) -> Result<Oid, git2::Error> {
    let mut root = repo.treebuilder(None)?;
    let mut steps = repo.treebuilder(None)?;
    let mut has_steps = false;
    for (path, content) in version_files(v) {
        let blob = repo.blob(content.as_bytes())?;
        if let Some(name) = path.strip_prefix("steps/") {
            steps.insert(name, blob, 0o100644)?;
            has_steps = true;
        } else {
            root.insert(path.as_str(), blob, 0o100644)?;
        }
    }
    if has_steps {
        let steps_oid = steps.write()?;
        root.insert("steps", steps_oid, 0o040000)?;
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

// Строит историю версий: коммиты + теги vN + refs/heads/main + HEAD→main. Возвращает tip.
fn build_history(
    repo: &Repository,
    versions: &[VersionData],
    mut parent: Option<Oid>,
) -> Result<Option<Oid>, git2::Error> {
    for v in versions {
        let oid = commit_version(repo, parent, v)?;
        let obj = repo.find_object(oid, Some(ObjectType::Commit))?;
        repo.tag_lightweight(&format!("v{}", v.version), &obj, true)?;
        parent = Some(oid);
    }
    if let Some(tip) = parent {
        repo.reference(MAIN_REF, tip, true, "setfork")?;
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
    let build = (|| -> Result<(), git2::Error> {
        let repo = Repository::init_bare(&work)?;
        build_history(&repo, versions, None)?;
        Ok(())
    })();
    match build {
        Ok(()) => Ok(work),
        Err(e) => {
            let _ = fs::remove_dir_all(&work);
            Err(git_io(e))
        }
    }
}

// pre-receive hook (порт store.ts PRE_RECEIVE): main защищён от удаления и
// non-fast-forward (канон версий; черновики force-push'абельны), плюс каждый
// пушнутый коммит обязан нести list.json в корне.
const PRE_RECEIVE: &str = "#!/bin/sh\nzero=0000000000000000000000000000000000000000\nwhile read old new ref; do\n  if [ \"$ref\" = \"refs/heads/main\" ]; then\n    if [ \"$new\" = \"$zero\" ]; then\n      echo \"SetFork: ветка main защищена от удаления\" >&2\n      exit 1\n    fi\n    if [ \"$old\" != \"$zero\" ] && ! git merge-base --is-ancestor \"$old\" \"$new\"; then\n      echo \"SetFork: non-fast-forward push в main запрещён (перезапись истории)\" >&2\n      exit 1\n    fi\n  fi\n  case \"$new\" in *$zero) continue ;; esac\n  if ! git cat-file -e \"$new:list.json\" 2>/dev/null; then\n    echo \"SetFork: list.json is required at the repo root\" >&2\n    exit 1\n  fi\ndone\nexit 0\n";

/// Ставит pre-receive hook (защита main + обязательный list.json); идемпотентно.
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
    Ok(())
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
    (|| -> Result<(), git2::Error> {
        let repo = Repository::init_bare(bare)?;
        build_history(&repo, versions, None)?;
        Ok(())
    })()
    .map_err(git_io)?;
    install_hook(bare)?;
    gc_auto(bare); // упаковать объекты стартовой истории
    Ok(())
}

/// Дописывает недостающие веб-версии поверх текущего main (git2), сохраняя запушенные коммиты.
/// `versions` — только те, что добавить (version > have). Без worktree.
pub fn append_versions(bare: &Path, versions: &[VersionData]) -> io::Result<()> {
    if versions.is_empty() {
        return Ok(());
    }
    (|| -> Result<(), git2::Error> {
        let repo = Repository::open_bare(bare)?;
        let parent = repo.refname_to_id(MAIN_REF).ok();
        build_history(&repo, versions, parent)?;
        Ok(())
    })()
    .map_err(git_io)?;
    gc_auto(bare); // loose-объекты дозаписанных версий → упаковка при пороге
    Ok(())
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
