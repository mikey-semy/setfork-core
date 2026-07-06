// ListWrite — первая WRITE-операция домена на Rust: addVersion.
// Полная семантика TS-адаптера list-store.adapter.ts::addVersion (в отличие от
// db::add_version, который упрощён под git-проекцию): LocaleText сохраняется как
// есть, imageRef → has_image/image_key, bump current_version + updated_at — всё
// в ОДНОЙ транзакции.
use sqlx::postgres::PgPool;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::pb_domain::list_write_server::ListWrite;
use crate::pb_domain::{AddVersionRequest, CreateListRequest, List, LocaleText, NewStep, StepRef, Version};

pub struct ListWriteSvc {
    pub pool: PgPool,
}

fn internal<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

fn loc_json(l: &Option<LocaleText>) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    if let Some(lt) = l {
        for (k, v) in &lt.v {
            m.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
    }
    serde_json::Value::Object(m)
}

fn refs_json(refs: &[StepRef]) -> serde_json::Value {
    serde_json::Value::Array(
        refs.iter()
            .map(|r| {
                let mut m = serde_json::Map::new();
                m.insert("label".into(), loc_json(&r.label));
                if !r.url.is_empty() {
                    m.insert("url".into(), serde_json::Value::String(r.url.clone()));
                }
                serde_json::Value::Object(m)
            })
            .collect(),
    )
}

async fn insert_steps(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ver_id: Uuid,
    steps: &[NewStep],
) -> Result<(), Status> {
        for (i, s) in steps.iter().enumerate() {
            let image_ref = if s.image_ref.is_empty() { None } else { Some(s.image_ref.clone()) };
            let subtasks =
                serde_json::Value::Array(s.subtasks.iter().map(|t| loc_json(&Some(t.clone()))).collect());
            // Блочная модель: не-step блоки несут type/content; у шага — 'step'/{}.
            let is_step = s.r#type.is_empty() || s.r#type == "step";
            let block_type = if is_step { "step" } else { s.r#type.as_str() };
            let content: serde_json::Value = if is_step || s.content_json.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&s.content_json).unwrap_or_else(|_| serde_json::json!({}))
            };
            sqlx::query(
                "insert into steps (version_id, n, type, content, title, \"desc\", command, has_image, image_key, \
                                    level, why, section, subtasks, refs) \
                 values ($1, $2, $3, $4::jsonb, $5::jsonb, $6::jsonb, $7, $8, $9, $10::step_level, $11::jsonb, $12::jsonb, $13::jsonb, $14::jsonb)",
            )
            .bind(ver_id)
            .bind((i as i32) + 1)
            .bind(block_type)
            .bind(content)
            .bind(loc_json(&s.title))
            .bind(loc_json(&s.desc))
            .bind(&s.command)
            .bind(image_ref.is_some())
            .bind(image_ref)
            .bind(if s.level.is_empty() { "required" } else { &s.level })
            .bind(loc_json(&s.why))
            .bind(loc_json(&s.section))
            .bind(subtasks)
            .bind(refs_json(&s.refs))
            .execute(&mut **tx)
            .await
            .map_err(internal)?;
        }
    Ok(())
}

#[tonic::async_trait]
impl ListWrite for ListWriteSvc {
    async fn add_version(&self, req: Request<AddVersionRequest>) -> Result<Response<Version>, Status> {
        let AddVersionRequest { list_id, note, steps } = req.into_inner();
        let tid = Uuid::parse_str(&list_id).map_err(|_| Status::invalid_argument("bad uuid"))?;

        let mut tx = self.pool.begin().await.map_err(internal)?;
        let current: Option<i32> = sqlx::query_scalar("select current_version from templates where id = $1 for update")
            .bind(tid)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?;
        let Some(current) = current else {
            return Err(Status::not_found("list not found"));
        };
        let new_version = current + 1;

        let row: (Uuid, i64) = sqlx::query_as(
            "insert into template_versions (template_id, version, note) values ($1, $2, $3) \
             returning id, floor(extract(epoch from created_at) * 1000)::bigint",
        )
        .bind(tid)
        .bind(new_version)
        .bind(&note)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;
        let (ver_id, created_ms) = row;

        insert_steps(&mut tx, ver_id, &steps).await?;

        sqlx::query("update templates set current_version = $2, updated_at = now() where id = $1")
            .bind(tid)
            .bind(new_version)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;

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
        let _ = ver_id;

        insert_steps(&mut tx, ver_id, &r.steps).await?;
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

// ── CollabWrite: issues/suggestions/comments (write) ─────────────────
// Зеркалит src/features/collab-store/adapter.ts. Даты — unix-ms (0 = null).
use crate::pb_domain::collab_write_server::CollabWrite;
use crate::pb_domain::{
    AddIssueCommentRequest, AddSuggestionCommentRequest, BoolResponse, CreateSuggestionRequest, Issue, IssueComment,
    OpenIssueRequest, SetIssueStatusRequest, Suggestion, SuggestionComment,
};

fn pid(s: &str) -> Result<Uuid, Status> {
    Uuid::parse_str(s).map_err(|_| Status::invalid_argument("bad uuid"))
}

// LocaleText-обёртка из jsonb-объекта {lang: str}.
fn loc_from_obj(v: Option<&serde_json::Value>) -> Option<LocaleText> {
    v.and_then(|x| x.as_object()).map(|o| LocaleText {
        v: o.iter().filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string()))).collect(),
    })
}

