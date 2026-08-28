//! Негодный `block_id` идентичность теряет — но теперь НЕ молча.
//!
//! Контракт (`proto/domain_read.proto`, `NewStep.block_id`): пустая строка = «идентичность
//! неизвестна», и это законно. Непустое негодное значение контрактом не описано, а ядро его
//! молча выбрасывало: вызывающий идентичность прислал, получил успех, и дифф после этого
//! читает переименование как «удалён + добавлен».
//!
//! Отказывать нельзя — прежнее поведение такой запрос принимало, и ужесточение сломало бы
//! вызывающих. Поэтому поведение оставлено, а тишина убрана. Тест держит ОБЕ половины:
//! запрос по-прежнему проходит, и след в журнале есть.
//!
//! Разбор: setfork-hq/reviews/vertical/2026-08-27-assemble-list-ledger.md (гипотеза слоёв 8–9,
//! закрыта прогоном 28.08).
mod support;

use setfork_core::pb_domain::list_write_server::ListWrite;
use setfork_core::pb_domain::{CreateListRequest, LocaleText, NewStep};
use setfork_core::services::list::ListWriteSvc;
use tonic::Request;
use uuid::Uuid;

fn step_with_bid(bid: &str) -> NewStep {
    NewStep {
        block_id: bid.into(),
        title: Some(LocaleText { v: [("en".to_string(), "Install".to_string())].into() }),
        desc: None,
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
        danger: false,
    }
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn broken_block_id_drops_identity_but_leaves_a_trace() {
    let log = support::LogSink::install(tracing::Level::WARN);
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "alice").await;
    let write = ListWriteSvc { pool: pool.clone() };

    let good = Uuid::new_v4().to_string();
    let created = write
        .create(Request::new(CreateListRequest {
            owner_id: owner.to_string(),
            slug: "mixed-identities".into(),
            title: Some(LocaleText { v: [("en".to_string(), "T".to_string())].into() }),
            desc: None,
            tags: vec![],
            ordered: true,
            visibility: String::new(),
            status: String::new(),
            origin: String::new(),
            forked_from_id: String::new(),
            note: "n".into(),
            // Три шага: годная идентичность, пустая (законно) и мусор.
            steps: vec![step_with_bid(&good), step_with_bid(""), step_with_bid("не-uuid-вовсе")],
            moderation: String::new(),
        }))
        .await
        .expect("запрос с негодным block_id по-прежнему ПРОХОДИТ — контракт не ужесточаем")
        .into_inner();

    let tid = Uuid::parse_str(&created.id).expect("id");
    let ids: Vec<(i32, Option<Uuid>)> = sqlx::query_as(
        "select s.n, s.block_id from steps s join template_versions v on v.id = s.version_id \
         where v.template_id = $1 order by s.n",
    )
    .bind(tid)
    .fetch_all(&pool)
    .await
    .expect("шаги читаются");

    assert_eq!(ids.len(), 3, "все три шага записаны");
    assert_eq!(ids[0].1.map(|u| u.to_string()).as_deref(), Some(good.as_str()), "годная дошла");
    assert_eq!(ids[1].1, None, "пустая — законное «неизвестна»");
    assert_eq!(ids[2].1, None, "мусор идентичностью не становится");

    // И вторая половина: потеря видна. Раньше здесь была тишина.
    let text = log.text();
    assert!(
        text.contains("block_id is not a uuid"),
        "потеря идентичности снова молчит — оператор не узнает, что кто-то шлёт мусор:\n{text}"
    );
    // Пустая строка — законный случай и жаловаться на него НЕ должна: иначе журнал
    // забьётся шумом на каждом шаге без идентичности, и настоящая строка в нём утонет.
    assert_eq!(text.matches("block_id is not a uuid").count(), 1, "жалоба ровно одна, на мусор");
}
