// CollabWrite — issues/suggestions/comments (write).
// Зеркалит src/features/collab-store/adapter.ts. Даты — unix-ms (0 = null).
use sqlx::postgres::PgPool;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::util::{internal, loc_json, parse_id, refs_json};
use crate::pb_domain::collab_write_server::CollabWrite;
use crate::pb_domain::{
    AddIssueCommentRequest, AddSuggestionCommentRequest, BoolResponse, CreateSuggestionRequest, Issue, IssueComment,
    LocaleText, NewStep, OpenIssueRequest, SetIssueStatusRequest, StepRef, Suggestion, SuggestionComment,
};

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
        let (tid, uid) = (parse_id(&list_id)?, parse_id(&author_id)?);
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
        let (iid, uid) = (parse_id(&issue_id)?, parse_id(&author_id)?);
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
        let iid = parse_id(&issue_id)?;
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
        let (tid, uid) = (parse_id(&list_id)?, parse_id(&author_id)?);
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
        let (sid, uid) = (parse_id(&suggestion_id)?, parse_id(&author_id)?);
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
