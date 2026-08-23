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
    /// Ветка `main` пропала при целых объектах — возвращена на коммит своего тега.
    MainRestored { version: i32 },
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
/// * каталог есть, а `main` нет → вернуть `main` на коммит старшего тега `vN`
///   (история цела, пропала ссылка); тегов нет вовсе → bootstrap из БД;
/// * `max_tag == current`, но main УШЁЛ ВПЕРЁД тега → непроецированный
///   push/merge-коммит (проекция упала ДО постановки тега) — спроецировать tip;
/// * `max_tag < current` → дописать недостающие версии (одноразовый долг
///   ленивой досыпки; после миграции штатно возникать не должен);
/// * `max_tag == current + 1` и тег стоит на tip main → спроецировать tip
///   (хвост записи, у которой git успел, а БД нет);
/// * иначе → `Conflict`: скорее всего посторонний тег `v<число>`. Имя версии
///   зарезервировано за системой на ОБЕИХ дверях — в RPC создания тега (#59,
///   30.07) и в pre-receive (#100, 23.08); до второй половины владелец мог
///   поставить такой тег пушем руками. Старые репозитории с таким тегом
///   остаются — их чинят руками по runbook.
pub async fn sync_repo_with_db(pool: &PgPool, id: Uuid, bare: &Path) -> Result<SyncOutcome, sqlx::Error> {
    let current: Option<i32> = sqlx::query_scalar("select current_version from templates where id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some(current) = current else {
        return Err(sqlx::Error::RowNotFound);
    };

    if !bare.exists() {
        // Версий в БД нет вовсе — материализовать нечего.
        return Ok(bootstrap_from_db(pool, id, bare).await?.unwrap_or(SyncOutcome::InSync));
    }

    let bare_state = bare.to_path_buf();
    let Some((has_main, have)) =
        tokio::task::spawn_blocking(move || bundle::refs_state(&bare_state)).await.map_err(join_err)?
    else {
        // Каталог есть, а репозиторий не открывается. Молчать нельзя, и лечить
        // пересборкой — тем более: битое репо не пустое, в нём могут лежать
        // принятые пуши и ветки предложений.
        return Err(join_err(format!("repo at {} exists but cannot be opened", bare.display())));
    };

    // Каталог на месте, а ГЛАВНОЙ ВЕТКИ нет. По счётчикам это состояние
    // неотличимо от синхронного (теги-то целы), поэтому ни одна ветка ниже его не
    // берёт, а `tag_on_tip` на нечитаемом ref'е отвечает «тег на вершине» — и репо
    // оставалось без main навсегда: клон пуст, зеркало пусто, само не починится.
    // Замер линзы 02 §4: после удаления main снимок отдавал null и после
    // выравнивания ветка не появлялась.
    if !has_main {
        // Объекты и теги целы — значит история НА МЕСТЕ, пропала только ссылка.
        // Возвращаем main на коммит нашего тега, а не пересобираем из БД:
        // `load_bundle_data` берёт СЕГОДНЯШНЮЮ мету списка для ВСЕХ версий, поэтому
        // после любой правки названия пересборка дала бы другие деревья и другие
        // SHA, форсом перевесила бы теги vN на эту синтетику и выбросила принятые
        // пуши и коммиты слияния — то есть уничтожила бы ровно тот канон, который
        // лечение обязано спасти (находка авто-ревью на #100, P1).
        //
        // Тег берём НЕ старший, а тот, что отвечает ТЕКУЩЕЙ версии базы (или ниже,
        // если БД ушла вперёд). Восстановление по старшему подняло бы main на
        // ПОСТОРОННИЙ тег `v<current+1>` — такое имя мог занять релиз, пока
        // резерв имён стоял только на веб-двери, — и следующая же проверка сочла
        // бы это хвостом прерванной записи и
        // спроецировала чужой коммит новой версией, причём на обычном ЧТЕНИИ
        // (второй P1 авто-ревью на #100). Ниже своей версии подниматься безопасно:
        // это наш собственный коммит, а недостающие версии допишет досыпка.
        let restore_ver = have.min(current);
        if restore_ver > 0 && restore_main_from_tag(bare, restore_ver).await? {
            tracing::warn!(%id, version = restore_ver, "main was missing, restored from tag v{restore_ver} (heal)");
            let outcome = align_by_counters(pool, id, bare, current, have).await?;
            return Ok(match outcome {
                SyncOutcome::InSync => SyncOutcome::MainRestored { version: restore_ver },
                other => other,
            });
        }
        // Пересобираем из БД ТОЛЬКО когда тегов нет вовсе. Если теги есть, а нужного
        // нет (дыра в истории: `v1, v2, v5` при `current = 3`), пересборка переписала
        // бы целую историю и форсом сдвинула бы все теги — под видом лечения. Такое
        // расхождение обязано дойти до человека конфликтом, а не «вылечиться».
        if have == 0
            && let Some(outcome) = bootstrap_from_db(pool, id, bare).await?
        {
            tracing::warn!(%id, "repo had neither main nor version tags, rebuilt from db (heal)");
            return Ok(outcome);
        }
    }

    align_by_counters(pool, id, bare, current, have).await
}

