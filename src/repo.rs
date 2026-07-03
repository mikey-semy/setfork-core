use crate::{bundle, db};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

// Корень персистентных bare-репо. ОБЯЗАТЕЛЕН и общий с фронтом (GIT_DATA_DIR):
// у Rust и Next разный cwd, поэтому дефолт `cwd/.setfork-git` не годится — только явный путь.
fn root() -> PathBuf {
    PathBuf::from(
        std::env::var("GIT_DATA_DIR").expect("GIT_DATA_DIR не задан (общий с фронтом том git-объектов)"),
    )
}
fn repo_path(id: Uuid) -> PathBuf {
    root().join(format!("{}.git", id))
}

// Пер-репо async-лок. В пределах процесса Rust; когда SETFORK_CORE_URL включён,
// git-репозиториями владеет Rust (Next их не трогает) — этого достаточно.
fn locks() -> &'static StdMutex<HashMap<Uuid, Arc<AsyncMutex<()>>>> {
    static L: OnceLock<StdMutex<HashMap<Uuid, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    L.get_or_init(|| StdMutex::new(HashMap::new()))
}
pub async fn repo_lock(id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
    let m = {
        let mut g = locks().lock().unwrap();
        g.entry(id).or_insert_with(|| Arc::new(AsyncMutex::new(()))).clone()
    };
    m.lock_owned().await
}

fn join_err<E: std::fmt::Display>(e: E) -> sqlx::Error {
    sqlx::Error::Protocol(e.to_string())
}

/// Гарантирует персистентный bare-репозиторий, синхронный с историей версий (порт store.ts ensureRepo).
/// git-объекты — источник правды (пуш сохраняется), веб-версии дописываются лениво поверх.
/// Возвращает (путь, template_id) или None (списка нет).
pub async fn ensure_repo(pool: &PgPool, owner: &str, slug: &str) -> Result<Option<(PathBuf, Uuid)>, sqlx::Error> {
    let Some((id, current_version)) = db::resolve_list(pool, owner, slug).await? else {
        return Ok(None);
    };
    let bare = repo_path(id);
    let _guard = repo_lock(id).await;

    if !bare.exists() {
        // bootstrap из полной истории
        let versions = db::load_bundle_data(pool, id).await?;
        if versions.is_empty() {
            return Ok(None);
        }
        let bare2 = bare.clone();
        tokio::task::spawn_blocking(move || bundle::bootstrap_bare(&versions, &bare2))
            .await
            .map_err(join_err)?
            .map_err(join_err)?;
        return Ok(Some((bare, id)));
    }

    // репо есть → дописать недостающие веб-версии (сохраняя запушенные коммиты)
    let bare_tag = bare.clone();
    let have = tokio::task::spawn_blocking(move || bundle::max_tag_version(&bare_tag))
        .await
        .map_err(join_err)?;
    if current_version > have {
        let versions: Vec<_> = db::load_bundle_data(pool, id)
            .await?
            .into_iter()
            .filter(|v| v.version > have)
            .collect();
        if !versions.is_empty() {
            let bare2 = bare.clone();
            tokio::task::spawn_blocking(move || bundle::append_versions(&bare2, &versions))
                .await
                .map_err(join_err)?
                .map_err(join_err)?;
        }
    }
    Ok(Some((bare, id)))
}

/// Bundle из персистентного репо (включая запушенные коммиты) — порт store.ts bundleRepo.
pub async fn bundle_repo(pool: &PgPool, owner: &str, slug: &str) -> Result<Option<Vec<u8>>, sqlx::Error> {
    let Some((bare, _id)) = ensure_repo(pool, owner, slug).await? else {
        return Ok(None);
    };
    let data = tokio::task::spawn_blocking(move || bundle_all(&bare))
        .await
        .map_err(join_err)?
        .map_err(join_err)?;
    Ok(Some(data))
}

fn bundle_all(bare: &Path) -> std::io::Result<Vec<u8>> {
    let bare_s = bare.to_string_lossy().to_string();
    let out = std::env::temp_dir().join(format!("setfork-{}.bundle", Uuid::new_v4()));
    let out_s = out.to_string_lossy().to_string();
    let status = std::process::Command::new("git")
        .args(["-C", &bare_s, "bundle", "create", &out_s, "--all"])
        .output()?;
    if !status.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            String::from_utf8_lossy(&status.stderr).to_string(),
        ));
    }
    let data = std::fs::read(&out);
    let _ = std::fs::remove_file(&out);
    data
}
