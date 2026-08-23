//! Персистентные bare-репо (GIT_DATA_DIR, общий с фронтом том): bootstrap из
//! истории версий при первом касании/восстановлении и пер-репо локи
//! (in-process mutex + PG advisory).
//!
//! Ленивой досыпки веб-версий здесь больше НЕТ (Ф1): версии рождаются
//! git-first в git::version::commit_web_version, читающие пути ничего не
//! дописывают. Отставшие репо выравнивает одноразовый `sync-repos` (main.rs)
//! и защитное предусловие пути записи (git::version::sync_repo_with_db).
use crate::db;
use crate::git::bundle;
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::{HashMap, HashSet};
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
/// Путь bare-репо списка. pub — нужен CLI sync-repos и git-first пути записи.
pub fn repo_path(id: Uuid) -> PathBuf {
    root().join(format!("{}.git", id))
}

/// Каталоги репозиториев на томе, которым не соответствует ни один список.
///
/// Правило намеренно УЗКОЕ: берём только имена вида `<uuid>.git`, всё остальное в
/// каталоге данных не наше и не наше дело. Вызывающий обязан передать НЕПУСТОЙ
/// набор живых списков — пустой набор значит «база не та», а не «списков нет».
pub fn orphan_repo_dirs(root: &std::path::Path, live: &HashSet<Uuid>) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".git"))
            .and_then(|stem| Uuid::parse_str(stem).ok())
        else {
            continue;
        };
        if !live.contains(&id) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

// Пер-репо async-лок В ПРЕДЕЛАХ ПРОЦЕССА — быстрый путь, чтобы конкурентные
// задачи одного инстанса не держали по соединению каждая, а выстраивались тут.
fn locks() -> &'static StdMutex<HashMap<Uuid, Arc<AsyncMutex<()>>>> {
    static L: OnceLock<StdMutex<HashMap<Uuid, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    L.get_or_init(|| StdMutex::new(HashMap::new()))
}
async fn repo_lock(id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
    let m = {
        // Паника под локом не должна отравлять реестр навсегда (иначе один сбой
        // роняет ВСЕ последующие git-запросы) — данные внутри валидны, забираем как есть.
        let mut g = locks().lock().unwrap_or_else(|p| p.into_inner());
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

// Мини-пул под advisory-локи (db::connect_lock_pool, ставит main на старте
// сервера): гард держит соединение на всё время git-операции — из ОБЩЕГО пула
// это выедало по соединению на push (аудит 2026-07-20, P1-5: ~5 одновременных
// пушей при PGPOOL_MAX=10 исчерпывали пул). Не задан (CLI/тесты) → основной пул.
static LOCK_POOL: OnceLock<PgPool> = OnceLock::new();

pub fn set_lock_pool(pool: PgPool) {
    let _ = LOCK_POOL.set(pool);
}

pub async fn repo_guard(pool: &PgPool, id: Uuid) -> Result<RepoGuard, sqlx::Error> {
    let proc = repo_lock(id).await; // сначала выстраиваемся внутри процесса
    let lock_pool = LOCK_POOL.get().unwrap_or(pool);
    let mut tx = lock_pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(advisory_key(id)).execute(&mut *tx).await?;
    Ok(RepoGuard { _proc: proc, _tx: tx })
}

pub(crate) fn join_err<E: std::fmt::Display>(e: E) -> sqlx::Error {
    sqlx::Error::Protocol(e.to_string())
}

/// Гарантирует персистентный bare-репозиторий: bootstrap из истории БД, если
/// репо нет на диске (первое касание или восстановление тома), освежение
/// pre-receive hook и ВЫРАВНИВАНИЕ с БД (git::version::sync_repo_with_db).
///
/// Выравнивание — это НЕ прежняя «ленивая досыпка»: с Ф1 версии попадают в git
/// в момент создания (git-first), и на выровненном репо здесь нечего делать.
/// Оно срабатывает только на деградированных состояниях — легаси-хвостах до
/// одноразового `sync-repos` и хвостах сбоя проекции. Без него отставшее
/// легаси-репо принимало бы push поверх УСТАРЕВШЕГО main и молча затирало
/// веб-версии, которые в старом мире сделали бы такой push честным
/// non-fast-forward-отказом.
/// Возвращает (путь, template_id) или None (списка нет).
pub async fn ensure_repo(
    pool: &PgPool,
    owner: &str,
    slug: &str,
) -> Result<Option<(PathBuf, Uuid)>, sqlx::Error> {
    let Some((id, _current_version)) = db::resolve_list(pool, owner, slug).await? else {
        return Ok(None);
    };
    Ok(ensure_repo_by_id(pool, id).await?.map(|bare| (bare, id)))
}

/// То же по template_id (git-first путь записи знает id, а не owner/slug).
pub async fn ensure_repo_by_id(pool: &PgPool, id: Uuid) -> Result<Option<PathBuf>, sqlx::Error> {
    let bare = repo_path(id);
    let _guard = repo_guard(pool, id).await?;

    if !bare.exists() {
        // bootstrap из полной истории (детерминированные SHA — см. bundle.rs)
        let versions = match db::load_bundle_data(pool, id).await {
            Ok(v) => v,
            Err(sqlx::Error::RowNotFound) => return Ok(None), // списка нет
            Err(e) => return Err(e),
        };
        if versions.is_empty() {
            // Список существует, а строк истории ещё нет (current_version — дефолт
            // колонки). Это легальное состояние: старый drizzle-путь addVersion
            // принимал такую «первую» версию, и молча ужесточить контракт значило
            // сломать вызывающих (итесты фронта поймали ровно это). Репо рождается
            // ПУСТЫМ: первый addVersion создаст первый коммит через update_main
            // (создание main, expected_old = None).
            let bare2 = bare.clone();
            tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                git2::Repository::init_bare(&bare2).map_err(|e| std::io::Error::other(e.to_string()))?;
                bundle::install_hook(&bare2)
            })
            .await
            .map_err(join_err)?
            .map_err(join_err)?;
            return Ok(Some(bare));
        }
        let bare2 = bare.clone();
        tokio::task::spawn_blocking(move || bundle::bootstrap_bare(&versions, &bare2))
            .await
            .map_err(join_err)?
            .map_err(join_err)?;
        return Ok(Some(bare));
    }

    // Репо есть → освежаем pre-receive hook (идемпотентно; так обновления правил
    // докатываются и до уже существующих на диске репо).
    let bare_hook = bare.clone();
    tokio::task::spawn_blocking(move || bundle::install_hook(&bare_hook))
        .await
        .map_err(join_err)?
        .map_err(join_err)?;

    // Выравнивание деградированных состояний (см. док-коммент ensure_repo).
    // Ошибка выравнивания чтение не роняет: устаревшее репо читаемо, а Conflict
    // уже громко залогирован внутри sync; запись остановит своё предусловие.
    if let Err(e) = super::version::sync_repo_with_db(pool, id, &bare).await {
        tracing::error!(%id, error = %e, "repo/db alignment failed (reads continue)");
    }
    Ok(Some(bare))
}

