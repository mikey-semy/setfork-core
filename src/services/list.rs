//! List — доменный порт ListStore на Rust, read + write (см. proto/domain_read.proto).
//!
//! ListRead зеркалит TS-адаптер features/library/list-store.adapter.ts
//! запрос-в-запрос, чтобы golden-сверка JSON совпадала. Особенности TS,
//! сохранённые намеренно:
//! - repository_id = id списка (synthetic solo-repo, как в toList);
//! - Version.commit_sha всегда '' (TS отдаёт null);
//! - Contributor: владелец первым (accepted у него 0), остальные по accepted desc;
//!   avatar_ref = СЫРОЙ users.avatar_url (TS подписывает imgproxy-URL — это
//!   презентация; в golden-сверке поле нормализуется).
//!
//! ListWrite — полная семантика addVersion/create из list-store.adapter.ts
//! (в отличие от db::add_version, который упрощён под git-проекцию): LocaleText
//! сохраняется как есть, imageRef → has_image/image_key, bump current_version +
//! updated_at — всё в ОДНОЙ транзакции.
use sqlx::Row;
use sqlx::postgres::PgPool;
use tonic::{Code, Request, Response, Status};
use uuid::Uuid;

use super::util::{db_status, loc_json, loc_map, parse_id, refs_json};
use crate::blocks::{content_value, storage_type, wire_type};
use crate::db;
use crate::git::{repo, version};
use crate::pb_domain::list_read_server::ListRead;
use crate::pb_domain::list_write_server::ListWrite;
use crate::pb_domain::{
    AddVersionRequest, Contributor, ContributorsResponse, CreateListRequest, GetListResponse,
    GetVersionRequest, GetVersionResponse, List, ListId, ListRef, NewStep, Step, StepRef, Version,
    VersionsResponse,
};

/// ListRead: чтение списков/версий/шагов (зеркало list-store.adapter.ts).
pub struct ListReadSvc {
    pub pool: PgPool,
}

// Golden-сверка с TS вынесена в services::golden (инструмент CLI, не RPC).

#[tonic::async_trait]
impl ListRead for ListReadSvc {
    async fn get_list(&self, req: Request<ListRef>) -> Result<Response<GetListResponse>, Status> {
        let ListRef { owner, slug } = req.into_inner();
        let row = sqlx::query(
            "select t.id, t.owner_id, t.slug, t.title, t.\"desc\", t.tags, t.ordered, \
                    t.status::text as status, t.visibility::text as visibility, \
                    t.moderation::text as moderation, t.moderation_reason, t.verified, t.pinned, \
                    t.origin::text as origin, t.forked_from_id, t.current_version, \
                    t.stars_count, t.forks_count, t.runs_count, \n                    floor(extract(epoch from t.created_at) * 1000)::bigint as created_at_ms, \n                    floor(extract(epoch from t.updated_at) * 1000)::bigint as updated_at_ms \
             from templates t join users u on u.id = t.owner_id \
             where u.handle = $1 and t.slug = $2 limit 1",
        )
        .bind(&owner)
        .bind(&slug)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_status)?;