/// Выравнивание по счётчикам: `have` (старший тег vN) против `current_version`.
/// Вынесено из `sync_repo_with_db`, чтобы лечение пропавшей ветки могло пройти
/// этот же путь и не дублировать его вторым описанием.
async fn align_by_counters(
    pool: &PgPool,
    id: Uuid,
    bare: &Path,
    current: i32,
    have: i32,
) -> Result<SyncOutcome, sqlx::Error> {
    if have == current {
        // Равенство счётчиков ещё не синхронность: если проекция push/merge упала
        // ДО постановки тега, main уже впереди, а тега vN нет — по счётчикам всё
        // «сошлось». Не проверив tip, мы бы позволили следующей веб-записи
        // положить свой v(N+1) поверх, и принятый push никогда не стал бы
        // версией (P1 авто-ревью #63). Сверяем тег текущей версии с tip.
        if current > 0 && !tag_on_tip(bare, current).await? {
            match project::project_pushed_commit(pool, id, bare).await? {
                Some(v) => {
                    tracing::warn!(
                        %id, version = v,
                        "main was ahead without a tag (unprojected push), tip projected (heal)"
                    );
                    return Ok(SyncOutcome::ProjectedTip { version: v });
                }
                // tip не проецируем (битый/пустой list.json — так push и оставил
                // его без версии). Это штатный «неверсионный» коммит: запись
                // поверх легитимна, история его сохранит.
                None => {
                    tracing::warn!(
                        %id, current,
                        "main is ahead of tag v{current} but the tip does not project (list.json without a version), left as is"
                    );
                    return Ok(SyncOutcome::InSync);
                }
            }
        }
        return Ok(SyncOutcome::InSync);
    }

    if have < current {
        let versions: Vec<_> =
            db::load_bundle_data(pool, id).await?.into_iter().filter(|v| v.version > have).collect();
        let count = versions.len() as u64;
        if count == 0 {
            // Строк истории нет вовсе. Если и main ещё не родился (пустое репо,
            // ensure создал его под список без версий) — это «список до первой
            // версии»: current_version тут дефолт колонки, выравнивать нечего.
            // Рассинхроном считается только «main есть, а строк нет».
            if have == 0 && !main_exists(bare).await? {
                return Ok(SyncOutcome::InSync);
            }
            return Ok(SyncOutcome::Conflict { have, current });
        }
        let bare2 = bare.to_path_buf();
        // Досыпка, а не обычная запись: версия может УЖЕ лежать коммитом (потерян
        // тег, а не коммит), и тогда её надо дотегировать, а не коммитить второй раз.
        tokio::task::spawn_blocking(move || bundle::append_missing_versions(&bare2, &versions))
            .await
            .map_err(join_err)?
            .map_err(join_err)?;
        metrics::counter!("repo_catchup_versions_total").increment(count);
        tracing::warn!(%id, from = have + 1, to = current, "git lagged behind db, versions appended (catch-up)");
        return Ok(SyncOutcome::Appended { from: have + 1, to: current });
    }

    // have > current: либо хвост незавершённой записи (ровно на 1, тег на tip),
    // либо посторонний тег vN / глубокое расхождение.
    if have == current + 1
        && tag_on_tip(bare, have).await?
        && let Some(v) = project::project_pushed_commit(pool, id, bare).await?
    {
        tracing::warn!(%id, version = v, "git was one version ahead of db, tip projected (heal)");
        return Ok(SyncOutcome::ProjectedTip { version: v });
    }

    metrics::counter!("version_tag_conflicts_total").increment(1);
    tracing::error!(
        %id, have, current,
        "tag v{have} is above current version {current} and this is not a write tail: a v<number> name is \
         taken by something other than a version, or history diverged - write stopped, see runbook \
         git-projection-catchup"
    );
    Ok(SyncOutcome::Conflict { have, current })
}

