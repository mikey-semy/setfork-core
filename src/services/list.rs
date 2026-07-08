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
use sqlx::postgres::PgPool;
use sqlx::Row;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::util::{internal, loc_json, loc_map, parse_id, refs_json};
use crate::blocks::is_step_type;
use crate::db;
use crate::pb_domain::list_read_server::ListRead;
use crate::pb_domain::list_write_server::ListWrite;
use crate::pb_domain::{
    AddVersionRequest, Contributor, ContributorsResponse, CreateListRequest, GetListResponse,
    GetVersionRequest, GetVersionResponse, List, ListId, ListRef, LocaleText, NewStep, Step,
    StepRef, Version, VersionsResponse,
};

/// ListRead: чтение списков/версий/шагов (зеркало list-store.adapter.ts).
pub struct ListReadSvc {
    pub pool: PgPool,
}

// ── Golden-сверка с TS ────────────────────────────────────────────────
// Канонический JSON (camelCase, '' → null) — сравнивается со скриптом
// scripts/golden-domain-read.ts на TS-стороне. avatarRef нормализуется в null
// на ОБЕИХ сторонах (TS подписывает imgproxy-URL — недетерминированно).

fn jloc(l: &Option<LocaleText>) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    if let Some(lt) = l {
        let mut keys: Vec<_> = lt.v.keys().collect();
        keys.sort();
        for k in keys {
            m.insert(k.clone(), serde_json::Value::String(lt.v[k].clone()));
        }
    }
    serde_json::Value::Object(m)
}

fn jnull(s: &str) -> serde_json::Value {
    if s.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(s.to_string())
    }
}

fn jver(v: &Version) -> serde_json::Value {
    serde_json::json!({
        "id": v.id, "listId": v.list_id, "version": v.version, "note": v.note,
        "commitSha": serde_json::Value::Null, "createdAtMs": v.created_at_ms,
    })
}

