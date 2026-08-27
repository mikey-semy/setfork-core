//! ПРОБА линзы проверки ядра: rate-limit СКВОЗЬ живой сервер, а не через allow().
//!
//! Юниты в src/ratelimit.rs проверяют чистую функцию allow(). Не проверено ничем:
//! навешан ли слой на реальный сервер, доходит ли RESOURCE_EXHAUSTED до клиента,
//! правда ли неавторизованные не расходуют окно (иначе лимит сам становится
//! вектором DoS), и не лимитируется ли health.
//!
//! Требует поднятого ядра: SETFORK_RPC_RPM=3 SETFORK_RPC_RPM_HEAVY=2, токен probe-token.
//! `cargo test --test ratelimit_probe -- --ignored --nocapture --test-threads=1`

#![cfg(feature = "probes")]

use setfork_core::pb::RepoRef;
use setfork_core::pb::git_core_client::GitCoreClient;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request};

const ADDR: &str = "http://127.0.0.1:50051";

/// `None` — ядра на 50051 нет. Проба РУЧНАЯ (нужен сервер с особыми лимитами), и
/// падать из-за его отсутствия она не должна: тогда любой общий прогон с фичей
/// `probes` краснеет не по делу, а на такую красноту перестают смотреть.
///
/// ⚠️ Цена этого решения: без сервера тест печатает пропуск в stderr и завершается
/// УСПЕХОМ. `cargo test` прячет stderr, поэтому без `--nocapture` он выглядит зелёным,
/// не проверив ничего. Так и случилось 27.08: общий прогон проб дал «26 passed», и из
/// этого был сделан неверный вывод, что сервер пробам больше не нужен.
///
/// Менять поведение на падение НЕ надо — довод выше остаётся верным. Надо помнить, что
/// зелёная эта проба означает ровно одно: «либо проверила, либо не смогла».
async fn chan() -> Option<Channel> {
    match Channel::from_static(ADDR).connect().await {
        Ok(c) => Some(c),
        Err(_) => {
            eprintln!(
                "ПРОПУСК: ядра на {ADDR} нет. Проба ручная — подними ядро с \
                 SETFORK_RPC_RPM=3 SETFORK_RPC_RPM_HEAVY=2 и токеном probe-token."
            );
            None
        }
    }
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
async fn the_plain_method_limit_reaches_the_client() {
    let Some(channel) = chan().await else { return };
    let mut c = GitCoreClient::new(channel);
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
async fn a_foreign_token_does_not_consume_the_budget() {
    let Some(channel) = chan().await else { return };
    let mut c = GitCoreClient::new(channel);
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
async fn health_is_not_rate_limited() {
    use tonic_health::pb::HealthCheckRequest;
    use tonic_health::pb::health_client::HealthClient;
    let Some(channel) = chan().await else { return };
    let mut h = HealthClient::new(channel);
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
