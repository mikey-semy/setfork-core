//! ПРОБА линзы проверки ядра: rate-limit СКВОЗЬ живой сервер, а не через allow().
//!
//! Юниты в src/ratelimit.rs проверяют чистую функцию allow(). Не проверено ничем:
//! навешан ли слой на реальный сервер, доходит ли RESOURCE_EXHAUSTED до клиента,
//! правда ли неавторизованные не расходуют окно (иначе лимит сам становится
//! вектором DoS), и не лимитируется ли health.
//!
//! Требует поднятого ядра: SETFORK_RPC_RPM=3 SETFORK_RPC_RPM_HEAVY=2, токен probe-token.
//! `cargo test --test ratelimit_probe -- --ignored --nocapture --test-threads=1`

use setfork_core::pb::git_core_client::GitCoreClient;
use setfork_core::pb::RepoRef;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request};

const ADDR: &str = "http://127.0.0.1:50051";

async fn chan() -> Channel {
    Channel::from_static(ADDR).connect().await.expect("ядро должно быть поднято на 50051")
}

fn req(token: Option<&str>) -> Request<RepoRef> {
    let mut r = Request::new(RepoRef { owner: "nobody".into(), slug: "nothing".into() });
    if let Some(t) = token {
        r.metadata_mut()
            .insert("authorization", MetadataValue::try_from(format!("Bearer {t}")).expect("meta"));
    }
    r
}

/// Лимит обычного метода (rpm=3) доходит до клиента как RESOURCE_EXHAUSTED.
#[tokio::test]
#[ignore = "нужно поднятое ядро с SETFORK_RPC_RPM=3"]
async fn лимит_обычного_метода_доходит_до_клиента() {
    let mut c = GitCoreClient::new(chan().await);
    let mut codes = Vec::new();
    for _ in 0..6 {
        let code = match c.list_branches(req(Some("probe-token"))).await {
            Ok(_) => Code::Ok,
            Err(e) => e.code(),
        };
        codes.push(code);
    }
    println!("КОДЫ (rpm=3): {codes:?}");
    // Первые три — «нормальный» ответ сервиса (список не существует → NotFound),
    // это НЕ отказ лимита. Дальше обязан прийти RESOURCE_EXHAUSTED.
    assert!(
        codes[..3].iter().all(|c| *c != Code::ResourceExhausted),
        "в пределах бюджета отказов лимита быть не должно: {codes:?}"
    );
    assert_eq!(codes[3], Code::ResourceExhausted, "четвёртый запрос сверх rpm=3: {codes:?}");
    assert_eq!(codes[5], Code::ResourceExhausted, "окно держится: {codes:?}");
}

/// Неавторизованные не должны расходовать окно: иначе любой, кто достучался
/// до порта, гасит бюджет метода для настоящего фронта (DoS-амплификация).
#[tokio::test]
#[ignore = "нужно поднятое ядро"]
async fn чужой_токен_не_съедает_бюджет() {
    let mut c = GitCoreClient::new(chan().await);
    let mut unauth = Vec::new();
    for _ in 0..20 {
        let code = match c.list_tags(req(Some("wrong-token"))).await {
            Ok(_) => Code::Ok,
            Err(e) => e.code(),
        };
        unauth.push(code);
    }
    println!("ЧУЖОЙ ТОКЕН x20: {:?}", &unauth[..3]);
    assert!(
        unauth.iter().all(|c| *c == Code::Unauthenticated),
        "чужой токен обязан давать unauthenticated, а не отказ лимита: {unauth:?}"
    );

    // После 20 неавторизованных настоящий вызывающий должен пройти.
    let real = match c.list_tags(req(Some("probe-token"))).await {
        Ok(_) => Code::Ok,
        Err(e) => e.code(),
    };
    println!("НАСТОЯЩИЙ ПОСЛЕ НИХ: {real:?}");
    assert_ne!(real, Code::ResourceExhausted, "неавторизованные съели окно настоящего клиента");
}

/// Health — проба оркестратора: под лимит попадать не должна, иначе контейнер
/// объявят мёртвым ровно в момент нагрузки.
#[tokio::test]
#[ignore = "нужно поднятое ядро"]
async fn health_не_лимитируется() {
    use tonic_health::pb::health_client::HealthClient;
    use tonic_health::pb::HealthCheckRequest;
    let mut h = HealthClient::new(chan().await);
    let mut codes = Vec::new();
    for _ in 0..30 {
        let code = match h.check(HealthCheckRequest { service: String::new() }).await {
            Ok(_) => Code::Ok,
            Err(e) => e.code(),
        };
        codes.push(code);
    }
    println!("HEALTH x30 уникальных кодов: {:?}", {
        let mut u: Vec<_> = codes.clone();
        u.dedup();
        u
    });
    assert!(
        codes.iter().all(|c| *c != Code::ResourceExhausted),
        "health под лимитом — оркестратор убьёт живой контейнер: {codes:?}"
    );
}