pub async fn golden_json(
    pool: &PgPool,
    owner: &str,
    slug: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let svc = ListReadSvc { pool: pool.clone() };
    let gl = svc
        .get_list(Request::new(ListRef { owner: owner.into(), slug: slug.into() }))
        .await?
        .into_inner();
    if !gl.found {
        return Ok(serde_json::json!({ "found": false }));
    }
    let l = gl.list.unwrap();
    let versions = svc
        .list_versions(Request::new(ListId { id: l.id.clone() }))
        .await?
        .into_inner()
        .versions;
    let cur = svc
        .get_version(Request::new(GetVersionRequest { list_id: l.id.clone(), version: l.current_version }))
        .await?
        .into_inner();
    let contributors = svc
        .get_contributors(Request::new(ListId { id: l.id.clone() }))
        .await?
        .into_inner()
        .contributors;

    let steps: Vec<serde_json::Value> = cur
        .steps
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id, "versionId": s.version_id, "n": s.n,
                "title": jloc(&s.title), "desc": jloc(&s.desc), "command": s.command,
                "level": s.level, "why": jloc(&s.why), "section": jloc(&s.section),
                "subtasks": s.subtasks.iter().map(|t| jloc(&Some(t.clone()))).collect::<Vec<_>>(),
                "refs": s.refs.iter().map(|r| serde_json::json!({ "label": jloc(&r.label), "url": jnull(&r.url) })).collect::<Vec<_>>(),
                "imageRef": jnull(&s.image_ref),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "found": true,
        "list": {
            "id": l.id, "ownerId": l.owner_id, "slug": l.slug,
            "title": jloc(&l.title), "desc": jloc(&l.desc), "tags": l.tags,
            "ordered": l.ordered, "status": l.status, "visibility": l.visibility,
            "moderation": l.moderation, "moderationReason": jnull(&l.moderation_reason),
            "verified": l.verified, "pinned": l.pinned, "origin": l.origin,
            "forkedFromId": jnull(&l.forked_from_id), "currentVersion": l.current_version,
            "starsCount": l.stars_count, "forksCount": l.forks_count, "runsCount": l.runs_count,
            "repositoryId": l.repository_id,
            "createdAtMs": l.created_at_ms, "updatedAtMs": l.updated_at_ms,
        },
        "versions": versions.iter().map(jver).collect::<Vec<_>>(),
        "current": {
            "version": cur.version.as_ref().map(jver).unwrap_or(serde_json::Value::Null),
            "steps": steps,
        },
        "contributors": contributors.iter().map(|c| serde_json::json!({
            "handle": c.handle, "avatarRef": serde_json::Value::Null, "accepted": c.accepted,
        })).collect::<Vec<_>>(),
    }))
}

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
        .map_err(internal)?;

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
            "select id, template_id, version, note, \
                    floor(extract(epoch from created_at) * 1000)::bigint as created_at_ms \
             from template_versions where template_id = $1 order by version desc",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        let versions = rows
            .iter()
            .map(|r| Version {
                id: r.get::<Uuid, _>("id").to_string(),
                list_id: r.get::<Uuid, _>("template_id").to_string(),
                version: r.get("version"),
                note: r.get("note"),
                commit_sha: String::new(), // TS: всегда null
                created_at_ms: r.get("created_at_ms"),
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
            "select id, template_id, version, note, \
                    floor(extract(epoch from created_at) * 1000)::bigint as created_at_ms \
             from template_versions where template_id = $1 and version = $2 limit 1",
        )
        .bind(id)
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?;
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
        };

        let srows = sqlx::query(
            "select id, version_id, n, \"type\", content, title, \"desc\", command, level::text as level, \
                    why, section, subtasks, refs, image_key \
             from steps where version_id = $1 order by n asc",
        )
        .bind(vid)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
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
                let block_ty = {
                    let t = s.get::<Option<String>, _>("type").unwrap_or_default();
                    if is_step_type(&t) { String::new() } else { t }
                };
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

    async fn get_contributors(
        &self,
        req: Request<ListId>,
    ) -> Result<Response<ContributorsResponse>, Status> {
        let id = parse_id(&req.into_inner().id)?;
        let tpl = sqlx::query("select owner_id from templates where id = $1 limit 1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?;
        let Some(tpl) = tpl else {
            return Ok(Response::new(ContributorsResponse { contributors: vec![] }));
        };
        let owner_id: Uuid = tpl.get("owner_id");

        let mut out: Vec<Contributor> = Vec::new();
        let owner = sqlx::query("select handle, avatar_url from users where id = $1 limit 1")
            .bind(owner_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?;
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
        .map_err(internal)?;
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
    // Блочная модель: не-step блоки несут type/content; у шага — 'step'/{}.
    let is_step = is_step_type(&s.r#type);
    db::StepRow {
        block_type: if is_step { "step".into() } else { s.r#type.clone() },
        content: if is_step || s.content_json.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&s.content_json).unwrap_or_else(|_| serde_json::json!({}))
        },
        title: loc_json(&s.title),
        desc: loc_json(&s.desc),
        command: s.command.clone(),
        image_key: if s.image_ref.is_empty() { None } else { Some(s.image_ref.clone()) },
        level: db::Level::parse(&s.level),
        why: loc_json(&s.why),
        section: loc_json(&s.section),
        subtasks: serde_json::Value::Array(s.subtasks.iter().map(|t| loc_json(&Some(t.clone()))).collect()),
        refs: refs_json(&s.refs),
    }
}

#[tonic::async_trait]
impl ListWrite for ListWriteSvc {
    async fn add_version(&self, req: Request<AddVersionRequest>) -> Result<Response<Version>, Status> {
        let AddVersionRequest { list_id, note, steps } = req.into_inner();
        let tid = parse_id(&list_id)?;
        let rows: Vec<db::StepRow> = steps.iter().map(step_row).collect();
        // Транзакция «новая версия» общая с git-проекцией — db::add_version_rows.
        let Some((ver_id, new_version, created_ms)) =
            db::add_version_rows(&self.pool, tid, &note, &rows).await.map_err(internal)?
        else {
            return Err(Status::not_found("list not found"));
        };

        Ok(Response::new(Version {
            id: ver_id.to_string(),
            list_id: tid.to_string(),
            version: new_version,
            note,
            commit_sha: String::new(),
            created_at_ms: created_ms,
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

        let mut tx = self.pool.begin().await.map_err(internal)?;
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
        .map_err(internal)?;
        let (tid, created_ms, updated_ms) = row;

        let ver_id: Uuid = sqlx::query_scalar(
            "insert into template_versions (template_id, version, note) values ($1, 1, $2) returning id",
        )
        .bind(tid)
        .bind(&r.note)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;
        let rows: Vec<db::StepRow> = r.steps.iter().map(step_row).collect();
        db::insert_step_rows(&mut tx, ver_id, &rows).await.map_err(internal)?;
        tx.commit().await.map_err(internal)?;

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