        let Some(r) = row else {
            return Ok(Response::new(GetListResponse { found: false, list: None }));
        };
        let id: Uuid = r.get("id");
        let list = List {
            id: id.to_string(),
            owner_id: r.get::<Uuid, _>("owner_id").to_string(),
            slug: r.get("slug"),
            title: Some(loc_map(&r.get::<serde_json::Value, _>("title"))),
            desc: Some(loc_map(&r.get::<serde_json::Value, _>("desc"))),
            tags: r.get("tags"),
            ordered: r.get("ordered"),
            status: r.get("status"),
            visibility: r.get("visibility"),
            moderation: r.get("moderation"),
            moderation_reason: r.get::<Option<String>, _>("moderation_reason").unwrap_or_default(),
            verified: r.get("verified"),
            pinned: r.get("pinned"),
            origin: r.get("origin"),
            forked_from_id: r
                .get::<Option<Uuid>, _>("forked_from_id")
                .map(|u| u.to_string())
                .unwrap_or_default(),
            current_version: r.get("current_version"),
            stars_count: r.get("stars_count"),
            forks_count: r.get("forks_count"),
            runs_count: r.get("runs_count"),
            // Synthetic solo-repo, как в TS toList (r.repositoryId не читается там же).
            repository_id: id.to_string(),
            created_at_ms: r.get("created_at_ms"),
            updated_at_ms: r.get("updated_at_ms"),
        };
        Ok(Response::new(GetListResponse { found: true, list: Some(list) }))
    }

    async fn list_versions(&self, req: Request<ListId>) -> Result<Response<VersionsResponse>, Status> {
        let id = parse_id(&req.into_inner().id)?;
        let rows = sqlx::query(
            "select id, template_id, version, note, author_id::text as author_id, \
                    floor(extract(epoch from created_at) * 1000)::bigint as created_at_ms \
             from template_versions where template_id = $1 order by version desc",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_status)?;
        // Пустая история — в git не ходим вовсе: делать нечего, а том может быть недоступен.
        let shas = if rows.is_empty() { std::collections::HashMap::new() } else { version_shas(id).await };
        let versions = rows
            .iter()
            .map(|r| Version {
                id: r.get::<Uuid, _>("id").to_string(),
                list_id: r.get::<Uuid, _>("template_id").to_string(),
                version: r.get("version"),
                note: r.get("note"),
                commit_sha: shas.get(&r.get::<i32, _>("version")).cloned().unwrap_or_default(),
                created_at_ms: r.get("created_at_ms"),
                author_id: r.get::<Option<String>, _>("author_id").unwrap_or_default(),
            })
            .collect();
        Ok(Response::new(VersionsResponse { versions }))
    }

    async fn get_version(
        &self,
        req: Request<GetVersionRequest>,
    ) -> Result<Response<GetVersionResponse>, Status> {
        let GetVersionRequest { list_id, version } = req.into_inner();
        let id = parse_id(&list_id)?;
        let vrow = sqlx::query(
            "select id, template_id, version, note, author_id::text as author_id, \
                    floor(extract(epoch from created_at) * 1000)::bigint as created_at_ms \
             from template_versions where template_id = $1 and version = $2 limit 1",
        )
        .bind(id)
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_status)?;
        let Some(v) = vrow else {
            return Ok(Response::new(GetVersionResponse { found: false, version: None, steps: vec![] }));
        };
        let vid: Uuid = v.get("id");
        let sha = version_sha_one(id, v.get::<i32, _>("version")).await;
        let ver = Version {
            id: vid.to_string(),
            list_id: v.get::<Uuid, _>("template_id").to_string(),
            version: v.get("version"),
            note: v.get("note"),
            commit_sha: sha,
            created_at_ms: v.get("created_at_ms"),
            author_id: v.get::<Option<String>, _>("author_id").unwrap_or_default(),
        };

        let srows = sqlx::query(
            "select id, version_id, n, \"type\", content, title, \"desc\", command, level::text as level, \
                    why, section, subtasks, refs, image_key, needs_human, needs_human_ask, danger \
             from steps where version_id = $1 order by n asc",
        )
        .bind(vid)
        .fetch_all(&self.pool)
        .await
        .map_err(db_status)?;
        let steps = srows
            .iter()
            .map(|s| {
                let subtasks = s
                    .get::<serde_json::Value, _>("subtasks")
                    .as_array()
                    .map(|a| a.iter().map(loc_map).collect())
                    .unwrap_or_default();
                let refs = s
                    .get::<serde_json::Value, _>("refs")
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|r| StepRef {
                                label: Some(loc_map(r.get("label").unwrap_or(&serde_json::Value::Null))),
                                url: r.get("url").and_then(|u| u.as_str()).unwrap_or_default().to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // '' у шага (в т.ч. NULL/'step'); тип несём только у text/image.
                let block_ty = wire_type(s.get::<Option<String>, _>("type").as_deref());
                Step {
                    id: s.get::<Uuid, _>("id").to_string(),
                    version_id: s.get::<Uuid, _>("version_id").to_string(),
                    n: s.get("n"),
                    title: Some(loc_map(&s.get::<serde_json::Value, _>("title"))),
                    desc: Some(loc_map(&s.get::<serde_json::Value, _>("desc"))),
                    command: s.get("command"),
                    level: s.get("level"),
                    why: Some(loc_map(&s.get::<serde_json::Value, _>("why"))),
                    section: Some(loc_map(&s.get::<serde_json::Value, _>("section"))),
                    subtasks,
                    refs,
                    image_ref: s.get::<Option<String>, _>("image_key").unwrap_or_default(),
                    // type/content_json — только у не-step блоков ('' у шага).
                    // Пометка «здесь нужен человек» — часть шага: читатель должен видеть,
                    // где нужен живой опыт, независимо от того, кто отдаёт список.
                    needs_human: s.try_get::<bool, _>("needs_human").unwrap_or(false),
                    needs_human_ask: Some(loc_map(
                        &s.try_get::<serde_json::Value, _>("needs_human_ask")
                            .unwrap_or(serde_json::Value::Null),
                    )),
                    danger: s.try_get::<bool, _>("danger").unwrap_or(false),
                    r#type: block_ty.clone(),
                    content_json: if block_ty.is_empty() {
                        String::new()
                    } else {
                        s.try_get::<serde_json::Value, _>("content")
                            .map(|c| c.to_string())
                            .unwrap_or_default()
                    },
                }
            })
            .collect();
        Ok(Response::new(GetVersionResponse { found: true, version: Some(ver), steps }))
    }

    async fn get_contributors(&self, req: Request<ListId>) -> Result<Response<ContributorsResponse>, Status> {
        let id = parse_id(&req.into_inner().id)?;
        // ОДИН запрос вместо трёх. Раньше было: строка списка → строка владельца по
        // owner_id → агрегат по предложениям. Первые два — N+1 на одну строку, третий
        // от owner_id вообще не зависел. На нашей нагрузке это не «медленно» (замер
        // 24.08: ядро почти простаивает), но три round trip'а там, где хватает одного,
        // остаются тремя и под нагрузкой — шаг 3 трека производительности.
        //
        // Порядок и состав закреплены пробой `co_author_set_and_order`, написанной
        // ДО этой правки и зелёной на прежнем коде: владелец первым и с нулём (паритет
        // с TS), он же не задваивается, если сам автор предложения; остальные по
        // убыванию принятых; автор без единого принятого всё равно в списке.
        let rows = sqlx::query(
            "with owner_row as ( \
                 select u.handle, u.avatar_url, 0::int as accepted, 0::int as ord \
                 from templates t join users u on u.id = t.owner_id \
                 where t.id = $1 \
             ), contrib as ( \
                 select u.handle, u.avatar_url, \
                        (count(*) filter (where s.status = 'accepted'))::int as accepted, \
                        1::int as ord \
                 from suggestions s \
                 join users u on u.id = s.author_id \
                 join templates t on t.id = s.template_id \
                 where s.template_id = $1 and s.author_id <> t.owner_id \
                 group by u.handle, u.avatar_url \
             ) \
             select handle, avatar_url, accepted, ord from owner_row \
             union all \
             select handle, avatar_url, accepted, ord from contrib \
             order by ord, accepted desc",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_status)?;

        let contributors = rows
            .into_iter()
            .map(|r| Contributor {
                handle: r.get("handle"),
                avatar_ref: r.get::<Option<String>, _>("avatar_url").unwrap_or_default(),
                accepted: r.get("accepted"),
            })
            .collect();
        Ok(Response::new(ContributorsResponse { contributors }))
    }
}

