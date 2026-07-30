//! Git-first создание версии (Ф1, единый путь записи) и выравнивание репо с БД.
//!
//! Целевая модель: любая правка СНАЧАЛА становится коммитом main, потом
//! проецируется в Postgres как read-model. Коммиты — события, версии — снимки,
//! теги vN — указатели; Postgres можно пересобрать из git (reproject), git из
//! Postgres не откатывается никогда.
//!
//! Порядок durable-записи: git-коммит фиксируется ДО коммита транзакции БД.
//! Строки версии пишутся в ещё открытую транзакцию (откат при сбое git
//! бесплатен), а сбой БД ПОСЛЕ git-коммита — «канон впереди проекции»:
//! громкий лог + метрика, восстановление — штатный rebuild `reproject`.
use std::path::Path;

use sqlx::PgPool;
use uuid::Uuid;

use crate::db::{self, StepRow};
use crate::git::bundle::{self, VersionData};
use crate::git::{MAIN_REF, project, repo::join_err};

/// Чем закончилось выравнивание репо с БД (sync-repos / предусловие записи).
#[derive(Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Тег vN совпадает с current_version — делать нечего.
    InSync,
    /// Репо не было на диске — материализовано из истории БД целиком.
    Bootstrapped { versions: usize },
    /// Git отставал от БД (наследие ленивой досыпки) — версии дописаны.
    Appended { from: i32, to: i32 },
    /// Git был на одну версию впереди (сбой прошлой проекции) — tip спроецирован в БД.
    ProjectedTip { version: i32 },
    /// Расхождение, которое чинится только руками (посторонний тег vN и т.п.) —
    /// см. runbook git-projection-catchup.
    Conflict { have: i32, current: i32 },
}

/// Выравнивает репо списка с БД. Вызывать ПОД репо-локом (`repo::repo_guard`).
///
/// Ветки:
/// * репо нет → bootstrap всей истории из БД (восстановление/первое касание);
/// * `max_tag < current` → дописать недостающие версии (одноразовый долг
///   ленивой досыпки; после миграции штатно возникать не должен);
/// * `max_tag == current + 1` и тег стоит на tip main → спроецировать tip
///   (хвост записи, у которой git успел, а БД нет);
/// * иначе → `Conflict`: скорее всего посторонний тег `v<число>` (до починки
///   #59 релиз мог занять имя версии) — руками по runbook.
pub async fn sync_repo_with_db(pool: &PgPool, id: Uuid, bare: &Path) -> Result<SyncOutcome, sqlx::Error> {
    let current: Option<i32> = sqlx::query_scalar("select current_version from templates where id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some(current) = current else {
        return Err(sqlx::Error::RowNotFound);
    };

    if !bare.exists() {
        let versions = db::load_bundle_data(pool, id).await?;
        if versions.is_empty() {
            return Ok(SyncOutcome::InSync); // материализовать нечего
        }
        let n = versions.len();
        let bare2 = bare.to_path_buf();
        tokio::task::spawn_blocking(move || bundle::bootstrap_bare(&versions, &bare2))
            .await
            .map_err(join_err)?
            .map_err(join_err)?;
        return Ok(SyncOutcome::Bootstrapped { versions: n });
    }

    let bare_tag = bare.to_path_buf();
    let have =
        tokio::task::spawn_blocking(move || bundle::max_tag_version(&bare_tag)).await.map_err(join_err)?;

    if have == current {
        return Ok(SyncOutcome::InSync);
    }

    if have < current {
        let versions: Vec<_> =
            db::load_bundle_data(pool, id).await?.into_iter().filter(|v| v.version > have).collect();
        let count = versions.len() as u64;
        if count == 0 {
            // Версии в БД числятся (current), а строк истории нет — рассинхрон данных.
            return Ok(SyncOutcome::Conflict { have, current });
        }
        let bare2 = bare.to_path_buf();
        tokio::task::spawn_blocking(move || bundle::append_versions(&bare2, &versions))
            .await
            .map_err(join_err)?
            .map_err(join_err)?;
        metrics::counter!("repo_catchup_versions_total").increment(count);
        tracing::warn!(%id, from = have + 1, to = current, "git отставал от БД — версии дописаны (догон)");
        return Ok(SyncOutcome::Appended { from: have + 1, to: current });
    }

    // have > current: либо хвост незавершённой записи (ровно на 1, тег на tip),
    // либо посторонний тег vN / глубокое расхождение.
    if have == current + 1 {
        let bare_chk = bare.to_path_buf();
        let tag_on_tip = tokio::task::spawn_blocking(move || -> bool {
            let Ok(repo) = git2::Repository::open_bare(&bare_chk) else { return false };
            let (Ok(tag), Ok(tip)) =
                (repo.refname_to_id(&format!("refs/tags/v{have}")), repo.refname_to_id(MAIN_REF))
            else {
                return false;
            };
            tag == tip
        })
        .await
        .map_err(join_err)?;
        if tag_on_tip && let Some(v) = project::project_pushed_commit(pool, id, bare).await? {
            tracing::warn!(%id, version = v, "git был впереди БД на одну версию — tip спроецирован (heal)");
            return Ok(SyncOutcome::ProjectedTip { version: v });
        }
    }

    metrics::counter!("version_tag_conflicts_total").increment(1);
    tracing::error!(
        %id, have, current,
        "тег v{have} выше текущей версии {current} и это не хвост записи: имя вида v<число> занято \
         НЕ версией либо история разошлась — запись остановлена, см. runbook git-projection-catchup"
    );
    Ok(SyncOutcome::Conflict { have, current })
}

