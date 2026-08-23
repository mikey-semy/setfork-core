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
use tonic::{Code, Status};

use crate::reason::{self, Reason};

/// Сколько ждём вердикт. Коротко: это внутрисетевой вызов, и залипший фронт не
/// должен превращаться в залипший push — лучше быстрый честный отказ.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Почему адрес приложения непригоден. Текст идёт человеку в сообщение остановки.
#[derive(Debug, PartialEq, Eq)]
pub enum BadAppUrl {
    /// Переменная не задана или пуста.
    Missing,
    /// Есть значение, но это не абсолютный `http://`-адрес.
    NotAbsolute(String),
    /// Указан `https://`, а клиент здесь принципиально plaintext (см. ниже).
    TlsUnsupported(String),
}

/// Проверенный адрес приложения из строки.
///
/// Проверяем ФОРМУ, а не только непустоту. Прод 01.08 показал, зачем: там стояло
/// `SETFORK_APP_URL=setfork-frontend-zpyzi8` — имя без схемы и порта, да ещё и не
/// то. Ядро стартовало молча, а каждая запись отклонялась бы по fail-closed, и
/// причина нашлась бы не сразу: в логах пусто, пока никто не пишет.
///
/// Это ровно то, от чего fail-fast и заводился (аудит 2026-07-20, P1-7): ошибку
/// конфигурации ловим на старте, а не первым отказом в бою.
pub fn parse_app_url(raw: Option<&str>) -> Result<String, BadAppUrl> {
    let s = raw.map(str::trim).unwrap_or("");
    if s.is_empty() {
        return Err(BadAppUrl::Missing);
    }
    let url = s.trim_end_matches('/');
    // https отвергаем ЯВНО и с объяснением, а не молча принимаем: клиент собран
    // с plaintext-коннектором (`HttpConnector`), TLS он не умеет по построению —
    // вызов идёт внутри docker-сети, и тянуть ради него rustls незачем. Принять
    // https на старте значило бы завести ровно ту ловушку, ради которой эта
    // проверка и появилась: конфиг «валиден», а каждая запись падает в бою
    // (авто-ревью core#73, P1).
    if url.starts_with("https://") {
        return Err(BadAppUrl::TlsUnsupported(url.to_string()));
    }
    // Схема обязательна: без неё hyper соберёт запрос с пустым authority и
    // получит ошибку соединения на каждом вызове.
    let rest = url.strip_prefix("http://").ok_or_else(|| BadAppUrl::NotAbsolute(url.to_string()))?;
    // Хост непустой и не начинается со слеша (иначе это путь, а не authority).
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() {
        return Err(BadAppUrl::NotAbsolute(url.to_string()));
    }
    // Значение обязано разбираться как URI — последняя проверка тем же кодом,
    // который потом соберёт запрос.
    if url.parse::<hyper::Uri>().is_err() {
        return Err(BadAppUrl::NotAbsolute(url.to_string()));
    }
    Ok(url.to_string())
}

/// Адрес приложения (`SETFORK_APP_URL`, напр. `http://app:3000`). None — не задан
/// ЛИБО задан непригодно; сервер в обоих случаях не стартует (проверка в main).
pub fn app_url() -> Option<&'static str> {
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| parse_app_url(std::env::var("SETFORK_APP_URL").ok().as_deref()).ok()).as_deref()
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
    /// Спросить не удалось ПО СВЯЗИ: приложение не отвечает, оборвалось тело,
    /// пришёл 5xx, вышел таймаут. Ответ всё равно «нет», но беда ПРЕХОДЯЩАЯ —
    /// клиенту честно сказать «повтори», и код для этого один: `Unavailable`.
    Unreachable(String),
    /// Приложение ОТВЕТИЛО, но ответ непонятен: не JSON, нет поля `allow`, тип не
    /// тот, либо код ответа 4xx («не пущу спрашивать»). Повтор такого не лечит —
    /// это расхождение контракта или настройки, а не срыв связи.
    Malformed(String),
    /// Спросить не смогли ПО СВОЕЙ вине: запрос не собрался (например, токен канала
    /// с переводом строки — такой заголовок невалиден). Ни приложение, ни сеть тут
    /// ни при чём, и говорить «ответ непонятен» было бы неправдой: ответа не было.
    Local(String),
}

