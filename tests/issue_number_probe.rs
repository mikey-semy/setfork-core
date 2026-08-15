//! ПРОБА линзы проверки ядра: нумерация задач при параллельном создании.
//!
//! `open_issue` берёт номер подзапросом `(select coalesce(max(number),0)+1 ...)`
//! внутри INSERT, без блокировки. В ПРОДОВОЙ схеме фронта на (template_id, number)
//! висит uniqueIndex `issues_tpl_number` — в тестовой схеме ядра
//! (tests/support/mod.rs) его НЕТ, поэтому гонка тестами ядра невоспроизводима.
//!
//! Здесь индекс добавляется в свою эфемерную схему, чтобы увидеть, что реально
//! происходит на проде при одновременном создании задач в одном списке.
mod support;

use setfork_core::pb_domain::collab_write_server::CollabWrite;
use setfork_core::pb_domain::list_write_server::ListWrite;
use setfork_core::pb_domain::{CreateListRequest, CreateSuggestionRequest, LocaleText, NewStep, OpenIssueRequest};
use setfork_core::services::collab::CollabWriteSvc;
use setfork_core::services::list::ListWriteSvc;
use tonic::Request;

fn lt(s: &str) -> Option<LocaleText> {
    Some(LocaleText { v: [("en".to_string(), s.to_string())].into_iter().collect() })
}

fn step(title: &str) -> NewStep {
    NewStep {
        block_id: String::new(),
        title: lt(title),
        desc: lt("d"),
        command: String::new(),
        level: "required".into(),
        why: None,
        section: None,
        subtasks: vec![],
        refs: vec![],
        image_ref: String::new(),
        r#type: String::new(),
        content_json: String::new(),
        needs_human: false,
        needs_human_ask: None,
    }
}

#[tokio::test]
#[ignore = "ПАДАЕТ (дефект): 4 из 8, остальным AlreadyExists — нет повтора со следующим номером"]
async fn параллельное_создание_задач_на_продовой_схеме() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "alice").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let created = write
        .create(Request::new(CreateListRequest {
            owner_id: owner.to_string(),
            slug: "issue-race".into(),
            title: lt("Probe"),
            desc: lt("d"),
            tags: vec![],
            ordered: true,
            visibility: String::new(),
            status: String::new(),
            origin: String::new(),
            forked_from_id: String::new(),
            note: "v1".into(),
            steps: vec![step("Шаг")],
        }))
        .await
        .expect("create")
        .into_inner();

    // Догоняем тестовую схему до продовой ровно в этом месте.
    sqlx::query("create unique index issues_tpl_number on issues (template_id, number)")
        .execute(&pool)
        .await
        .expect("unique index как в схеме фронта");

    let collab = CollabWriteSvc { pool: pool.clone() };
    let n = 8;
    let mut set = tokio::task::JoinSet::new();
    for i in 0..n {
        let c = CollabWriteSvc { pool: collab.pool.clone() };
        let (lid, aid) = (created.id.clone(), owner.to_string());
        set.spawn(async move {
            c.open_issue(Request::new(OpenIssueRequest {
                list_id: lid,
                author_id: aid,
                title: format!("задача {i}"),
                body: String::new(),
                labels: vec![],
            }))
            .await
            .map(|r| r.into_inner().number)
            .map_err(|e| (e.code(), e.message().to_string()))
        });
    }

    let mut ok = Vec::new();
    let mut err = Vec::new();
    while let Some(res) = set.join_next().await {
        match res.expect("join") {
            Ok(num) => ok.push(num),
            Err(e) => err.push(e),
        }
    }
    ok.sort_unstable();
    println!("СОЗДАНО {} из {n}: номера {ok:?}", ok.len());
    for (code, msg) in &err {
        println!("ОТКАЗ: {code:?} — {msg}");
    }

    // Сколько задач реально в БД.
    let cnt: i64 = sqlx::query_scalar("select count(*) from issues").fetch_one(&pool).await.expect("count");
    println!("В БД: {cnt}");

    assert_eq!(ok.len(), n as usize, "все {n} задач обязаны создаться, отказов быть не должно");
    let mut uniq = ok.clone();
    uniq.dedup();
    assert_eq!(uniq.len(), ok.len(), "номера задач обязаны быть уникальны: {ok:?}");
}

/// То же самое для ПРЕДЛОЖЕНИЙ: в продовой схеме на (template_id, number) висит
/// uniqueIndex `suggestions_tpl_number` (schema.ts:808), номер берётся тем же
/// подзапросом без блокировки. Автосоздание предложений гномами делает этот путь
/// не гипотетическим.
#[tokio::test]
#[ignore = "ПАДАЕТ (дефект): половина отказов вместо ретрая номера"]
async fn параллельное_создание_предложений_на_продовой_схеме() {
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "bob").await;
    let write = ListWriteSvc { pool: pool.clone() };
    let created = write
        .create(Request::new(CreateListRequest {
            owner_id: owner.to_string(),
            slug: "sug-race".into(),
            title: lt("Probe"),
            desc: lt("d"),
            tags: vec![],
            ordered: true,
            visibility: String::new(),
            status: String::new(),
            origin: String::new(),
            forked_from_id: String::new(),
            note: "v1".into(),
            steps: vec![step("Шаг")],
        }))
        .await
        .expect("create")
        .into_inner();

    sqlx::query("create unique index suggestions_tpl_number on suggestions (template_id, number)")
        .execute(&pool)
        .await
        .expect("unique index как в схеме фронта");

    let n = 8;
    let mut set = tokio::task::JoinSet::new();
    for i in 0..n {
        let c = CollabWriteSvc { pool: pool.clone() };
        let (lid, aid) = (created.id.clone(), owner.to_string());
        set.spawn(async move {
            c.create_suggestion(Request::new(CreateSuggestionRequest {
                list_id: lid,
                author_id: aid,
                note: format!("предложение {i}"),
                steps: vec![],
            }))
            .await
            .map(|r| r.into_inner().id)
            .map_err(|e| (e.code(), e.message().to_string()))
        });
    }
    let mut ok = 0;
    let mut err = Vec::new();
    while let Some(res) = set.join_next().await {
        match res.expect("join") {
            Ok(_) => ok += 1,
            Err(e) => err.push(e),
        }
    }
    let cnt: i64 = sqlx::query_scalar("select count(*) from suggestions").fetch_one(&pool).await.expect("count");
    println!("ПРЕДЛОЖЕНИЙ создано {ok} из {n}, в БД {cnt}");
    for (code, msg) in &err {
        println!("  ОТКАЗ: {code:?} — {msg}");
    }
    assert_eq!(ok, n, "все {n} предложений обязаны создаться");
}