/// Bundle из персистентного репо (включая запушенные коммиты) — порт store.ts bundleRepo.
pub async fn bundle_repo(pool: &PgPool, owner: &str, slug: &str) -> Result<Option<Vec<u8>>, sqlx::Error> {
    let Some((bare, _id)) = ensure_repo(pool, owner, slug).await? else {
        return Ok(None);
    };
    let data =
        tokio::task::spawn_blocking(move || bundle_all(&bare)).await.map_err(join_err)?.map_err(join_err)?;
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
        return Err(std::io::Error::other(String::from_utf8_lossy(&status.stderr).to_string()));
    }
    let data = std::fs::read(&out);
    let _ = std::fs::remove_file(&out);
    data
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

#[cfg(test)]
mod orphan_tests {
    use super::*;

    /// Правило отбора «лишних» репозиториев проверяется здесь, а не только глазами в
    /// CLI: команда УДАЛЯЕТ данные, и ошибка в имени каталога стоила бы истории
    /// живого списка.
    #[test]
    fn берём_только_репо_несуществующих_списков() {
        let root = std::env::temp_dir().join(format!("sf-orphan-{}", Uuid::new_v4()));
        let live_id = Uuid::new_v4();
        let dead_id = Uuid::new_v4();
        for name in [
            format!("{live_id}.git"),
            format!("{dead_id}.git"),
            "не-uuid.git".to_string(), // чужой каталог — не наше дело
            format!("{dead_id}"),      // без .git — тоже не наше
            "README".to_string(),
        ] {
            std::fs::create_dir_all(root.join(name)).expect("mkdir");
        }
        let live: HashSet<Uuid> = [live_id].into_iter().collect();

        let orphans = orphan_repo_dirs(&root, &live).expect("обход");

        assert_eq!(orphans.len(), 1, "лишним признан ровно один каталог: {orphans:?}");
        assert!(orphans[0].ends_with(format!("{dead_id}.git")));
        std::fs::remove_dir_all(&root).ok();
    }
}
