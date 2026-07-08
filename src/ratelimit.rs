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
use tonic::body::BoxBody;
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
    "CreateBranch",
    "DeleteBranch",
    "Create",     // ListWrite.Create — вставка списка+версии+шагов
    "AddVersion", // ListWrite.AddVersion — новая версия + шаги
];

fn env_limit(name: &str, default: u32) -> u32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

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
    pub fn from_env() -> Self {
        let rpm = env_limit("SETFORK_RPC_RPM", 600);
        let rpm_heavy = env_limit("SETFORK_RPC_RPM_HEAVY", 60);
        let expected_auth = std::env::var("SETFORK_CORE_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .map(|t| format!("Bearer {t}"));
        println!("setfork-core: rate-limit {rpm}/мин (тяжёлые {rpm_heavy}/мин; 0 = выкл)");
        Self {
            inner: Arc::new(State { rpm, rpm_heavy, expected_auth, windows: Mutex::new(HashMap::new()) }),
        }
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
        // health и рефлексия — без лимита (docker/k8s-пробы).
        if path.starts_with("/grpc.health") {
            return true;
        }
        // Неавторизованные не расходуют окно: пропускаем сюда, интерцептор ниже
        // всё равно вернёт unauthenticated. Так лимит защищает только реальный
        // (авторизованный) трафик, а не служит вектором DoS.
        if let Some(expected) = &self.state.expected_auth {
            if auth != Some(expected.as_str()) {
                return true;
            }
        }
        let method = path.rsplit('/').next().unwrap_or(path);
        let limit = if HEAVY.contains(&method) { self.state.rpm_heavy } else { self.state.rpm };
        if limit == 0 {
            return true;
        }
        let now = Instant::now();
        let mut windows = self.state.windows.lock().unwrap();
        let q = windows.entry(method.to_string()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) > WINDOW) {
            q.pop_front();
        }
        if q.len() as u32 >= limit {
            return false;
        }
        q.push_back(now);
        true
    }
}

impl<S, ReqBody> Service<Request<ReqBody>> for RateLimited<S>
where
    S: Service<Request<ReqBody>, Response = Response<BoxBody>> + Send,
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
                .body(tonic::body::empty_body())
                .expect("static response");
            futures_util::future::Either::Right(futures_util::future::ready(Ok(resp)))
        }
    }
}
