//! Предусловие записи: ядро СПРАШИВАЕТ приложение, можно ли писать в список.
//!
//! ADR-0015: точка принуждения — git-слой, точка решения — приложение. Ровно так
//! устроен Gitaly: его `pre-receive` не решает сам и не полагается на переданный
//! вердикт, а постит изменения в `/internal/allowed` у Rails, «so that Rails can
//! determine whether the change is allowed or not» (gitaly/doc/hooks.md).
//!
//! Почему это не «гейт в ядре»: ядро не знает ни пользователей, ни ролей, ни
//! правил — оно не читает `frozen_at`/`archived_at` (по ADR-0014 это канон
//! Postgres и надстройка) и не хранит их семантику. Оно задаёт один вопрос и
//! подчиняется ответу. ADR-0011 §2 (ядро не делает пользовательской авторизации)
//! этим не нарушается.
//!
//! Почему из RPC-слоя, а не из шелл-хука, как у Gitaly: хук срабатывает только на
//! `receive-pack`, а половина нашей записи идёт через git2 (merge, теги,
//! CommitToBranch) и хука не видит вовсе — ровно поэтому в ядре есть единая точка
//! `git::update::update_main`. Вызов из RPC покрывает ВСЕ пути записи.
//!
//! **Fail-closed.** Фронт не ответил, ответил ошибкой или не уложился в таймаут —
//! запись отклоняется. Дверь, открытая по умолчанию, обесценивает всю конструкцию.
use std::sync::OnceLock;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use tonic::Status;

/// Сколько ждём вердикт. Коротко: это внутрисетевой вызов, и залипший фронт не
/// должен превращаться в залипший push — лучше быстрый честный отказ.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Адрес приложения (`SETFORK_APP_URL`, напр. `http://app:3000`). None — не задан.
/// Читается один раз; сервер без него не стартует (см. main::require_app_url).
pub fn app_url() -> Option<&'static str> {
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| {
        std::env::var("SETFORK_APP_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
    })
    .as_deref()
}

type HttpClient = Client<HttpConnector, Full<Bytes>>;

fn client() -> &'static HttpClient {
    static C: OnceLock<HttpClient> = OnceLock::new();
    // Один клиент на процесс: держит пул соединений, иначе каждый пуш платил бы
    // за новый TCP-хендшейк к соседнему контейнеру.
    C.get_or_init(|| Client::builder(TokioExecutor::new()).build(HttpConnector::new()))
}

/// Вердикт приложения. `Allow` — можно писать; всё остальное — причина отказа.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Allow,
    /// Продуктовый запрет с кодом причины ('frozen' | 'archived' | 'not-found').
    Deny(String),
    /// Спросить не удалось. Текст — для лога и для человека; ответ всё равно «нет».
    Unavailable(String),
}

/// Разбор тела ответа. Форма — `{"allow":true}` или `{"allow":false,"reason":"frozen"}`.
/// Неожиданная форма — это НЕ «можно»: непонятый ответ трактуем как недоступность.
fn parse_verdict(body: &[u8]) -> Verdict {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Verdict::Unavailable("вердикт не разобрался как JSON".into());
    };
    match v.get("allow").and_then(|a| a.as_bool()) {
        Some(true) => Verdict::Allow,
        Some(false) => {
            Verdict::Deny(v.get("reason").and_then(|r| r.as_str()).unwrap_or("denied").to_string())
        }
        None => Verdict::Unavailable("в вердикте нет поля allow".into()),
    }
}

async fn ask(base: &str, owner: &str, slug: &str) -> Verdict {
    let payload = serde_json::json!({ "owner": owner, "slug": slug }).to_string();
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("{base}/api/internal/write-allowed"))
        .header("content-type", "application/json");
    // Тот же общий токен канала, что у gRPC: направление вызова обратное, а
    // граница доверия та же.
    if let Ok(token) = std::env::var("SETFORK_CORE_TOKEN")
        && !token.is_empty()
    {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let req = match builder.body(Full::new(Bytes::from(payload))) {
        Ok(r) => r,
        Err(e) => return Verdict::Unavailable(format!("запрос не собрался: {e}")),
    };

    let resp = match tokio::time::timeout(TIMEOUT, client().request(req)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Verdict::Unavailable(format!("приложение недоступно: {e}")),
        Err(_) => return Verdict::Unavailable(format!("приложение не ответило за {TIMEOUT:?}")),
    };
    let status = resp.status();
    if !status.is_success() {
        return Verdict::Unavailable(format!("приложение ответило {status}"));
    }
    match resp.into_body().collect().await {
        Ok(b) => parse_verdict(&b.to_bytes()),
        Err(e) => Verdict::Unavailable(format!("тело вердикта не прочиталось: {e}")),
    }
}

/// Спрашивает приложение и превращает отказ в gRPC-статус.
///
/// Зовётся из КАЖДОГО мутирующего RPC до записи; страж-тест
/// `tests/write_gate_guard.rs` следит, чтобы новый мутирующий метод не появился
/// без этого вызова (та же форма защиты, что у `update_main`).
pub async fn ensure_writable(owner: &str, slug: &str) -> Result<(), Status> {
    match app_url() {
        Some(base) => ensure_writable_at(base, owner, slug).await,
        // Сюда попадаем только в явно небезопасном dev-режиме: сервер без
        // SETFORK_APP_URL не стартует (проверка в main).
        None => Ok(()),
    }
}

/// То же с ЯВНЫМ адресом приложения.
///
/// Публичная не ради вызывающих в проде (им нужен `ensure_writable`), а ради
/// проверяемости: адрес из конфига читается один раз на процесс, и тест,
/// подменяющий его переменной окружения, гонялся бы с соседними тестами за
/// первую инициализацию. Явный параметр убирает и кэш, и гонку.
pub async fn ensure_writable_at(base: &str, owner: &str, slug: &str) -> Result<(), Status> {
    match ask(base, owner, slug).await {
        Verdict::Allow => Ok(()),
        Verdict::Deny(reason) => {
            metrics::counter!("write_gate_denied_total", "reason" => reason.clone()).increment(1);
            Err(match reason.as_str() {
                "not-found" => Status::not_found("list not found"),
                other => Status::failed_precondition(other.to_string()),
            })
        }
        Verdict::Unavailable(why) => {
            // Громко: это отказ в обслуживании записи, а не рядовая ошибка ввода.
            metrics::counter!("write_gate_denied_total", "reason" => "unavailable").increment(1);
            tracing::error!(owner, slug, why, "вердикт записи не получен — отказываем (fail-closed)");
            Err(Status::unavailable("write precondition check unavailable"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn вердикт_разбирается() {
        assert_eq!(parse_verdict(br#"{"allow":true}"#), Verdict::Allow);
        assert_eq!(parse_verdict(br#"{"allow":false,"reason":"frozen"}"#), Verdict::Deny("frozen".into()));
        assert_eq!(
            parse_verdict(br#"{"allow":false}"#),
            Verdict::Deny("denied".into()),
            "отказ без причины остаётся отказом"
        );
    }

    #[test]
    fn непонятый_ответ_это_не_разрешение() {
        // Главное свойство fail-closed: всё, что не «allow: true», не пропускает.
        for body in [&b"{}"[..], b"not json at all", b"", b"{\"allow\":\"yes\"}", b"[]"] {
            assert!(
                !matches!(parse_verdict(body), Verdict::Allow),
                "разобрано как разрешение: {:?}",
                String::from_utf8_lossy(body)
            );
        }
    }
}