// ── ListWrite: addVersion / create ───────────────────────────────────────

/// ListWrite: создание списка и версий через общий движок db::add_version_rows.
pub struct ListWriteSvc {
    pub pool: PgPool,
}

// NewStep → db::StepRow: семантика TS-адаптера как есть — LocaleText без trim
// и фильтрации, imageRef → image_key, пустой level = required.
/// Идентичность блока из запроса. Пустая строка — законное «идентичность неизвестна»
/// (так записано в контракте). Непустое НЕГОДНОЕ значение — другое дело: вызывающий
/// идентичность прислал, а мы её выбрасываем, и дифф после этого читает переименование
/// как «удалён + добавлен». Молчать об этом нельзя — но и отказывать нельзя, потому что
/// прежнее поведение принимало такой запрос, и ужесточение контракта сломало бы
/// вызывающих (тем же уроком, что записан в `ensure_repo_by_id` про пустую историю).
///
/// Поэтому: поведение прежнее, но след в журнале есть. Если строка появится — значит
/// кто-то шлёт мусор, и это видно, а не теряется молча.
fn block_id_of(s: &NewStep) -> Option<uuid::Uuid> {
    match uuid::Uuid::parse_str(&s.block_id) {
        Ok(id) => Some(id),
        Err(_) if s.block_id.is_empty() => None,
        Err(_) => {
            tracing::warn!(
                len = s.block_id.len(),
                "block_id is not a uuid - identity dropped, diff will fall back to title matching"
            );
            None
        }
    }
}

