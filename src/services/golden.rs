//! Golden-сверка READ-портов с TS: канонический JSON (camelCase, '' → null) —
//! сравнивается со скриптом scripts/golden-domain-read.ts фронта и с
//! фикстурами tests/golden.rs. avatarRef нормализуется в null на ОБЕИХ
//! сторонах (TS подписывает imgproxy-URL — недетерминированно).
//! Это инструмент сверки (CLI `domain-read` в main.rs), не RPC.
use sqlx::postgres::PgPool;
use tonic::Request;

use super::list::ListReadSvc;
use crate::pb_domain::list_read_server::ListRead;
use crate::pb_domain::{GetVersionRequest, ListId, ListRef, LocaleText, Version};

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
    if s.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(s.to_string()) }
}

fn jver(v: &Version) -> serde_json::Value {
    serde_json::json!({
        "id": v.id, "listId": v.list_id, "version": v.version, "note": v.note,
        // ⚠️ SHA гасится НАМЕРЕННО, а не забыт: с 31.08 ядро читает его из тегов канона
        // (`list.rs::version_shas`), а у TS такого поля нет вовсе. Это инструмент СВЕРКИ
        // двух реализаций — поле, которого одна сторона дать не может, нормализуется здесь
        // так же, как `avatarRef` выше. Пробросишь `v.commit_sha` — сверка начнёт краснеть
        // на каждом списке, и покажет она не расхождение, а разную зрелость сторон.
        //
        // ⚠️ И обратное, ради чего эта строка написана: из-за нормализации `domain-read`
        // НЕ ГОДИТСЯ как инструмент проверки самого SHA — он тут всегда null. 31.08 на этом
        // едва не закрылась ложная приёмка «SHA пустой на проде». Смотреть SHA надо там,
        // где его отдаёт RPC, а не здесь.
        "commitSha": serde_json::Value::Null,
        "createdAtMs": v.created_at_ms,
    })
}

pub async fn golden_json(
    pool: &PgPool,
    owner: &str,
    slug: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let svc = ListReadSvc { pool: pool.clone() };
    let gl =
        svc.get_list(Request::new(ListRef { owner: owner.into(), slug: slug.into() })).await?.into_inner();
    if !gl.found {
        return Ok(serde_json::json!({ "found": false }));
    }
    let l = gl.list.unwrap();
    let versions = svc.list_versions(Request::new(ListId { id: l.id.clone() })).await?.into_inner().versions;
    let cur = svc
        .get_version(Request::new(GetVersionRequest { list_id: l.id.clone(), version: l.current_version }))
        .await?
        .into_inner();
    let contributors =
        svc.get_contributors(Request::new(ListId { id: l.id.clone() })).await?.into_inner().contributors;

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
