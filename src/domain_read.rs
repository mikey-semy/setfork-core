// ListRead — READ-часть доменного порта ListStore на Rust (см. proto/domain_read.proto).
// Зеркалит TS-адаптер features/library/list-store.adapter.ts запрос-в-запрос,
// чтобы golden-сверка JSON совпадала. Особенности TS, сохранённые намеренно:
// - repository_id = id списка (synthetic solo-repo, как в toList);
// - Version.commit_sha всегда '' (TS отдаёт null);
// - Contributor: владелец первым (accepted у него 0), остальные по accepted desc;
//   avatar_ref = СЫРОЙ users.avatar_url (TS подписывает imgproxy-URL — это
//   презентация; в golden-сверке поле нормализуется).
use sqlx::postgres::PgPool;
use sqlx::Row;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::pb_domain::list_read_server::ListRead;
use crate::pb_domain::{
    Contributor, ContributorsResponse, GetListResponse, GetVersionRequest, GetVersionResponse,
    List, ListId, ListRef, LocaleText, Step, StepRef, Version, VersionsResponse,
};

pub struct ListReadSvc {
    pub pool: PgPool,
}

fn loc_map(v: &serde_json::Value) -> LocaleText {
    let mut m = std::collections::HashMap::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if let Some(s) = val.as_str() {
                m.insert(k.clone(), s.to_string());
            }
        }
    }
    LocaleText { v: m }
}

fn internal<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

fn parse_id(s: &str) -> Result<Uuid, Status> {
    Uuid::parse_str(s).map_err(|_| Status::invalid_argument("bad uuid"))
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
            "select id, version_id, n, title, \"desc\", command, level::text as level, \
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

// ── CurationRead: READ-часть порта CurationStore (простые exists/count) ──
use crate::pb_domain::curation_read_server::CurationRead;
use crate::pb_domain::{BoolResponse, CountResponse, IdsResponse, UserList};

pub struct CurationReadSvc {
    pub pool: PgPool,
}

#[tonic::async_trait]
impl CurationRead for CurationReadSvc {
    async fn is_starred(&self, req: Request<UserList>) -> Result<Response<BoolResponse>, Status> {
        let UserList { list_id, user_id } = req.into_inner();
        let (tid, uid) = (parse_id(&list_id)?, parse_id(&user_id)?);
        let row: Option<(i32,)> =
            sqlx::query_as("select 1 from stars where template_id = $1 and user_id = $2 limit 1")
                .bind(tid)
                .bind(uid)
                .fetch_optional(&self.pool)
                .await
                .map_err(internal)?;
        Ok(Response::new(BoolResponse { value: row.is_some() }))
    }

    async fn is_watching(&self, req: Request<UserList>) -> Result<Response<BoolResponse>, Status> {
        let UserList { list_id, user_id } = req.into_inner();
        let (tid, uid) = (parse_id(&list_id)?, parse_id(&user_id)?);
        let row: Option<(i32,)> =
            sqlx::query_as("select 1 from watches where template_id = $1 and user_id = $2 limit 1")
                .bind(tid)
                .bind(uid)
                .fetch_optional(&self.pool)
                .await
                .map_err(internal)?;
        Ok(Response::new(BoolResponse { value: row.is_some() }))
    }

    async fn watch_count(&self, req: Request<ListId>) -> Result<Response<CountResponse>, Status> {
        let tid = parse_id(&req.into_inner().id)?;
        let (n,): (i64,) = sqlx::query_as("select count(*) from watches where template_id = $1")
            .bind(tid)
            .fetch_one(&self.pool)
            .await
            .map_err(internal)?;
        Ok(Response::new(CountResponse { value: n as i32 }))
    }

    async fn watcher_ids(&self, req: Request<ListId>) -> Result<Response<IdsResponse>, Status> {
        let tid = parse_id(&req.into_inner().id)?;
        let rows: Vec<(Uuid,)> =
            sqlx::query_as("select user_id from watches where template_id = $1 order by created_at asc")
                .bind(tid)
                .fetch_all(&self.pool)
                .await
                .map_err(internal)?;
        Ok(Response::new(IdsResponse { ids: rows.iter().map(|r| r.0.to_string()).collect() }))
    }
}
