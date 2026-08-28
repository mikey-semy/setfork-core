//! Журнал обязан называть ПРИЧИНУ отказа, а не только код gRPC.
//!
//! Код грубее причины: замер 27.08.2026 показал, что под `FailedPrecondition` живут четыре
//! причины (ARCHIVED, CONFLICT, FROZEN, PROTECTED), под `InvalidArgument` — две. Оператор,
//! читавший журнал, видел код и не мог сказать, какой отказ сработал, хотя ядро причину уже
//! вычислило и отправило клиенту.
//!
//! Проверяется через НАСТОЯЩИЙ сервер с телеметрическим слоем и настоящий клиент: слой читает
//! заголовки ответа, и вопрос «доезжает ли туда трейлер» чтением кода не решается.
mod support;

use setfork_core::pb_domain::list_write_client::ListWriteClient;
use setfork_core::pb_domain::list_write_server::ListWriteServer;
use setfork_core::pb_domain::{AddVersionRequest, CreateListRequest, LocaleText};
use setfork_core::services::list::ListWriteSvc;

fn req(owner: &str, slug: &str) -> CreateListRequest {
    CreateListRequest {
        owner_id: owner.into(),
        slug: slug.into(),
        title: Some(LocaleText { v: [("en".to_string(), "T".to_string())].into() }),
        desc: None,
        tags: vec![],
        ordered: true,
        visibility: String::new(),
        status: String::new(),
        origin: String::new(),
        forked_from_id: String::new(),
        note: "n".into(),
        steps: vec![],
        moderation: String::new(),
    }
}

#[tokio::test]
#[ignore = "нужен TEST_DATABASE_URL (Postgres)"]
async fn rpc_error_line_names_the_reason() {
    let log = support::LogSink::install(tracing::Level::WARN);

    let _gd = support::own_git_data_dir("log-reason").await;
    let pool = support::pool_with_schema().await;
    let owner = support::seed_user(&pool, "alice").await.to_string();

    // Свободный порт: занимаем, узнаём номер, отпускаем — сервер сядет на него сам.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = probe.local_addr().expect("addr");
    drop(probe);

    let svc = ListWriteSvc { pool: pool.clone() };
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .layer(setfork_core::telemetry::TelemetryLayer)
            .add_service(ListWriteServer::new(svc))
            .serve(addr)
            .await;
    });

    let mut client = {
        let mut attempt = None;
        for _ in 0..50 {
            match ListWriteClient::connect(format!("http://{addr}")).await {
                Ok(c) => {
                    attempt = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
            }
        }
        attempt.expect("сервер не поднялся за секунду")
    };

    client.create(req(&owner, "taken")).await.expect("первый проходит");
    let err = client.create(req(&owner, "taken")).await.expect_err("второй обязан упасть");
    assert_eq!(err.code(), tonic::Code::AlreadyExists, "код тот, что ожидаем");

    // И ТО ЖЕ на пути ПРАВКИ — там, где причину якобы читает фронт.
    let made = client.create(req(&owner, "editable")).await.expect("создан").into_inner();
    let e2 = client
        .add_version(AddVersionRequest {
            list_id: made.id.clone(),
            note: "n".into(),
            steps: vec![],
            author_id: owner.clone(),
            expected_version: Some(999),
            meta: None,
        })
        .await
        .expect_err("правка на чужой версии обязана упасть");
    // Контраст на том же стенде: путь правки нёс причину и раньше. Он здесь не для
    // симметрии, а потому что без него зелёный тест не отличить от «трейлеры не ходят вовсе».
    assert_eq!(
        e2.metadata().get(setfork_core::reason::REASON_KEY).and_then(|v| v.to_str().ok()),
        Some("STALE"),
        "причина не дошла по проводу даже на пути правки — сломан сам механизм, а не одна ветка"
    );
    assert_eq!(
        err.metadata().get(setfork_core::reason::REASON_KEY).and_then(|v| v.to_str().ok()),
        Some("EXISTS"),
        "причина не дошла по проводу на пути создания"
    );

    // Даём слою дописать строку: она пишется после того, как ответ ушёл клиенту.
    for _ in 0..50 {
        if log.text().contains("rpc error") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let text = log.text();

    assert!(text.contains("rpc error"), "строки отказа в журнале нет вовсе:\n{text}");
    assert!(
        text.contains("reason=\"EXISTS\"") || text.contains("reason=EXISTS"),
        "журнал не называет причину — оператор снова видит только код:\n{text}"
    );
}
