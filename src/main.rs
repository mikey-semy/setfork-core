// Точка входа: конфиг/env, CLI-режимы golden-сверки, wiring gRPC-сервисов.
// Вся логика — в services/ (транспорт по доменам), git/ (git-подсистема), db.
use tonic::{transport::Server, Request, Status};

mod blocks;
mod db;
mod git;
pub mod pb;
pub mod pb_domain;
mod ratelimit;
mod services;
#[cfg(test)]
mod roundtrip_tests;

use git::{bundle, bundle::VersionData, smart_http};
use pb::git_core_server::GitCoreServer;
use services::git_core::GitCoreSvc;

// Материализует репо во временный каталог, выполняет `op` над ним и гарантированно
// удаляет каталог. `op` синхронна (шелл git) — весь блок идёт в spawn_blocking.
fn with_materialized<F>(versions: Vec<VersionData>, op: F) -> std::io::Result<Vec<u8>>
where
    F: FnOnce(&std::path::Path) -> std::io::Result<Vec<u8>>,
{
    let dir = bundle::materialize_repo(&versions)?;
    let res = op(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    res
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let pool = db::connect().await?;

    // CLI-режимы для golden-проверки (тот же код-пас, что и RPC, без gRPC-транспорта):
    //   bundle           <owner> <slug> <out>
    //   advertise-upload <owner> <slug> <out>   (== GET /info/refs?service=git-upload-pack)
    //   advertise-receive<owner> <slug> <out>   (== GET /info/refs?service=git-receive-pack)
    //   upload-pack      <owner> <slug> <body> <out>  (== POST /git-upload-pack)
    let args: Vec<String> = std::env::args().collect();
    if let Some(cmd) = args.get(1).map(|s| s.as_str()) {
        let cli = |i: usize| args.get(i).cloned().unwrap_or_default();
        // pool клонируется (Arc внутри) — оригинал остаётся серверу ниже.
        let svc = GitCoreSvc { pool: pool.clone() };
        match cmd {
            "bundle" => {
                let data = svc.build(&cli(2), &cli(3)).await.map_err(|e| e.to_string())?;
                std::fs::write(cli(4), &data)?;
                println!("wrote {} bytes → {}", data.len(), cli(4));
                return Ok(());
            }
            "advertise-upload" | "advertise-receive" => {
                let versions = svc.load(&cli(2), &cli(3)).await.map_err(|e| e.to_string())?;
                let up = cmd == "advertise-upload";
                let data = tokio::task::spawn_blocking(move || {
                    with_materialized(versions, |dir| {
                        if up {
                            smart_http::upload_pack_advertise(dir, None)
                        } else {
                            smart_http::receive_pack_advertise(dir, None)
                        }
                    })
                })
                .await??;
                std::fs::write(cli(4), &data)?;
                println!("wrote {} bytes → {}", data.len(), cli(4));
                return Ok(());
            }
            // Golden-сверка READ-портов: канонический JSON (см. services::list::golden_json)
            //   domain-read <owner> <slug> <out.json>
            "domain-read" => {
                let v = services::list::golden_json(&pool, &cli(2), &cli(3)).await.map_err(|e| e.to_string())?;
                std::fs::write(cli(4), serde_json::to_string_pretty(&v)?)?;
                println!("wrote domain-read json → {}", cli(4));
                return Ok(());
            }
            "upload-pack" => {
                let versions = svc.load(&cli(2), &cli(3)).await.map_err(|e| e.to_string())?;
                let body = std::fs::read(cli(4))?;
                let data = tokio::task::spawn_blocking(move || {
                    with_materialized(versions, |dir| smart_http::upload_pack_rpc(dir, &body, None))
                })
                .await??;
                std::fs::write(cli(5), &data)?;
                println!("wrote {} bytes → {}", data.len(), cli(5));
                return Ok(());
            }
            // не CLI-команда → падём в режим сервера ниже
            _ => {}
        }
    }

    let n = db::published_count(&pool).await?;
    println!("setfork-core: connected to Postgres — {n} published public lists");

    let addr = std::env::var("SETFORK_CORE_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:50051".to_string())
        .parse()?;

    // gRPC health-check (grpc.health.v1) — для проб оркестратора/LB.
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter.set_serving::<GitCoreServer<GitCoreSvc>>().await;

    // Graceful shutdown: по SIGTERM/SIGINT сперва снимаем SERVING (оркестратор
    // уводит трафик), затем serve_with_shutdown перестаёт принимать и до-обслуживает
    // текущие RPC (напр. идущий receive-pack не обрывается на середине).
    let shutdown = async move {
        wait_for_signal().await;
        let mut health_reporter = health_reporter;
        health_reporter.set_not_serving::<GitCoreServer<GitCoreSvc>>().await;
        println!("setfork-core: получен сигнал остановки — дренаж активных RPC…");
    };

    println!("setfork-core git-core listening on {addr}");
    // Auth канала Next↔ядро: общий Bearer-токен (SETFORK_CORE_TOKEN). Ядро НЕ делает
    // пользовательской авторизации (BFF-модель: весь гейт владения/модерации — на фронте),
    // поэтому токен канала — ЕДИНСТВЕННАЯ граница доступа ко всей записи/чтению контента.
    // Fail-closed: без токена сервер не стартует — иначе любой, кто дотянулся до порта,
    // получает суперправа над всем контентом всех пользователей. Явный локальный dev без
    // токена — только с SETFORK_ALLOW_INSECURE=1. Health без авторизации (docker/k8s-пробы).
    let raw_token = std::env::var("SETFORK_CORE_TOKEN").ok().filter(|t| !t.is_empty());
    let allow_insecure = std::env::var("SETFORK_ALLOW_INSECURE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if raw_token.is_none() && !allow_insecure {
        eprintln!(
            "setfork-core: ОСТАНОВКА — SETFORK_CORE_TOKEN не задан. Без него канал открыт кому \
             угодно в сети (полный обход владения и модерации). Задайте токен (тот же — фронту), \
             либо для локального dev явно выставьте SETFORK_ALLOW_INSECURE=1."
        );
        std::process::exit(1);
    }
    let token: Option<&'static str> =
        raw_token.map(|t| &*Box::leak(format!("Bearer {t}").into_boxed_str()));
    match token {
        Some(_) => println!("setfork-core: канал защищён Bearer-токеном"),
        None => println!("setfork-core: ВНИМАНИЕ — SETFORK_ALLOW_INSECURE=1, канал БЕЗ авторизации (только локальный dev)"),
    }
    let check_auth = move |req: Request<()>| -> Result<Request<()>, Status> {
        let Some(expected) = token else { return Ok(req) };
        match req.metadata().get("authorization").and_then(|v| v.to_str().ok()) {
            // constant-time не нужен: токен длинный и случайный, тайминг не течёт полезно,
            // но сравнение всё равно полное (eq по всей строке).
            Some(got) if got == expected => Ok(req),
            _ => Err(Status::unauthenticated("invalid core token")),
        }
    };

    Server::builder()
        .layer(ratelimit::RateLimitLayer::from_env())
        .add_service(health_service)
        .add_service(GitCoreServer::with_interceptor(GitCoreSvc { pool: pool.clone() }, check_auth))
        .add_service(pb_domain::list_read_server::ListReadServer::with_interceptor(
            services::list::ListReadSvc { pool: pool.clone() },
            check_auth,
        ))
        .add_service(pb_domain::curation_read_server::CurationReadServer::with_interceptor(
            services::curation::CurationReadSvc { pool: pool.clone() },
            check_auth,
        ))
        .add_service(pb_domain::curation_write_server::CurationWriteServer::with_interceptor(
            services::curation::CurationWriteSvc { pool: pool.clone() },
            check_auth,
        ))
        .add_service(pb_domain::collab_write_server::CollabWriteServer::with_interceptor(
            services::collab::CollabWriteSvc { pool: pool.clone() },
            check_auth,
        ))
        .add_service(pb_domain::list_write_server::ListWriteServer::with_interceptor(
            services::list::ListWriteSvc { pool },
            check_auth,
        ))
        .serve_with_shutdown(addr, shutdown)
        .await?;
    println!("setfork-core: остановлен чисто");
    Ok(())
}

/// Ждёт первый из сигналов остановки: Ctrl-C (SIGINT) или SIGTERM (docker stop / k8s).
async fn wait_for_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("не удалось повесить обработчик Ctrl-C");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("не удалось повесить обработчик SIGTERM")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
