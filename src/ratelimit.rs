//! Rate-limit RPC (tower-layer поверх tonic): скользящее окно 60с на метод.
//! Канал закрыт Bearer-токеном (единственный вызывающий — наш фронт), поэтому
//! лимит глобальный per-метод: страхует от разгона багом/циклом и усиления
//! абьюза через фронт. Health не лимитируется (layer навешивается до него —
//! см. main.rs: путь /grpc.health отфильтрован явно).
//!
//! Бюджеты: тяжёлые методы (git-мутации/bundle) — SETFORK_RPC_RPM_HEAVY
//! (дефолт 60/мин), остальные — SETFORK_RPC_RPM (дефолт 600/мин). 0 = выключить.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request, Response};
use tonic::body::Body;
use tower::{Layer, Service};

const WINDOW: Duration = Duration::from_secs(60);

// Тяжёлые методы: git-мутации, merge, bundle (материализация репо), а также доменные
// write, наполняющие БД (список/версия + шаги в транзакции) — чтобы Create/AddVersion
// не были вектором массового наполнения на обычном бюджете 600/мин.
const HEAVY: &[&str] = &[
    "ReceivePack",
    "CreateBundle",
    "MergeBranch",
    "MergeResolved",
    "MirrorPush", // сеть до форджи — не даём разгонять
    "CreateBranch",
    "DeleteBranch",
    "Create",     // ListWrite.Create — вставка списка+версии+шагов
    "AddVersion", // ListWrite.AddVersion — новая версия + шаги
];

#[derive(Clone)]
pub struct RateLimitLayer {
    inner: Arc<State>,
}

struct State {
    rpm: u32,
    rpm_heavy: u32,
    // Ожидаемый `Bearer <token>` (если канал закрыт). Неавторизованные запросы
    // НЕ расходуют окно — иначе кто угодно с доступом к порту исчерпает бюджет
    // метода и положит обслуживание (DoS-амплификация до интерцептора auth).
    expected_auth: Option<String>,
    windows: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimitLayer {
    /// Параметры — из config::Config: env здесь больше не читаем (токен
    /// читался дважды — main и этот модуль; аудит 2026-07-20, P2-14).
    pub fn new(rpm: u32, rpm_heavy: u32, token: Option<&str>) -> Self {
        tracing::info!(rpm, rpm_heavy, "rate-limit per-метод (0 = выкл)");
        Self::with(rpm, rpm_heavy, token.map(|t| format!("Bearer {t}")))
    }

    // Явные параметры — общий конструктор new и юнит-тестов.
    fn with(rpm: u32, rpm_heavy: u32, expected_auth: Option<String>) -> Self {
        Self { inner: Arc::new(State { rpm, rpm_heavy, expected_auth, windows: Mutex::new(HashMap::new()) }) }
    }
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimited<S>;
    fn layer(&self, service: S) -> Self::Service {
        RateLimited { inner: service, state: self.inner.clone() }
    }
}

#[derive(Clone)]
pub struct RateLimited<S> {
    inner: S,
    state: Arc<State>,
}

impl<S> RateLimited<S> {
    /// true = запрос пропускаем. Путь вида /setfork.git.v1.GitCore/ReceivePack.
    fn allow(&self, path: &str, auth: Option<&str>) -> bool {
        // health и рефлексия — без лимита (docker/k8s-пробы, grpcurl).
        if path.starts_with("/grpc.health") || path.starts_with("/grpc.reflection") {
            return true;
        }
        // Неавторизованные не расходуют окно: пропускаем сюда, интерцептор ниже
        // всё равно вернёт unauthenticated. Так лимит защищает только реальный
        // (авторизованный) трафик, а не служит вектором DoS.
        if let Some(expected) = &self.state.expected_auth
            && auth != Some(expected.as_str())
        {
            return true;
        }
        let method = path.rsplit('/').next().unwrap_or(path);
        let limit = if HEAVY.contains(&method) { self.state.rpm_heavy } else { self.state.rpm };
        if limit == 0 {
            return true;
        }
        let now = Instant::now();
        // Отравление мьютекса (паника держателя) не должно ронять весь трафик:
        // окна внутри валидны как данные — забираем их как есть.
        let mut windows = self.state.windows.lock().unwrap_or_else(|p| p.into_inner());
        let q = windows.entry(method.to_string()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) > WINDOW) {
            q.pop_front();
        }
        if q.len() as u32 >= limit {
            metrics::counter!("rpc_rate_limited_total", "method" => method.to_string()).increment(1);
            return false;
        }
        q.push_back(now);
        true
    }
}