/// Разбор тела ответа. Форма — `{"allow":true}` или `{"allow":false,"reason":"frozen"}`.
/// Неожиданная форма — это НЕ «можно»: непонятый ответ значит «спросить не удалось».
fn parse_verdict(body: &[u8]) -> Verdict {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Verdict::Malformed("вердикт не разобрался как JSON".into());
    };
    match v.get("allow").and_then(|a| a.as_bool()) {
        Some(true) => Verdict::Allow,
        Some(false) => {
            Verdict::Deny(v.get("reason").and_then(|r| r.as_str()).unwrap_or("denied").to_string())
        }
        None => Verdict::Malformed("в вердикте нет поля allow".into()),
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
        Err(e) => return Verdict::Local(format!("запрос не собрался: {e}")),
    };

    // Таймаут накрывает ВЕСЬ обмен, а не только получение заголовков: future от
    // `client().request()` резолвится, как только пришла «голова» ответа, тело
    // читается лениво. Если таймаут стоит только на нём, приложение, отдавшее
    // заголовки и залипшее на теле, держит push бесконечно — и держит репо-лок
    // вместе с ним (авто-ревью core#71, P1).
    let exchange = async {
        let resp = match client().request(req).await {
            Ok(r) => r,
            Err(e) => return Verdict::Unreachable(format!("приложение недоступно: {e}")),
        };
        let status = resp.status();
        if !status.is_success() {
            // 5xx — приложению плохо, это преходяще и повтор уместен. 4xx —
            // приложение ОТВЕТИЛО и отказалось отвечать по существу: разошлись
            // токены канала, сменился путь, включился чужой обработчик. Повтор
            // такого не лечит, и говорить клиенту «повтори» значит обречь его
            // долбиться в неверную настройку бесконечно (находка своего прохода
            // ревью по #101).
            return if status.is_server_error() {
                Verdict::Unreachable(format!("приложение ответило {status}"))
            } else {
                Verdict::Malformed(format!("приложение ответило {status} — спросить не дало"))
            };
        }
        match resp.into_body().collect().await {
            Ok(b) => parse_verdict(&b.to_bytes()),
            Err(e) => Verdict::Unreachable(format!("тело вердикта не прочиталось: {e}")),
        }
    };
    match tokio::time::timeout(TIMEOUT, exchange).await {
        Ok(v) => v,
        Err(_) => Verdict::Unreachable(format!("приложение не ответило за {TIMEOUT:?}")),
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
            // Вердикт приложения — уже машиночитаемый код; переводим его в
            // причину провода, чтобы фронт различал заморозку и архив, а не
            // получал общий failed_precondition (И1).
            Err(match reason.as_str() {
                "not-found" => reason::status(Code::NotFound, Reason::NotFound, "list not found"),
                "frozen" => reason::status(Code::FailedPrecondition, Reason::Frozen, "list is frozen"),
                "archived" => reason::status(Code::FailedPrecondition, Reason::Archived, "list is archived"),
                other => Status::failed_precondition(other.to_string()),
            })
        }
        // ДВЕ разные беды — два разных кода, и это ровно то, чего требуют оба
        // источника сразу. Срыв СВЯЗИ преходящ: клиенту честно сказать «повтори»,
        // и код для этого `Unavailable` (Gitaly запрещает отдавать его как общий
        // «что-то не вышло», но именно для ретраибельного случая он и существует).
        // Непонятый ОТВЕТ повтором не лечится: приложение ответило, но не то —
        // это расхождение контракта, то есть `FailedPrecondition` по AIP-193.
        // Раньше оба случая ехали одним кодом, и различить их клиент не мог.
        Verdict::Unreachable(why) => {
            metrics::counter!("write_gate_denied_total", "reason" => "unreachable").increment(1);
            tracing::error!(
                owner,
                slug,
                why,
                "write verdict not received (transport), refusing (fail-closed)"
            );
            Err(reason::status(
                Code::Unavailable,
                Reason::GateUnavailable,
                "write precondition check unavailable",
            ))
        }
        Verdict::Malformed(why) => {
            metrics::counter!("write_gate_denied_total", "reason" => "malformed").increment(1);
            tracing::error!(owner, slug, why, "write verdict not understood, refusing (fail-closed)");
            Err(reason::status(
                Code::FailedPrecondition,
                Reason::GateUnavailable,
                "write precondition verdict not understood",
            ))
        }
        Verdict::Local(why) => {
            // Наша собственная поломка: сказать «приложение недоступно» или «ответ
            // непонятен» было бы враньём в обе стороны, а искать будут по этой
            // строке. Internal — как раз «мы сломались», и повтор не поможет.
            metrics::counter!("write_gate_denied_total", "reason" => "local").increment(1);
            tracing::error!(
                owner,
                slug,
                why,
                "write verdict request could not be built, refusing (fail-closed)"
            );
            Err(reason::status(
                Code::Internal,
                Reason::GateUnavailable,
                "write precondition request could not be built",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Непонятый ответ — это НЕ «можно». Ветки собраны по промпту линзы 05 §3:
    /// каждая из них однажды может прийти от приложения, и ни одна не имеет права
    /// открыть запись.
    #[test]
    fn непонятый_вердикт_не_открывает_запись() {
        for body in [
            &b""[..],                   // пустое тело
            "не json вовсе".as_bytes(), // не разобралось
            b"{}",                      // нет поля allow
            br#"{"allow":"yes"}"#,      // строка вместо булева — «похоже на да»
            br#"{"allow":1}"#,          // число вместо булева
            br#"{"allowed":true}"#,     // поле названо иначе
            br#"[{"allow":true}]"#,     // массив вместо объекта
        ] {
            assert!(
                matches!(parse_verdict(body), Verdict::Malformed(_)),
                "тело {:?} обязано читаться как «ответ непонятен»",
                String::from_utf8_lossy(body)
            );
        }
    }

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

    /// Регрессия инцидента .com 01.08: в проде стояло имя контейнера без схемы и
    /// порта. Ядро стартовало молча, а каждая запись отклонялась бы по fail-closed
    /// — в логах при этом пусто, пока никто не пишет, и причина ищется долго.
    #[test]
    fn мусорный_адрес_приложения_не_проходит_старт() {
        assert_eq!(parse_app_url(None), Err(BadAppUrl::Missing));
        assert_eq!(parse_app_url(Some("   ")), Err(BadAppUrl::Missing));
        for bad in [
            "setfork-frontend-zpyzi8", // ровно то, что стояло на проде
            "setfork-frontend:3000",   // хост с портом, но без схемы
            "//setfork-frontend:3000", // без схемы
            "ftp://app:3000",          // не http(s)
            "http://",                 // пустой хост
            "http:///api",             // хост подменён путём
        ] {
            assert!(matches!(parse_app_url(Some(bad)), Err(BadAppUrl::NotAbsolute(_))), "{bad:?} прошёл");
        }
    }

    #[test]
    fn годный_адрес_нормализуется() {
        assert_eq!(parse_app_url(Some("http://app:3000")).as_deref(), Ok("http://app:3000"));
        // Хвостовой слеш срезаем: путь эндпоинта дописывается к базе, иначе вышло бы `//api`.
        assert_eq!(parse_app_url(Some("http://app:3000/")).as_deref(), Ok("http://app:3000"));
    }

    /// Регрессия P1 авто-ревью core#73: https проходил валидацию, но клиент
    /// plaintext — каждая запись падала бы в бою. Отвергаем на старте, с
    /// объяснением, а не молча.
    #[test]
    fn https_отвергается_пока_клиент_plaintext() {
        assert_eq!(
            parse_app_url(Some("https://app:3000")),
            Err(BadAppUrl::TlsUnsupported("https://app:3000".into()))
        );
    }
}
