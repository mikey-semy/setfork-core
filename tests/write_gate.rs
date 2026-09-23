//! Ф1 трека git-surface: предусловие записи спрашивается у приложения (ADR-0015).
//!
//! Проверяем ПОВЕДЕНИЕМ против настоящего HTTP-сервера, а не моком: ценность
//! гейта целиком в том, как он ведёт себя на краях — когда приложение отвечает
//! «нельзя», отвечает мусором, отвечает 500 или не отвечает вовсе. Мок, который
//! возвращает заранее заготовленный `Verdict`, ровно эти края и не проверил бы.
//!
//! Главное свойство — fail-closed: всё, что не является явным «allow: true»,
//! обязано остановить запись. Обратная ошибка (пропустить при сбое) тихо
//! отключила бы заморозку, а её уже один раз обходили живьём (линза 02, F3).
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use setfork_core::gate::{ContentRefusal, DenyDetail, ensure_content_allowed_at, ensure_writable_at};
use setfork_core::reason::REASON_KEY;

/// Причина отказа из трейлера — то, по чему клиент различает случаи (И1).
fn reason_of(s: &tonic::Status) -> Option<&str> {
    s.metadata().get(REASON_KEY).and_then(|v| v.to_str().ok())
}

/// Односвязный HTTP-сервер-заглушка: отдаёт заданный ответ и живёт до `stop`.
/// Свой, а не библиотечный: нужен ровно один эндпоинт и полный контроль над
/// формой ответа, включая заведомо битую.
struct Stub {
    addr: String,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Тела ВСЕХ пришедших запросов. Нужны там, где проверяется не ответ гейта,
    /// а сам ВОПРОС: форму тела видит только приложение, и сверять её иначе как
    /// по проводу значило бы сверять код с самим собой.
    seen: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    /// `reply` — готовый HTTP-ответ целиком (статус + заголовки + тело).
    fn start(reply: &'static str) -> Stub {
        Stub::start_inner(reply, false)
    }

    /// Отдаёт ЗАГОЛОВКИ, обещает тело и залипает, не дослав его. Самый коварный
    /// вид недоступности: соединение живо, ответ «начался», а конца нет.
    fn start_stalling(head: &'static str) -> Stub {
        Stub::start_inner(head, true)
    }

    /// Тела пришедших запросов, по порядку.
    fn asked(&self) -> Vec<String> {
        self.seen.lock().expect("seen").clone()
    }

    fn start_inner(reply: &'static str, stall: bool) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = format!("http://{}", listener.local_addr().expect("addr"));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_t = seen.clone();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_t.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(mut s) = stream else { continue };
                serve_one(&mut s, reply, &seen_t);
                if stall {
                    // Держим соединение открытым дольше таймаута гейта: закрыть
                    // его значило бы проверить обрыв, а не залипание.
                    std::thread::sleep(std::time::Duration::from_secs(20));
                }
            }
        });
        Stub { addr, stop, handle: Some(handle), seen }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Пинок, чтобы accept() разблокировался и поток вышел.
        let _ = TcpStream::connect(self.addr.trim_start_matches("http://"));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Отвечает и ВОЗВРАЩАЕТ тело запроса — по нему сверяется форма вопроса.
/// Дочитать запрос, ЗАПИСАТЬ его тело и только потом ответить.
///
/// ⚠️ Порядок — не вкус. Прежде тело записывалось ПОСЛЕ ответа: клиент получал ответ,
/// тест сразу читал `asked()` — и если поток заглушки не успел, видел пустой список.
/// Под нагрузкой раннера так упал master (`left: []`) на проверке, к которой правка
/// перед этим не прикасалась. Записанный до ответа вопрос виден тесту всегда.
fn serve_one(s: &mut TcpStream, reply: &str, seen: &Mutex<Vec<String>>) {
    // Дочитываем запрос до конца заголовков и тела (Content-Length), иначе
    // клиент увидит обрыв вместо ответа.
    let mut reader = BufReader::new(s.try_clone().expect("clone"));
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            // Соединение оборвалось до конца заголовков: вопроса не было.
            seen.lock().expect("seen").push(String::new());
            return;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        use std::io::Read;
        let _ = reader.read_exact(&mut body);
    }
    seen.lock().expect("seen").push(String::from_utf8_lossy(&body).to_string());
    let _ = s.write_all(reply.as_bytes());
    let _ = s.flush();
}

