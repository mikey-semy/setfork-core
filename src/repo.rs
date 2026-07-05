use crate::{bundle, db};
use sqlx::{PgPool, Postgres, Transaction};
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

// Пер-репо async-лок В ПРЕДЕЛАХ ПРОЦЕССА — быстрый путь, чтобы конкурентные
// задачи одного инстанса не держали по соединению каждая, а выстраивались тут.
fn locks() -> &'static StdMutex<HashMap<Uuid, Arc<AsyncMutex<()>>>> {
    static L: OnceLock<StdMutex<HashMap<Uuid, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    L.get_or_init(|| StdMutex::new(HashMap::new()))
}
async fn repo_lock(id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
    let m = {
        let mut g = locks().lock().unwrap();
        g.entry(id).or_insert_with(|| Arc::new(AsyncMutex::new(()))).clone()
    };
    m.lock_owned().await
}

// Ключ advisory-лока: 64-битный срез UUID. Коллизии крайне редки и безвредны
// (два разных репо изредка сериализуются — лишняя очередь, но не порча данных).
fn advisory_key(id: Uuid) -> i64 {
    let b = id.as_bytes();
    i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Лок репозитория: внутрипроцессный mutex + транзакционный advisory-лок Postgres.
/// Освобождается при drop гарда (в т.ч. при ошибке/панике) — транзакция откатывается,
/// `pg_advisory_xact_lock` снимается. Защищает от гонок между НЕСКОЛЬКИМИ инстансами
/// core (одного in-process mutex для этого было мало).
pub struct RepoGuard {
    _proc: tokio::sync::OwnedMutexGuard<()>,
    _tx: Transaction<'static, Postgres>,
}

pub async fn repo_guard(pool: &PgPool, id: Uuid) -> Result<RepoGuard, sqlx::Error> {
    let proc = repo_lock(id).await; // сначала выстраиваемся внутри процесса
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(advisory_key(id))
        .execute(&mut *tx)
        .await?;
    Ok(RepoGuard { _proc: proc, _tx: tx })
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
    let _guard = repo_guard(pool, id).await?;

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

    // репо есть → освежаем pre-receive hook (идемпотентно; так обновления правил
    // докатываются и до уже существующих на диске репо), затем дописываем версии.
    let bare_hook = bare.clone();
    tokio::task::spawn_blocking(move || bundle::install_hook(&bare_hook))
        .await
        .map_err(join_err)?
        .map_err(join_err)?;
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

#[cfg(test)]
mod tests {
    use super::advisory_key;
    use uuid::Uuid;

    #[test]
    fn advisory_key_stable_and_distinct() {
        let a = Uuid::from_u128(0x1234_5678_9abc_def0_1122_3344_5566_7788);
        let b = Uuid::from_u128(0x0fed_cba9_8765_4321_8877_6655_4433_2211);
        assert_eq!(advisory_key(a), advisory_key(a), "same UUID → same key");
        assert_ne!(advisory_key(a), advisory_key(b), "different UUIDs → different keys");
    }
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
