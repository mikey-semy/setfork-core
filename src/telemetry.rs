//! Наблюдаемость RPC: tower-layer поверх tonic — метрики Prometheus и
//! структурные логи per-RPC (метод, gRPC-код, латентность).
//!
//! Откуда код ответа: ошибки tonic (Err(Status)) уходят trailers-only ответом,
//! где grpc-status лежит в HTTP-ЗАГОЛОВКАХ; успешный unary кладёт статус в
//! настоящие trailers ПОСЛЕ тела — заголовка нет. Поэтому «нет заголовка = OK»
//! корректно для всех unary-методов этого сервиса (стримов у нас нет).
//! Отказы rate-limit (trailers-only, код 8) и auth (16) тоже попадают сюда —
//! layer навешивается снаружи rate-limit (см. main.rs).

use std::task::{Context, Poll};
use std::time::Instant;

use http::{Request, Response};
use tower::{Layer, Service};

#[derive(Clone, Default)]
pub struct TelemetryLayer;

impl<S> Layer<S> for TelemetryLayer {
    type Service = Telemetry<S>;
    fn layer(&self, service: S) -> Self::Service {
        Telemetry { inner: service }
    }
}

#[derive(Clone)]
pub struct Telemetry<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for Telemetry<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Send,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = futures_util::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let path = req.uri().path().to_string();
        let fut = self.inner.call(req);
        Box::pin(async move {
            let start = Instant::now();
            let res = fut.await;
            // health/reflection-пробы не метём (шум каждые 30с от оркестратора).
            if path.starts_with("/grpc.health") || path.starts_with("/grpc.reflection") {
                return res;
            }
            // Сырой последний сегмент пути НЕ идёт в лейбл метрики напрямую:
            // атакующий шлёт бесконечно много несуществующих путей (/svc/AAAA…),
            // каждый плодил бы новую серию Prometheus (в памяти навсегда) → OOM.
            // Незнакомый путь роутер tonic отдаёт как UNIMPLEMENTED (код 12) —
            // такие сворачиваем в фиксированный лейбл, а auth-интерцептор для них
            // не запускается (per-service, после роутинга), так что код 12 их
            // однозначно ловит. Реальные методы (конечный набор из proto) остаются
            // под своими именами. См. security-скан 2026-07-23, F4 (CWE-770).
            let raw_method = path.rsplit('/').next().unwrap_or(&path);
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            match &res {
                Ok(resp) => {
                    let code = resp
                        .headers()
                        .get("grpc-status")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("0")
                        .to_string();
                    let method =
                        if code == "12" { "unimplemented".to_string() } else { raw_method.to_string() };
                    metrics::counter!("rpc_requests_total", "method" => method.clone(), "code" => code.clone())
                        .increment(1);
                    metrics::histogram!("rpc_duration_seconds", "method" => method.clone())
                        .record(ms / 1000.0);
                    if code == "0" {
                        tracing::debug!(method, ms = format!("{ms:.1}"), "rpc ok");
                    } else {
                        // grpc-message percent-encoded — для лога сойдёт как есть.
                        let msg =
                            resp.headers().get("grpc-message").and_then(|v| v.to_str().ok()).unwrap_or("");
                        tracing::warn!(method, code, ms = format!("{ms:.1}"), msg, "rpc error");
                    }
                }
                Err(_) => {
                    // Транспортная ошибка: путь мог не дойти до роутинга — тоже не
                    // пускаем сырой сегмент в лейбл (тот же вектор кардинальности).
                    metrics::counter!("rpc_requests_total", "method" => "transport", "code" => "transport")
                        .increment(1);
                    tracing::error!(method = raw_method, ms = format!("{ms:.1}"), "rpc transport error");
                }
            }
            res
        })
    }
}