// ProposedItem-jsonb (как domainStepToProposed в adapter.ts): image_key опускаем,
// если пусто (undefined в TS → ключа нет). Порядок ключей не важен — golden
// сравнивает канонически.
fn proposed_json(s: &NewStep) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert("title".into(), loc_json(&s.title));
    m.insert("desc".into(), loc_json(&s.desc));
    m.insert("command".into(), serde_json::Value::String(s.command.clone()));
    m.insert("hasImage".into(), serde_json::Value::Bool(!s.image_ref.is_empty()));
    if !s.image_ref.is_empty() {
        m.insert("imageKey".into(), serde_json::Value::String(s.image_ref.clone()));
    }
    m.insert("level".into(), serde_json::Value::String(s.level.clone()));
    m.insert("why".into(), loc_json(&s.why));
    m.insert("section".into(), loc_json(&s.section));
    m.insert(
        "subtasks".into(),
        serde_json::Value::Array(s.subtasks.iter().map(|t| loc_json(&Some(t.clone()))).collect()),
    );
    m.insert("refs".into(), refs_json(&s.refs));
    // Блочная модель: type/content — только у не-step блоков (как toProposedItems в TS).
    if !(s.r#type.is_empty() || s.r#type == "step") {
        m.insert("type".into(), serde_json::Value::String(s.r#type.clone()));
        let content = if s.content_json.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&s.content_json).unwrap_or_else(|_| serde_json::json!({}))
        };
        m.insert("content".into(), content);
    }
    serde_json::Value::Object(m)
}