/// Есть ли у репо main (пустой bare списка до первой версии его не имеет).
/// Спрашивается только в ветке «тегов нет и строк истории нет», поэтому отдельным
/// чтением: на горячем пути состояние рефов уже прочитано одним `refs_state`.
async fn main_exists(bare: &Path) -> Result<bool, sqlx::Error> {
    let bare_chk = bare.to_path_buf();
    tokio::task::spawn_blocking(move || {
        git2::Repository::open_bare(&bare_chk).is_ok_and(|r| r.refname_to_id(MAIN_REF).is_ok())
    })
    .await
    .map_err(join_err)
}

/// Возвращает `refs/heads/main` на коммит тега `v<ver>`. Через ту же точку
/// обновления main, что и запись версий: проверки состава дерева обязаны
/// действовать и на лечении — иначе оно стало бы дырой в правилах хука.
/// `false` — тега `v<ver>` нет (восстанавливать не по чему); ветка при этом не
/// трогается вовсе, и решение остаётся за выравниванием по счётчикам.
async fn restore_main_from_tag(bare: &Path, ver: i32) -> Result<bool, sqlx::Error> {
    let bare2 = bare.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<bool, String> {
        let repo = git2::Repository::open_bare(&bare2).map_err(|e| e.to_string())?;
        let Ok(oid) = repo.refname_to_id(&format!("refs/tags/v{ver}")) else {
            return Ok(false);
        };
        crate::git::update::update_main(&repo, oid, None, "setfork: restore main from tag")
            .map_err(|e| e.to_string())?;
        let _ = repo.set_head(MAIN_REF);
        Ok(true)
    })
    .await
    .map_err(join_err)?
    .map_err(join_err)
}

/// Собирает историю списка из БД в bare-репо. `None` — версий в БД нет вовсе
/// (материализовать нечего); повторный вызов поверх существующего каталога
/// безопасен: SHA детерминированы, объекты уже на месте, теги ставятся заново.
async fn bootstrap_from_db(pool: &PgPool, id: Uuid, bare: &Path) -> Result<Option<SyncOutcome>, sqlx::Error> {
    let versions = db::load_bundle_data(pool, id).await?;
    if versions.is_empty() {
        return Ok(None);
    }
    let n = versions.len();
    let bare2 = bare.to_path_buf();
    tokio::task::spawn_blocking(move || bundle::bootstrap_bare(&versions, &bare2))
        .await
        .map_err(join_err)?
        .map_err(join_err)?;
    Ok(Some(SyncOutcome::Bootstrapped { versions: n }))
}

/// Тег `v<ver>` указывает ровно на tip main? Ошибки чтения считаем «на месте»:
/// sync не должен мешать чтению из-за нечитаемого ref'а. Единственное состояние,
/// которое так проглатывалось молча, — отсутствие main; его теперь ловит
/// отдельная ветка ВЫШЕ по коду, до сравнения счётчиков.
async fn tag_on_tip(bare: &Path, ver: i32) -> Result<bool, sqlx::Error> {
    let bare_chk = bare.to_path_buf();
    tokio::task::spawn_blocking(move || -> bool {
        let Ok(repo) = git2::Repository::open_bare(&bare_chk) else { return true };
        let (Ok(tag), Ok(tip)) =
            (repo.refname_to_id(&format!("refs/tags/v{ver}")), repo.refname_to_id(MAIN_REF))
        else {
            return true;
        };
        tag == tip
    })
    .await
    .map_err(join_err)
}

/// Патч меты, применяемый той же транзакцией, что и версия (Ф2a-довесок):
/// None = поле не трогать. Раньше фронт писал мету отдельным запросом ДО
/// addVersion — сбой RPC оставлял мету записанной без версии и без коммита.
#[derive(Debug, Default)]
pub struct MetaPatch {
    pub title: Option<serde_json::Value>, // LocaleText jsonb
    pub desc: Option<serde_json::Value>,
    pub tags: Option<Vec<String>>,
    pub ordered: Option<bool>,
}