impl<S, ReqBody> Service<Request<ReqBody>> for RateLimited<S>
where
    S: Service<Request<ReqBody>, Response = Response<Body>> + Send,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = futures_util::future::Either<
        S::Future,
        futures_util::future::Ready<Result<Self::Response, Self::Error>>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned);
        if self.allow(req.uri().path(), auth.as_deref()) {
            futures_util::future::Either::Left(self.inner.call(req))
        } else {
            // gRPC-отказ без вызова inner: RESOURCE_EXHAUSTED в trailers-only ответе.
            let resp = Response::builder()
                .status(200)
                .header("content-type", "application/grpc")
                .header("grpc-status", "8") // RESOURCE_EXHAUSTED
                .header("grpc-message", "rate limited")
                .body(Body::empty())
                .expect("static response");
            futures_util::future::Either::Right(futures_util::future::ready(Ok(resp)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limited(rpm: u32, rpm_heavy: u32, auth: Option<&str>) -> RateLimited<()> {
        let layer = RateLimitLayer::with(rpm, rpm_heavy, auth.map(str::to_string));
        RateLimited { inner: (), state: layer.inner }
    }

    const M: &str = "/setfork.domain.v1.ListRead/GetList"; // обычный метод
    const H: &str = "/setfork.git.v1.GitCore/ReceivePack"; // тяжёлый метод

    #[test]
    fn allows_under_limit_blocks_over() {
        let rl = limited(2, 60, None);
        assert!(rl.allow(M, None));
        assert!(rl.allow(M, None));
        assert!(!rl.allow(M, None), "третий запрос сверх rpm=2 в одном окне");
    }

    #[test]
    fn heavy_has_separate_budget() {
        let rl = limited(10, 1, None);
        assert!(rl.allow(H, None));
        assert!(!rl.allow(H, None), "второй тяжёлый сверх heavy=1");
        assert!(rl.allow(M, None), "обычный бюджет не задет тяжёлым");
    }

    #[test]
    fn zero_limit_disables() {
        let rl = limited(0, 0, None);
        for _ in 0..100 {
            assert!(rl.allow(M, None));
            assert!(rl.allow(H, None));
        }
    }

    #[test]
    fn health_and_reflection_bypass() {
        let rl = limited(1, 1, None);
        assert!(rl.allow(M, None));
        assert!(!rl.allow(M, None));
        for _ in 0..10 {
            assert!(rl.allow("/grpc.health.v1.Health/Check", None));
            assert!(rl.allow("/grpc.reflection.v1.ServerReflection/ServerReflectionInfo", None));
        }
    }

    #[test]
    fn unauthorized_do_not_consume_window() {
        let rl = limited(1, 1, Some("Bearer secret"));
        // Без/с чужим токеном — пропускаем (auth-интерцептор ниже вернёт 16),
        // окно НЕ расходуется: иначе любой с доступом к порту DoS-ит бюджет метода.
        for _ in 0..10 {
            assert!(rl.allow(M, None));
            assert!(rl.allow(M, Some("Bearer wrong")));
        }
        assert!(rl.allow(M, Some("Bearer secret")), "первый авторизованный проходит");
        assert!(!rl.allow(M, Some("Bearer secret")), "второй авторизованный сверх rpm=1");
    }
}