/// SHA версий из ТЕГОВ репозитория: `refs/tags/v<N>` → sha коммита.
///
/// Источник — git, а не колонка в Postgres, и это не вкус: по ADR-0014 канон живёт в git,
/// Postgres — read-model. Колонка дублировала бы канон в проекции, и её пришлось бы
/// поддерживать в согласии с ним. Тег версии И ЕСТЬ её байтовая идентичность — ровно та,
/// ради которой поле заведено в контракте.
///
/// ⚠️ Репозиторий здесь НЕ материализуется. Список, которого ещё не касались по git,
/// получает пустой SHA — это видимое лицо находки V1 («рождение списка канона не
/// создаёт», реестр вертикали 27.08: 58 списков из 659, все версия 1), а не поломка
/// чтения. Материализация по требованию — изменение поведения и отдельное решение
/// (T1.3b), а чтение версий не должно менять состояние тома.
///
/// Сбой git тоже даёт пустоту, а не отказ: витрина версий обязана открываться, даже если
/// том недоступен. Пустой SHA у вызывающего значит «неизвестен», и показывать его как
/// «версии нет» нельзя — тот же класс, что `new_version = 0` у слияния.
/// Путь репозитория, если том вообще задан.
///
/// ⚠️ `repo::repo_path` внутри делает `expect` на `GIT_DATA_DIR`. Команды golden-CLI
/// (`cli::run` в `main.rs`) исполняются ДО `require_git_data_dir()` и по замыслу работают
/// без тома — материализуют во временные каталоги. Значит зов `repo_path` отсюда уронил бы
/// `domain-read` паникой вместо вывода JSON. Проверка переменной раньше пути — не
/// перестраховка, а условие, при котором обещание «сбой git даёт пустоту, а не отказ»
/// вообще выполняется: паника случилась бы ДО `spawn_blocking` и никаким `unwrap_or_default`
/// не ловилась.
fn bare_if_volume_set(id: Uuid) -> Option<std::path::PathBuf> {
    std::env::var("GIT_DATA_DIR").ok()?;
    let bare = crate::git::repo::repo_path(id);
    bare.exists().then_some(bare)
}

