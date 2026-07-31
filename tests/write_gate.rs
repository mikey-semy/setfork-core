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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use setfork_core::gate::ensure_writable_at;
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

    fn start_inner(reply: &'static str, stall: bool) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = format!("http://{}", listener.local_addr().expect("addr"));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_t.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(mut s) = stream else { continue };
                serve_one(&mut s, reply);
                if stall {
                    // Держим соединение открытым дольше таймаута гейта: закрыть
                    // его значило бы проверить обрыв, а не залипание.
                    std::thread::sleep(std::time::Duration::from_secs(20));
                }
            }
        });
        Stub { addr, stop, handle: Some(handle) }
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

fn serve_one(s: &mut TcpStream, reply: &str) {
    // Дочитываем запрос до конца заголовков и тела (Content-Length), иначе
    // клиент увидит обрыв вместо ответа.
    let mut reader = BufReader::new(s.try_clone().expect("clone"));
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    if len > 0 {
        let mut body = vec![0u8; len];
        use std::io::Read;
        let _ = reader.read_exact(&mut body);
    }
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
async fn гейт_пропускает_только_явное_разрешение() {
    let stub = Stub::start(Box::leak(http(r#"{"allow":true}"#).into_boxed_str()));
    assert!(ensure_writable_at(&stub.addr, "mike", "list").await.is_ok(), "явное allow пропускает");
}

/// Продуктовый отказ доезжает кодом причины, а не общей ошибкой: фронту надо
/// показать человеку разное для «заморожен» и «в архиве».
#[tokio::test]
async fn причина_отказа_доезжает() {
    let stub = Stub::start(Box::leak(http(r#"{"allow":false,"reason":"frozen"}"#).into_boxed_str()));
    let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("должен быть отказ");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    // Причина — в трейлере (И1); текст сообщения клиент больше не разбирает.
    assert_eq!(reason_of(&err), Some("FROZEN"));
}

#[tokio::test]
async fn архив_отличим_от_заморозки() {
    let stub = Stub::start(Box::leak(http(r#"{"allow":false,"reason":"archived"}"#).into_boxed_str()));
    let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("отказ");
    assert_eq!(reason_of(&err), Some("ARCHIVED"));
}

#[tokio::test]
async fn несуществующий_список_это_not_found() {
    let stub = Stub::start(Box::leak(http(r#"{"allow":false,"reason":"not-found"}"#).into_boxed_str()));
    let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("отказ");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn ошибка_приложения_останавливает_запись() {
    let stub = Stub::start("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
    let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("500 не пропускает");
    assert_eq!(err.code(), tonic::Code::Unavailable);
}

#[tokio::test]
async fn мусор_в_ответе_останавливает_запись() {
    let stub = Stub::start(Box::leak(http("<html>что-то пошло не так</html>").into_boxed_str()));
    let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("мусор не пропускает");
    assert_eq!(err.code(), tonic::Code::Unavailable);
}

/// Самый важный край: приложения нет вообще. Отказ, а не «пропустим на всякий».
#[tokio::test]
async fn недоступное_приложение_останавливает_запись() {
    // Порт, который никто не слушает: занимаем и сразу отпускаем.
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        format!("http://{}", l.local_addr().expect("addr"))
    };
    let err = ensure_writable_at(&addr, "mike", "list").await.expect_err("недоступность не пропускает");
    assert_eq!(err.code(), tonic::Code::Unavailable);
}

/// Регрессия P1 авто-ревью core#71: заголовки пришли, тело залипло.
///
/// Таймаут стоял только на `client().request()`, а тот резолвится по приходу
/// «головы» ответа — тело читается лениво. Приложение, отдавшее заголовки и
/// замолчавшее, держало бы push бесконечно, вместе с репо-локом. Тест ждёт
/// реальные 5 секунд конфигурации: проверяем настоящее поведение, а не
/// подкрученное под тест.
#[tokio::test]
async fn залипшее_тело_ответа_не_держит_запись_вечно() {
    let stub = Stub::start_stalling(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 64\r\n\r\n",
    );
    let started = std::time::Instant::now();
    let err = ensure_writable_at(&stub.addr, "mike", "list").await.expect_err("залипание не пропускает");
    assert_eq!(err.code(), tonic::Code::Unavailable);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "отказ пришёл по таймауту, а не по обрыву"
    );
}