// LocaleText jsonb несёт хоть один непустой перевод?
fn has_text(v: &serde_json::Value) -> bool {
    v.as_object().is_some_and(|o| o.values().any(|s| s.as_str().is_some_and(|t| !t.trim().is_empty())))
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
    /// Правка основана не на текущей версии: пока её готовили, список успел
    /// уйти вперёд. Ничего не записано — писавшему нужно перечитать и повторить.
    VersionConflict { expected: i32, current: i32 },
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

/// Одна веб-правка: что записываем и на чём она основана.
///
/// Раньше это были пять параметров подряд, два из них `Option`, — такие ряды
/// путаются местами при вызове и молча компилируются.
pub struct WebEdit<'a> {
    pub note: &'a str,
    /// Автор версии; None — фоновые и git-пути, где автора нет.
    pub author_id: Option<Uuid>,
    pub rows: Vec<StepRow>,
    /// Патч меты списка, применяемый той же транзакцией (Ф2a-довесок).
    pub meta: MetaPatch,
    /// Версия, НА КОТОРОЙ основана правка. Задано — сверяется с текущей внутри
    /// транзакции, где строка уже взята `for update`; расхождение = отказ, ничего
    /// не записано. None — прежнее поведение (последняя запись побеждает).
    pub expected_version: Option<i32>,
}

impl<'a> WebEdit<'a> {
    /// Правка без предусловия по версии — как писали до появления сверки.
    pub fn new(note: &'a str, author_id: Option<Uuid>, rows: Vec<StepRow>, meta: MetaPatch) -> Self {
        WebEdit { note, author_id, rows, meta, expected_version: None }
    }
    /// То же, но основанное на конкретной версии (оптимистичная блокировка).
    pub fn based_on(mut self, expected_version: Option<i32>) -> Self {
        self.expected_version = expected_version;
        self
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
    edit: WebEdit<'_>,
) -> Result<WebVersion, WebVersionError> {
    let WebEdit { note, author_id, rows, meta, expected_version } = edit;
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
    let row =
        sqlx::query_as::<_, (i32, serde_json::Value, serde_json::Value, Vec<String>, bool, Option<String>)>(
            "select current_version, title, \"desc\", tags, ordered, list_kind \
             from templates where id = $1 for update",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((current, title, desc, tags, ordered, kind)) = row else {
        return Err(WebVersionError::NotFound);
    };
    // Сверка «правка основана на текущей версии» — ЗДЕСЬ, а не у вызывающего:
    // строка списка уже взята `for update` этой же транзакцией, поэтому между
    // сравнением и коммитом чужая версия лечь не может. Проверка снаружи (в
    // приложении) такого обещания не даёт — там между чтением и записью окно.
    if let Some(expected) = expected_version.filter(|e| *e != current) {
        return Err(WebVersionError::VersionConflict { expected, current });
    }
    // Патч меты — В ЭТОЙ ЖЕ транзакции, ДО сборки канона: коммит версии сразу
    // несёт свежие title/desc/tags/ordered, а сбой RPC не оставляет мету
    // записанной без версии (Ф2a-довесок). Пустой title игнорируется —
    // название обязательно.
    let title = match &meta.title {
        Some(t) if has_text(t) => {
            sqlx::query("update templates set title = $1::jsonb where id = $2")
                .bind(t)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            t.clone()
        }
        _ => title,
    };
    let desc = match &meta.desc {
        Some(d) => {
            sqlx::query("update templates set \"desc\" = $1::jsonb where id = $2")
                .bind(d)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            d.clone()
        }
        None => desc,
    };
    let tags = match &meta.tags {
        Some(tg) => {
            let tg: Vec<String> = tg.iter().take(20).cloned().collect();
            sqlx::query("update templates set tags = $1 where id = $2")
                .bind(&tg)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tg
        }
        None => tags,
    };
    let ordered = match meta.ordered {
        Some(o) => {
            sqlx::query("update templates set ordered = $1 where id = $2")
                .bind(o)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            o
        }
        None => ordered,
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
        // kind — из той же строки templates (Ф2a): мусор не публикуем.
        kind: kind.filter(|k| crate::git::serialize::is_valid_kind(k)),
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
            "web version projection FAILED: git commit written, db rows missing; recovery: reproject"
        );
        return Err(WebVersionError::ProjectionLost { version, sha, source: e });
    }
    Ok(WebVersion { ver_id, version, commit_sha: sha, created_at_ms: created_ms })
}