// ProposedItem-jsonb → NewStep (обратно, для возврата Suggestion.steps).
fn json_to_step(v: &serde_json::Value) -> NewStep {
    let subtasks = v
        .get("subtasks")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|it| loc_from_obj(Some(it))).collect())
        .unwrap_or_default();
    let refs = v
        .get("refs")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|it| it.as_object())
                .map(|o| StepRef {
                    label: loc_from_obj(o.get("label")),
                    url: o.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    NewStep {
        title: loc_from_obj(v.get("title")),
        desc: loc_from_obj(v.get("desc")),
        command: v.get("command").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        level: v.get("level").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        why: loc_from_obj(v.get("why")),
        section: loc_from_obj(v.get("section")),
        subtasks,
        refs,
        image_ref: v.get("imageKey").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        // type/content_json — только у не-step блоков.
        r#type: v.get("type").and_then(|x| x.as_str()).filter(|t| *t != "step").unwrap_or("").to_string(),
        content_json: {
            let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
            if ty.is_empty() || ty == "step" {
                String::new()
            } else {
                v.get("content").map(|c| c.to_string()).unwrap_or_default()
            }
        },
    }
}

pub struct CollabWriteSvc {
    pub pool: PgPool,
}

#[tonic::async_trait]
impl CollabWrite for CollabWriteSvc {
    async fn open_issue(&self, req: Request<OpenIssueRequest>) -> Result<Response<Issue>, Status> {
        use sqlx::Row;
        let OpenIssueRequest { list_id, author_id, title, body, labels } = req.into_inner();
        let (tid, uid) = (pid(&list_id)?, pid(&author_id)?);
        let labels_json = serde_json::Value::Array(labels.iter().map(|l| serde_json::Value::String(l.clone())).collect());
        let r = sqlx::query(
            "insert into issues (template_id, author_id, title, body, labels, number) \
             values ($1, $2, $3, $4, $5::jsonb, \
               (select coalesce(max(number), 0) + 1 from issues where template_id = $1)) \
             returning id, number, status::text as status, \
               floor(extract(epoch from created_at) * 1000)::bigint as c, \
               floor(extract(epoch from updated_at) * 1000)::bigint as u",
        )
        .bind(tid)
        .bind(uid)
        .bind(&title)
        .bind(&body)
        .bind(&labels_json)
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?;
        Ok(Response::new(Issue {
            id: r.get::<Uuid, _>("id").to_string(),
            list_id,
            number: r.get::<i32, _>("number"),
            author_id,
            title,
            body,
            status: r.get::<String, _>("status"),
            labels,
            created_at: r.get::<i64, _>("c"),
            updated_at: r.get::<i64, _>("u"),
            closed_at: 0,
        }))
    }

    async fn add_issue_comment(&self, req: Request<AddIssueCommentRequest>) -> Result<Response<IssueComment>, Status> {
        use sqlx::Row;
        let AddIssueCommentRequest { issue_id, author_id, body } = req.into_inner();
        let (iid, uid) = (pid(&issue_id)?, pid(&author_id)?);
        let r = sqlx::query(
            "insert into issue_comments (issue_id, author_id, body) values ($1, $2, $3) \
             returning id, floor(extract(epoch from created_at) * 1000)::bigint as c",
        )
        .bind(iid)
        .bind(uid)
        .bind(&body)
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?;
        Ok(Response::new(IssueComment {
            id: r.get::<Uuid, _>("id").to_string(),
            issue_id,
            author_id,
            body,
            created_at: r.get::<i64, _>("c"),
        }))
    }

    async fn set_issue_status(&self, req: Request<SetIssueStatusRequest>) -> Result<Response<BoolResponse>, Status> {
        let SetIssueStatusRequest { issue_id, status } = req.into_inner();
        let iid = pid(&issue_id)?;
        sqlx::query(
            "update issues set status = $2::issue_status, \
               closed_at = case when $2 = 'closed' then now() else null end, updated_at = now() \
             where id = $1",
        )
        .bind(iid)
        .bind(&status)
        .execute(&self.pool)
        .await
        .map_err(internal)?;
        Ok(Response::new(BoolResponse { value: true }))
    }

    async fn create_suggestion(&self, req: Request<CreateSuggestionRequest>) -> Result<Response<Suggestion>, Status> {
        use sqlx::Row;
        let CreateSuggestionRequest { list_id, author_id, note, steps } = req.into_inner();
        let (tid, uid) = (pid(&list_id)?, pid(&author_id)?);
        let items = serde_json::Value::Array(steps.iter().map(proposed_json).collect());
        let r = sqlx::query(
            "insert into suggestions (template_id, author_id, note, base_version, items) \
             values ($1, $2, $3, coalesce((select current_version from templates where id = $1), 1), $4::jsonb) \
             returning id, status::text as status, base_version, items, \
               floor(extract(epoch from created_at) * 1000)::bigint as c",
        )
        .bind(tid)
        .bind(uid)
        .bind(&note)
        .bind(&items)
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?;
        let items_back: serde_json::Value = r.get("items");
        let out_steps = items_back.as_array().map(|a| a.iter().map(json_to_step).collect()).unwrap_or_default();
        Ok(Response::new(Suggestion {
            id: r.get::<Uuid, _>("id").to_string(),
            list_id,
            author_id,
            status: r.get::<String, _>("status"),
            note,
            base_version: r.get::<i32, _>("base_version"),
            steps: out_steps,
            created_at: r.get::<i64, _>("c"),
            resolved_at: 0,
        }))
    }

    async fn add_suggestion_comment(&self, req: Request<AddSuggestionCommentRequest>) -> Result<Response<SuggestionComment>, Status> {
        use sqlx::Row;
        let AddSuggestionCommentRequest { suggestion_id, author_id, body } = req.into_inner();
        let (sid, uid) = (pid(&suggestion_id)?, pid(&author_id)?);
        let r = sqlx::query(
            "insert into suggestion_comments (suggestion_id, author_id, body) values ($1, $2, $3) \
             returning id, floor(extract(epoch from created_at) * 1000)::bigint as c",
        )
        .bind(sid)
        .bind(uid)
        .bind(&body)
        .fetch_one(&self.pool)
        .await
        .map_err(internal)?;
        Ok(Response::new(SuggestionComment {
            id: r.get::<Uuid, _>("id").to_string(),
            suggestion_id,
            author_id,
            body,
            created_at: r.get::<i64, _>("c"),
        }))
    }
}