fn http(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

#[tokio::test]
async fn the_gate_lets_through_only_an_explicit_allow() {
    let stub = Stub::start(Box::leak(http(r#"{"allow":true}"#).into_boxed_str()));
    assert!(ensure_writable_at(&stub.addr, "mike", "list").await.is_ok(), "явное allow пропускает");
}

/// Продуктовый отказ доезжает кодом причины, а не общей ошибкой: фронту надо
/// показать человеку разное для «заморожен» и «в архиве».
#[tokio::test]
async fn the_refusal_reason_arrives() {
    let stub = Stub::start(Box::leak(http(r#"{"allow":false,"reason":"frozen"}"#).into_boxed_str()));
    let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("должен быть отказ");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    // Причина — в трейлере (И1); текст сообщения клиент больше не разбирает.
    assert_eq!(reason_of(&err), Some("FROZEN"));
}

#[tokio::test]
async fn archived_is_distinguishable_from_frozen() {
    let stub = Stub::start(Box::leak(http(r#"{"allow":false,"reason":"archived"}"#).into_boxed_str()));
    let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("отказ");
    assert_eq!(reason_of(&err), Some("ARCHIVED"));
}

#[tokio::test]
async fn a_missing_list_is_not_found() {
    let stub = Stub::start(Box::leak(http(r#"{"allow":false,"reason":"not-found"}"#).into_boxed_str()));
    let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("отказ");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn an_app_error_stops_the_write() {
    let stub = Stub::start("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
    let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("500 не пропускает");
    // Срыв СВЯЗИ — преходящая беда, и клиенту честно сказать «повтори»: код
    // Unavailable существует ровно для этого. Непонятый ОТВЕТ приложения едет
    // другим кодом (см. пробу мусора ниже) — повтор его не лечит. Причину
    // проверяем тоже: код различает случаи, а контракт держится на ней.
    assert_eq!(err.code(), tonic::Code::Unavailable);
    assert_eq!(reason_of(&err), Some("GATE_UNAVAILABLE"));
}

/// 4xx — НЕ то же, что 5xx, и это главная развилка ответа.
///
/// Приложение ответило и отказалось отвечать по существу: разошлись токены канала,
/// сменился путь, включился чужой обработчик. Повтор такого не лечит, а сказать
/// клиенту «повтори» значит обречь пуш долбиться в неверную настройку бесконечно —
/// на git-пути это ещё и 503 с Retry-After наружу (находка своего прохода ревью
/// по #101).
#[tokio::test]
async fn a_refusal_to_ask_is_not_passed_off_as_a_transport_failure() {
    for resp in [
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
    ] {
        let stub = Stub::start(resp);
        let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("4xx не пропускает");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{resp}");
        // Приложение ОТВЕТИЛО — просто не то. Это расхождение контракта, а не сеть,
        // и причина обязана быть своя: фронт по ней говорит «повторять бесполезно».
        assert_eq!(reason_of(&err), Some("GATE_MALFORMED"), "{resp}");
    }
}

#[tokio::test]
async fn garbage_in_the_reply_stops_the_write() {
    let stub = Stub::start(Box::leak(http("<html>что-то пошло не так</html>").into_boxed_str()));
    let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("мусор не пропускает");
    // Приложение ОТВЕТИЛО, но не то: повтор этого не лечит, и код обязан отличаться
    // от срыва связи — иначе клиент, повторяющий Unavailable, будет долбиться в
    // расхождение контракта до посинения.
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert_eq!(reason_of(&err), Some("GATE_MALFORMED"));
}

/// Самый важный край: приложения нет вообще. Отказ, а не «пропустим на всякий».
#[tokio::test]
async fn an_unreachable_app_stops_the_write() {
    // Порт, который никто не слушает: занимаем и сразу отпускаем.
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        format!("http://{}", l.local_addr().expect("addr"))
    };
    let err = ensure_writable_at(&addr, "mike", "list").await.expect_err("недоступность не пропускает");
    assert_eq!(err.code(), tonic::Code::Unavailable);
    assert_eq!(reason_of(&err), Some("GATE_UNAVAILABLE"));
}

/// Регрессия P1 авто-ревью core#71: заголовки пришли, тело залипло.
///
/// Таймаут стоял только на `client().request()`, а тот резолвится по приходу
/// «головы» ответа — тело читается лениво. Приложение, отдавшее заголовки и
/// замолчавшее, держало бы push бесконечно, вместе с репо-локом. Тест ждёт
/// реальные 5 секунд конфигурации: проверяем настоящее поведение, а не
/// подкрученное под тест.
#[tokio::test]
async fn a_stuck_reply_body_does_not_hold_the_write_forever() {
    let stub = Stub::start_stalling(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 64\r\n\r\n",
    );
    let started = std::time::Instant::now();
    let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("залипание не пропускает");
    assert_eq!(err.code(), tonic::Code::Unavailable);
    assert_eq!(reason_of(&err), Some("GATE_UNAVAILABLE"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "отказ пришёл по таймауту, а не по обрыву"
    );
}

// ── H15-002: второй вопрос — БЕЗОПАСНО ЛИ СОДЕРЖИМОЕ ─────────────────────────
//
// Края здесь те же самые, и проверяются они по той же причине: отказ у `git push`
// читает человек, и «в шаге 3 запрещённая команда» с «проверка не ответила» —
// это два противоположных совета. Слепить их в один «нельзя» значило бы послать
// человека переписывать шаг, в котором всё в порядке.

/// Место находки доезжает целиком — по нему человек и чинит свой список.
#[tokio::test]
async fn a_destructive_command_names_the_step_and_the_command() {
    let stub = Stub::start(Box::leak(
        http(r#"{"allow":false,"reason":"destructive","step":3,"rule":"rm_rf","fragment":"rm -rf /"}"#)
            .into_boxed_str(),
    ));
    let err = ensure_content_allowed_at(&stub.addr, "mike", "list", &[Some("rm -rf /".into())])
        .await
        .expect_err("разрушительная команда не проходит");
    assert_eq!(
        err,
        ContentRefusal::Destructive(DenyDetail {
            step: 3,
            rule: "rm_rf".into(),
            fragment: "rm -rf /".into()
        })
    );
}

/// ОКНО ВЫКАТКИ. Приложение старше этой правки поля `blocks` не знает и отвечает
/// прежним `{"allow":true}` — пуш обязан пройти ровно как вчера. Ветки «а вдруг
/// старое» для этого нет намеренно: такая ветка однажды открыла бы дверь и новому.
#[tokio::test]
async fn an_app_that_ignores_blocks_lets_the_push_through() {
    let stub = Stub::start(Box::leak(http(r#"{"allow":true}"#).into_boxed_str()));
    assert!(
        ensure_content_allowed_at(&stub.addr, "mike", "list", &[Some("rm -rf /".into())]).await.is_ok(),
        "старое приложение = сегодняшнее поведение, а не отказ"
    );
}

/// Список успели заморозить, пока ехал пак: причина называется своя, а не
/// «запрещённая команда» — иначе человек пойдёт править шаг, который ни при чём.
#[tokio::test]
async fn a_product_refusal_without_a_place_keeps_its_own_reason() {
    let stub = Stub::start(Box::leak(http(r#"{"allow":false,"reason":"frozen"}"#).into_boxed_str()));
    let err = ensure_content_allowed_at(&stub.addr, "mike", "list", &[Some("make".into())])
        .await
        .expect_err("отказ");
    assert_eq!(err, ContentRefusal::Denied("frozen".into()));
}

/// Те же две беды и та же развилка, что у предусловия записи: 5xx преходящ и
/// лечится повтором, 4xx — расхождение контракта, повтор бесполезен. Советы
/// человеку разные, значит и случаи обязаны быть разными.
#[tokio::test]
async fn transport_and_contract_failures_stay_apart() {
    let five = Stub::start("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n");
    assert!(matches!(
        ensure_content_allowed_at(&five.addr, "mike", "list", &[Some("make".into())]).await,
        Err(ContentRefusal::Unavailable(_))
    ));

    let four = Stub::start("HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
    assert!(matches!(
        ensure_content_allowed_at(&four.addr, "mike", "list", &[Some("make".into())]).await,
        Err(ContentRefusal::NotUnderstood(_))
    ));

    let garbage = Stub::start(Box::leak(http("<html>не json</html>").into_boxed_str()));
    assert!(matches!(
        ensure_content_allowed_at(&garbage.addr, "mike", "list", &[Some("make".into())]).await,
        Err(ContentRefusal::NotUnderstood(_))
    ));
}

/// Приложения нет вовсе — fail-closed, как и у соседа. Дверь, открытая при сбое,
/// обесценила бы всю проверку: уронить фронт проще, чем обойти правило.
#[tokio::test]
async fn an_unreachable_app_stops_the_content_too() {
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        format!("http://{}", l.local_addr().expect("addr"))
    };
    assert!(matches!(
        ensure_content_allowed_at(&addr, "mike", "list", &[Some("rm -rf /".into())]).await,
        Err(ContentRefusal::Unavailable(_))
    ));
}

/// ОКНО ВЫКАТКИ, проверенное ПО ПРОВОДУ. Форму вопроса видит только приложение,
/// и `ask_payload` сам по себе её не гарантирует: достаточно, чтобы вызывающий
/// передал не тот аргумент. Обычное предусловие записи обязано спрашивать ровно
/// тем же телом, что и до этой правки, — приложение отличает «ядро не
/// спрашивало про содержимое» от «спросило про пустой список», и лишнее поле
/// заставило бы его считать команды там, где их взять неоткуда.
#[tokio::test]
async fn the_write_precondition_asks_exactly_as_it_did_before() {
    let stub = Stub::start(Box::leak(http(r#"{"allow":true}"#).into_boxed_str()));
    ensure_writable_at(&stub.addr, "mike", "list").await.expect("allow");
    assert_eq!(stub.asked(), vec![r#"{"owner":"mike","slug":"list"}"#.to_string()]);

    let content = Stub::start(Box::leak(http(r#"{"allow":true}"#).into_boxed_str()));
    ensure_content_allowed_at(&content.addr, "mike", "list", &[Some("make".into()), None])
        .await
        .expect("allow");
    assert_eq!(
        content.asked(),
        vec![r#"{"owner":"mike","slug":"list","blocks":[{"command":"make"},{"command":null}]}"#.to_string()],
        "вопрос о содержимом обязан нести блоки, и каждый на своём месте"
    );
}