/// Номер версии из имени тега. `strip_prefix`, а НЕ `trim_start_matches`: второй снимает
/// все ведущие `v` подряд, и тогда релизный тег `vv2` (законный — `is_version_tag` его
/// версией не считает) разобрался бы как версия 2 и столкнулся с настоящим `v2`. Победитель
/// зависел бы от порядка обхода рефов, то есть показанная «байтовая идентичность» указывала
/// бы на чужой коммит и менялась от запроса к запросу.
fn version_of_tag(name: &str) -> Option<i32> {
    name.strip_prefix('v')?.parse::<i32>().ok()
}

async fn version_shas(id: Uuid) -> std::collections::HashMap<i32, String> {
    let Some(bare) = bare_if_volume_set(id) else { return std::collections::HashMap::new() };
    tokio::task::spawn_blocking(move || {
        let mut out = std::collections::HashMap::new();
        let Ok(repo) = git2::Repository::open_bare(&bare) else { return out };
        let Ok(tags) = repo.references_glob("refs/tags/v*") else { return out };
        for r in tags.flatten() {
            let Ok(name) = r.shorthand() else { continue };
            let Some(n) = version_of_tag(name) else { continue };
            // Тег может быть аннотированным — тогда нужен коммит, на который он смотрит.
            if let Ok(commit) = r.peel_to_commit() {
                out.insert(n, commit.id().to_string());
            }
        }
        out
    })
    .await
    .unwrap_or_default()
}

