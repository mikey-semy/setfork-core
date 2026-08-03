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
        let versions = rows
            .iter()
            .map(|r| Version {
                id: r.get::<Uuid, _>("id").to_string(),
                list_id: r.get::<Uuid, _>("template_id").to_string(),
                version: r.get("version"),
                note: r.get("note"),
                commit_sha: String::new(), // TS: всегда null
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
        let ver = Version {
            id: vid.to_string(),
            list_id: v.get::<Uuid, _>("template_id").to_string(),
            version: v.get("version"),
            note: v.get("note"),
            commit_sha: String::new(),
            created_at_ms: v.get("created_at_ms"),
            author_id: v.get::<Option<String>, _>("author_id").unwrap_or_default(),
        };

        let srows = sqlx::query(
            "select id, version_id, n, \"type\", content, title, \"desc\", command, level::text as level, \
                    why, section, subtasks, refs, image_key, needs_human, needs_human_ask \
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
        let tpl = sqlx::query("select owner_id from templates where id = $1 limit 1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_status)?;
        let Some(tpl) = tpl else {
            return Ok(Response::new(ContributorsResponse { contributors: vec![] }));
        };
        let owner_id: Uuid = tpl.get("owner_id");

        let mut out: Vec<Contributor> = Vec::new();
        let owner = sqlx::query("select handle, avatar_url from users where id = $1 limit 1")
            .bind(owner_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_status)?;
        if let Some(o) = owner {
            out.push(Contributor {
                handle: o.get("handle"),
                avatar_ref: o.get::<Option<String>, _>("avatar_url").unwrap_or_default(),
                accepted: 0, // TS: Infinity для сортировки → 0 в выдаче
            });
        }

        let rows = sqlx::query(
            "select u.handle, u.avatar_url, s.author_id, \
                    (count(*) filter (where s.status = 'accepted'))::int as accepted \
             from suggestions s join users u on u.id = s.author_id \
             where s.template_id = $1 \
             group by u.handle, u.avatar_url, s.author_id \
             order by accepted desc",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_status)?;
        for r in rows {
            let author: Uuid = r.get("author_id");
            if author == owner_id {
                continue; // владелец уже первым
            }
            out.push(Contributor {
                handle: r.get("handle"),
                avatar_ref: r.get::<Option<String>, _>("avatar_url").unwrap_or_default(),
                accepted: r.get("accepted"),
            });
        }
        Ok(Response::new(ContributorsResponse { contributors: out }))
    }
}

// ── ListWrite: addVersion / create ───────────────────────────────────────

/// ListWrite: создание списка и версий через общий движок db::add_version_rows.
pub struct ListWriteSvc {
    pub pool: PgPool,
}

// NewStep → db::StepRow: семантика TS-адаптера как есть — LocaleText без trim
// и фильтрации, imageRef → image_key, пустой level = required.
fn step_row(s: &NewStep) -> db::StepRow {
    // Блочная модель: правила нормализации type/content — в blocks (одно место).
    db::StepRow {
        block_type: storage_type(&s.r#type),
        content: content_value(&s.r#type, &s.content_json),
        // Идентичность блока сквозь версии; '' или мусор → None (фолбэк диффа).
        block_id: uuid::Uuid::parse_str(&s.block_id).ok(),
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
        version::WebVersionError::OutOfSync { have, current } => Status::failed_precondition(format!(
            "repo out of sync (git v{have}, db v{current}) — см. runbook git-projection-catchup"
        )),
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

        let mut tx = self.pool.begin().await.map_err(db_status)?;
        let row: (Uuid, i64, i64) = sqlx::query_as(
            "insert into templates (owner_id, slug, title, \"desc\", tags, ordered, visibility, status,                                     origin, forked_from_id, current_version)              values ($1, $2, $3::jsonb, $4::jsonb, $5, $6, $7::list_visibility, $8::list_status,                      $9::template_origin, $10, 1)              returning id, floor(extract(epoch from created_at) * 1000)::bigint,                        floor(extract(epoch from updated_at) * 1000)::bigint",
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
        .fetch_one(&mut *tx)
        .await
        .map_err(db_status)?;
        let (tid, created_ms, updated_ms) = row;

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
            moderation: "active".into(),
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
