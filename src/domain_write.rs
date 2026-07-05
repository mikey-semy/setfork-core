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
            sqlx::query(
                "insert into steps (version_id, n, title, \"desc\", command, has_image, image_key, \
                                    level, why, section, subtasks, refs) \
                 values ($1, $2, $3::jsonb, $4::jsonb, $5, $6, $7, $8::step_level, $9::jsonb, $10::jsonb, $11::jsonb, $12::jsonb)",
            )
            .bind(ver_id)
            .bind((i as i32) + 1)
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