/// SHA ОДНОЙ версии: прямой поиск рефа вместо обхода всех тегов. Тем же приёмом, что
/// `git::version` и `git_core` — у списка с сотней версий перечисление стоило бы сотни
/// поисков объектов ради одного ответа.
async fn version_sha_one(id: Uuid, version: i32) -> String {
    let Some(bare) = bare_if_volume_set(id) else { return String::new() };
    tokio::task::spawn_blocking(move || {
        let Ok(repo) = git2::Repository::open_bare(&bare) else { return String::new() };
        let Ok(r) = repo.find_reference(&format!("refs/tags/v{version}")) else { return String::new() };
        r.peel_to_commit().map(|c| c.id().to_string()).unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

fn step_row(s: &NewStep) -> db::StepRow {
    // Блочная модель: правила нормализации type/content — в blocks (одно место).
    db::StepRow {
        block_type: storage_type(&s.r#type),
        content: content_value(&s.r#type, &s.content_json),
        // Идентичность блока сквозь версии; '' или мусор → None (фолбэк диффа).
        block_id: block_id_of(s),
        title: loc_json(&s.title),
        desc: loc_json(&s.desc),
        command: s.command.clone(),
        image_key: if s.image_ref.is_empty() { None } else { Some(s.image_ref.clone()) },
        level: db::Level::parse(&s.level),
        why: loc_json(&s.why),
        section: loc_json(&s.section),
        subtasks: serde_json::Value::Array(s.subtasks.iter().map(|t| loc_json(&Some(t.clone()))).collect()),
        refs: refs_json(&s.refs),
        // Пометка «здесь нужен человек» доезжает до записи. Вопрос имеет смысл только
        // при поднятой пометке: «что спросить» без «нужен человек» — висячий текст.
        needs_human: s.needs_human,
        needs_human_ask: if s.needs_human { loc_json(&s.needs_human_ask) } else { serde_json::json!({}) },
        // Разрушительный пункт: пометку ставит вызывающий (автор вручную либо
        // авто-простановка по шаблону команды на его стороне) — ядро её хранит.
        danger: s.danger,
    }
}

// Отказ git-first записи версии → gRPC-статус (текст — оператору в лог фронта).
fn web_version_status(e: version::WebVersionError) -> Status {
    match e {
        version::WebVersionError::NotFound => Status::not_found("list not found"),
        // ABORTED — код конфликта конкурентной записи (AIP-154, google.rpc.Code):
        // не invalid_argument (запрос корректен) и не failed_precondition (состояние
        // системы исправно). Причина STALE — та же, что у веток: объект подвинули
        // между чтением и записью, клиент перечитывает и повторяет.
        version::WebVersionError::VersionConflict { expected, current } => crate::reason::status(
            Code::Aborted,
            crate::reason::Reason::Stale,
            format!(
                "list moved on: it is at v{current}, the edit is based on v{expected} — read it again and re-apply"
            ),
        ),
        // FAILED_PRECONDITION с причиной: повтор бессмыслен (в отличие от STALE),
        // это состояние чинит оператор. Без причины в трейлере фронт не мог отличить
        // его от прочих предусловий и показывал безымянный сбой.
        version::WebVersionError::OutOfSync { have, current } => crate::reason::status(
            Code::FailedPrecondition,
            crate::reason::Reason::OutOfSync,
            format!("repo out of sync (git v{have}, db v{current}) - see runbook git-projection-catchup"),
        ),
        version::WebVersionError::Db(e) => db_status(e),
        version::WebVersionError::Git(e) => Status::internal(format!("git commit failed: {e}")),
        // Канон записан, проекция отстала: повтор сохранения сам долечит
        // (sync_repo_with_db спроецирует tip), либо reproject руками.
        version::WebVersionError::ProjectionLost { version, sha, source } => Status::internal(format!(
            "version v{version} committed to git ({sha}) but projection failed: {source}; retry or reproject"
        )),
    }
}

#[tonic::async_trait]
impl ListWrite for ListWriteSvc {
    /// Новая версия — GIT-FIRST (Ф1): сначала коммит vN на main (через единую
    /// точку обновления с валидацией), затем строки БД как проекция — одна
    /// операция под репо-локом. БД здесь read-model: git не откатывается.
    async fn add_version(&self, req: Request<AddVersionRequest>) -> Result<Response<Version>, Status> {
        let AddVersionRequest { list_id, note, steps, author_id, meta, expected_version } = req.into_inner();
        let tid = parse_id(&list_id)?;
        // author_id: '' = null (фоновые/git-пути автора не знают).
        let author = if author_id.is_empty() { None } else { Some(parse_id(&author_id)?) };
        let rows: Vec<db::StepRow> = steps.iter().map(step_row).collect();
        // Патч меты (Ф2a-довесок): применяется ядром в той же транзакции, что и
        // версия, — сбой RPC не оставляет мету записанной без версии.
        let meta = meta
            .map(|m| version::MetaPatch {
                title: m.title.is_some().then(|| loc_json(&m.title)),
                desc: m.desc.is_some().then(|| loc_json(&m.desc)),
                tags: m.tags.map(|t| t.v),
                ordered: m.ordered,
            })
            .unwrap_or_default();

        // Репо обязано существовать до коммита (bootstrap при первом касании).
        let bare = repo::ensure_repo_by_id(&self.pool, tid)
            .await
            .map_err(db_status)?
            .ok_or_else(|| Status::not_found("list not found"))?;
        // Коммит + проекция — критическая секция, как у push/merge.
        let _guard = repo::repo_guard(&self.pool, tid).await.map_err(db_status)?;
        let edit = version::WebEdit::new(&note, author, rows, meta).based_on(expected_version);
        let out =
            version::commit_web_version(&self.pool, tid, &bare, edit).await.map_err(web_version_status)?;
        // Ф3: зеркало догоняет истину после каждой версии (фоново).
        super::git_core::spawn_mirror(self.pool.clone(), tid, bare);

        Ok(Response::new(Version {
            id: out.ver_id.to_string(),
            list_id: tid.to_string(),
            version: out.version,
            note,
            commit_sha: out.commit_sha,
            created_at_ms: out.created_at_ms,
            author_id,
        }))
    }
    async fn create(&self, req: Request<CreateListRequest>) -> Result<Response<List>, Status> {
        let r = req.into_inner();
        let owner = Uuid::parse_str(&r.owner_id).map_err(|_| Status::invalid_argument("bad owner uuid"))?;
        let forked_from: Option<Uuid> = if r.forked_from_id.is_empty() {
            None
        } else {
            Some(Uuid::parse_str(&r.forked_from_id).map_err(|_| Status::invalid_argument("bad fork uuid"))?)
        };

        // Состояние публикации — часть ВСТАВКИ. Отдельный update после create оставлял бы
        // окно, в котором список уже виден всем: между коммитом транзакции и апдейтом (и
        // навсегда, если апдейт не случился). Пустое значение = active — сборка фронта,
        // которая поля ещё не шлёт, пишет как раньше.
        let moderation = if r.moderation.is_empty() { "active" } else { r.moderation.as_str() };
        if !matches!(moderation, "active" | "pending" | "flagged" | "hidden") {
            return Err(Status::invalid_argument("bad moderation"));
        }

        let mut tx = self.pool.begin().await.map_err(db_status)?;
        // moderation возвращается ИЗ СТРОКИ, а не подставляется из запроса: по этому полю
        // вызывающий проверяет, что его решение доехало (сборки фронта и ядра выкатываются
        // порознь). Ответ, собранный из входа, на такой вопрос отвечает всегда «да».
        let row: (Uuid, String, i64, i64) = sqlx::query_as(
            "insert into templates (owner_id, slug, title, \"desc\", tags, ordered, visibility, status,                                     origin, forked_from_id, moderation, current_version)              values ($1, $2, $3::jsonb, $4::jsonb, $5, $6, $7::list_visibility, $8::list_status,                      $9::template_origin, $10, $11::moderation_status, 1)              returning id, moderation::text,                        floor(extract(epoch from created_at) * 1000)::bigint,                        floor(extract(epoch from updated_at) * 1000)::bigint",
        )
        .bind(owner)
        .bind(&r.slug)
        .bind(loc_json(&r.title))
        .bind(loc_json(&r.desc))
        .bind(&r.tags)
        .bind(r.ordered)
        .bind(if r.visibility.is_empty() { "public" } else { &r.visibility })
        .bind(if r.status.is_empty() { "published" } else { &r.status })
        .bind(if r.origin.is_empty() { "authored" } else { &r.origin })
        .bind(forked_from)
        .bind(moderation)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_status)?;
        let (tid, stored_moderation, created_ms, updated_ms) = row;

        let ver_id: Uuid = sqlx::query_scalar(
            "insert into template_versions (template_id, version, note, author_id) values ($1, 1, $2, $3) returning id",
        )
        .bind(tid)
        .bind(&r.note)
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_status)?;
        let rows: Vec<db::StepRow> = r.steps.iter().map(step_row).collect();
        db::insert_step_rows(&mut tx, ver_id, &rows).await.map_err(db_status)?;
        tx.commit().await.map_err(db_status)?;

        Ok(Response::new(List {
            id: tid.to_string(),
            owner_id: owner.to_string(),
            slug: r.slug,
            title: r.title,
            desc: r.desc,
            tags: r.tags,
            ordered: r.ordered,
            status: if r.status.is_empty() { "published".into() } else { r.status },
            visibility: if r.visibility.is_empty() { "public".into() } else { r.visibility },
            moderation: stored_moderation,
            moderation_reason: String::new(),
            verified: false,
            pinned: false,
            origin: if r.origin.is_empty() { "authored".into() } else { r.origin },
            forked_from_id: forked_from.map(|u| u.to_string()).unwrap_or_default(),
            current_version: 1,
            stars_count: 0,
            forks_count: 0,
            runs_count: 0,
            repository_id: tid.to_string(), // synthetic solo-repo, как в TS toList
            created_at_ms: created_ms,
            updated_at_ms: updated_ms,
        }))
    }
}