/// Итог git-first записи версии.
#[derive(Debug)]
pub struct WebVersion {
    pub ver_id: Uuid,
    pub version: i32,
    pub commit_sha: String,
    pub created_at_ms: i64,
}

/// Почему версия не записана (или записана не до конца).
#[derive(Debug)]
pub enum WebVersionError {
    /// Списка нет.
    NotFound,
    /// Репо и БД разошлись так, что чинить руками (см. sync_repo_with_db).
    OutOfSync { have: i32, current: i32 },
    /// Сбой БД ДО git-коммита — ничего не записано.
    Db(sqlx::Error),
    /// Сбой git-коммита — транзакция БД откачена, ничего не записано.
    Git(String),
    /// Git-коммит vN записан, а транзакция БД не зафиксировалась: канон впереди
    /// проекции. Данные НЕ потеряны — восстановление: `reproject <owner> <slug>`.
    ProjectionLost { version: i32, sha: String, source: sqlx::Error },
}

impl From<sqlx::Error> for WebVersionError {
    fn from(e: sqlx::Error) -> Self {
        WebVersionError::Db(e)
    }
}

/// Создаёт версию git-first: коммит vN на main (через единую точку обновления,
/// внутри `bundle::append_versions`) + проекция строк в Postgres — одна операция
/// под уже взятым репо-локом. Этим путём идёт ЛЮБАЯ веб-правка (сайт, MCP,
/// агент); push проецируется зеркально (git уже записан пушем).
///
/// Вызывающий обязан: убедиться, что репо существует (`repo::ensure_repo_by_id`)
/// и держать `repo::repo_guard` на всё время вызова.
pub async fn commit_web_version(
    pool: &PgPool,
    id: Uuid,
    bare: &Path,
    note: &str,
    author_id: Option<Uuid>,
    rows: Vec<StepRow>,
) -> Result<WebVersion, WebVersionError> {
    // Предусловие: git-tip соответствует current_version. Отставшие репо догоняются
    // здесь же (иначе новый коммит оставил бы дыру в истории), убежавшие — лечатся
    // проекцией tip; глубокое расхождение — отказ, а не тихая порча.
    match sync_repo_with_db(pool, id, bare).await {
        Ok(SyncOutcome::Conflict { have, current }) => {
            return Err(WebVersionError::OutOfSync { have, current });
        }
        Ok(_) => {}
        Err(sqlx::Error::RowNotFound) => return Err(WebVersionError::NotFound),
        Err(e) => return Err(WebVersionError::Db(e)),
    }

    let mut tx = pool.begin().await?;
    let meta = sqlx::query_as::<_, (i32, serde_json::Value, serde_json::Value, Vec<String>, bool)>(
        "select current_version, title, \"desc\", tags, ordered from templates where id = $1 for update",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((current, title, desc, tags, ordered)) = meta else {
        return Err(WebVersionError::NotFound);
    };
    let version = current + 1;
    // Строка версии — в ещё открытую транзакцию: git-коммиту нужен её created_at
    // (иначе bootstrap из БД никогда не воспроизвёл бы тот же SHA), а откат при
    // сбое git бесплатен — durable-запись БД случится только после git.
    let (ver_id, created_s, created_ms) =
        db::insert_version_row(&mut tx, id, version, note, author_id).await?;

    let vdata = VersionData {
        version,
        note: note.to_string(),
        ts: created_s,
        title: db::loc(&title),
        desc: db::loc(&desc),
        tags,
        ordered,
        steps: rows.iter().enumerate().map(|(i, r)| db::ser_step_from_row(i as i32 + 1, r)).collect(),
    };

    // Git — первым. append_versions идёт через единую точку обновления main
    // (валидация как у pre-receive) и ставит тег vN.
    let bare2 = bare.to_path_buf();
    let sha = match tokio::task::spawn_blocking(move || bundle::append_versions(&bare2, &[vdata])).await {
        Ok(Ok(Some(sha))) => sha,
        Ok(Ok(None)) => return Err(WebVersionError::Git("append_versions: нечего коммитить".into())),
        Ok(Err(e)) => return Err(WebVersionError::Git(e.to_string())),
        Err(e) => return Err(WebVersionError::Git(e.to_string())),
    };

    // Проекция: строки шагов + bump — тем же движком, что у git-проекции push.
    let db_tail = async {
        db::insert_step_rows(&mut tx, ver_id, &rows).await?;
        db::bump_current_version(&mut tx, id, version).await?;
        tx.commit().await
    };
    if let Err(e) = db_tail.await {
        metrics::counter!("projection_failures_total", "op" => "web").increment(1);
        tracing::error!(
            %id, version, sha, error = %e,
            "ОШИБКА проекции веб-версии — git-коммит записан, строк БД нет; восстановление: reproject"
        );
        return Err(WebVersionError::ProjectionLost { version, sha, source: e });
    }
    Ok(WebVersion { ver_id, version, commit_sha: sha, created_at_ms: created_ms })
}
