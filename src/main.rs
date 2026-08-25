//! Точка входа: конфиг/env, CLI-режимы golden-сверки, wiring gRPC-сервисов.
//! Вся логика — в services/ (транспорт по доменам), git/ (git-подсистема), db.
use tonic::{Request, Status, transport::Server};

use setfork_core::{config, db, git, pb_domain, ratelimit, services, telemetry};

use git::{bundle, bundle::VersionData, smart_http};
use services::git_core::GitCoreSvc;
use setfork_core::pb::git_core_server::GitCoreServer;

/// Сериализованные proto-дескрипторы (build.rs) — для gRPC server reflection.
const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/descriptor.bin"));

/// Сервис с потолком приёма и интерцептором авторизации.
///
/// Макрос, а не функция: `max_decoding_message_size` — ИНХЕРЕНТНЫЙ метод каждого
/// сгенерированного сервера, общего трейта под него нет, поэтому обобщённо его не
/// вызвать. Тело повторяет то, что делает `with_interceptor` у tonic-build
/// (`InterceptedService::new(Self::new(inner), interceptor)`), — лимит вставлен
/// в середину, потому что на самом InterceptedService таких методов уже нет.
macro_rules! capped {
    ($server:expr, $limit:expr, $auth:expr $(,)?) => {
        tonic::service::interceptor::InterceptedService::new($server.max_decoding_message_size($limit), $auth)
    };
}

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
    init_tracing(); // до Config::from_env — иначе warn'ы валидации конфига пропадут
    let cfg = config::Config::from_env()?;
    let pool = db::connect(&cfg.database_url, cfg.pgpool_max).await?;

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
            // Golden-сверка READ-портов: канонический JSON (см. services::golden)
            //   domain-read <owner> <slug> <out.json>
            "domain-read" => {
                let v = services::golden::golden_json(&pool, &cli(2), &cli(3))
                    .await
                    .map_err(|e| e.to_string())?;
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
            // ШТАТНЫЙ rebuild хвоста проекции (Ф1: git — канон, БД — read-model):
            // проецирует текущий main-tip в НОВУЮ версию списка. Применять, когда
            // git оказался впереди БД (лог «канон записан, проекция отстала»).
            // Не проверяет, была ли версия уже создана — инструмент оператора,
            // см. runbook git-projection-catchup.
            //   reproject <owner> <slug>
            "reproject" => {
                require_git_data_dir()?;
                let (owner, slug) = (cli(2), cli(3));
                let Some((bare, id)) = git::repo::ensure_repo(&pool, &owner, &slug).await? else {
                    return Err(format!("list {owner}/{slug} not found").into());
                };
                match git::project::project_pushed_commit(&pool, id, &bare).await? {
                    Some(v) => println!("reproject {owner}/{slug}: created version v{v}"),
                    // Оба случая называем: list.json может РАЗОБРАТЬСЯ и не иметь шагов, и тогда
                    // «нет валидного list.json» отправило бы починку не туда.
                    None => println!(
                        "reproject {owner}/{slug}: nothing to project (list.json is invalid or has no steps)"
                    ),
                }
                return Ok(());
            }
            // Осиротевшие репозитории: список удалён из БД, а его bare-репо осталось
            // на томе навсегда — с полной историей версий (находка F4 линзы 02).
            // Это и лишний диск, и содержимое, которое автор считает удалённым.
            //
            // РУЧНАЯ команда, а не автоматика: удаление данных обязано быть решением
            // человека. По умолчанию только ПОКАЗЫВАЕТ; сносит с `--apply`.
            //   gc-repos [--apply]
            "gc-repos" => {
                require_git_data_dir()?;
                let apply = args.iter().any(|a| a == "--apply");
                let root = std::path::PathBuf::from(std::env::var("GIT_DATA_DIR")?);
                let ids: Vec<uuid::Uuid> =
                    sqlx::query_scalar("select id from templates").fetch_all(&pool).await?;
                // Fail-closed: пустая выборка почти наверняка значит «не та база», а не
                // «списков нет». Снести по такой выборке ВЕСЬ том нельзя.
                if ids.is_empty() {
                    return Err("no lists in the database - refusing to treat every repo as orphaned".into());
                }
                let live: std::collections::HashSet<uuid::Uuid> = ids.into_iter().collect();
                let paths = git::repo::orphan_repo_dirs(&root, &live)?;
                let (mut orphans, mut bytes) = (0u64, 0u64);
                for path in paths {
                    let size = git::bundle::repo_size_bytes(&path);
                    orphans += 1;
                    bytes += size;
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
                    println!("  {} ({} KB){}", name, size / 1024, if apply { " - removing" } else { "" });
                    if apply {
                        // Список мог родиться ПОКА мы обходили том: снимок живых id
                        // сделан до обхода, и по нему свежий репозиторий выглядит
                        // лишним. Перед сносом спрашиваем базу заново — версии из неё
                        // восстановимы, а принятые пуши, ветки и человеческие теги нет.
                        let stem = name.strip_suffix(".git").unwrap_or(&name);
                        let still_gone: Option<uuid::Uuid> =
                            sqlx::query_scalar("select id from templates where id = $1::uuid")
                                .bind(stem)
                                .fetch_optional(&pool)
                                .await?;
                        if still_gone.is_some() {
                            println!("    {name}: list appeared during the sweep - leaving it alone");
                            orphans -= 1;
                            bytes -= size;
                            continue;
                        }
                        std::fs::remove_dir_all(&path)?;
                    }
                }
                println!(
                    "gc-repos: orphan repos {orphans}, {} KB{}",
                    bytes / 1024,
                    if apply { " - removed" } else { " (dry run; to delete: gc-repos --apply)" }
                );
                return Ok(());
            }
            // Одноразовый догон после снятия ленивой досыпки (Ф1) и общий
            // инструмент выравнивания: каждому списку — репо, синхронное с БД.
            // Идемпотентен, безопасен к повторному запуску. Конфликты (посторонний
            // тег vN и т.п.) только печатает — чинить руками по runbook.
            //   sync-repos
            "sync-repos" => {
                require_git_data_dir()?;
                let rows: Vec<(uuid::Uuid, String, String)> = sqlx::query_as(
                    "select t.id, u.handle, t.slug from templates t join users u on u.id = t.owner_id \
                     order by u.handle, t.slug",
                )
                .fetch_all(&pool)
                .await?;
                let total = rows.len();
                let (mut in_sync, mut boot, mut appended, mut projected, mut conflicts) = (0, 0, 0, 0, 0);
                let mut restored = 0;
                for (id, handle, slug) in rows {
                    let bare = git::repo::repo_path(id);
                    let _guard = git::repo::repo_guard(&pool, id).await?;
                    match git::version::sync_repo_with_db(&pool, id, &bare).await {
                        Ok(git::version::SyncOutcome::InSync) => in_sync += 1,
                        Ok(git::version::SyncOutcome::Bootstrapped { versions }) => {
                            boot += 1;
                            println!("  {handle}/{slug}: repo created ({versions} versions)");
                        }
                        Ok(git::version::SyncOutcome::MainRestored { version }) => {
                            restored += 1;
                            println!("  {handle}/{slug}: main was missing, restored from tag v{version}");
                        }
                        Ok(git::version::SyncOutcome::Appended { from, to }) => {
                            appended += 1;
                            println!("  {handle}/{slug}: caught up v{from}..v{to}");
                        }
                        Ok(git::version::SyncOutcome::ProjectedTip { version }) => {
                            projected += 1;
                            println!("  {handle}/{slug}: tip projected into db -> v{version}");
                        }
                        Ok(git::version::SyncOutcome::Conflict { have, current }) => {
                            conflicts += 1;
                            println!(
                                "  WARN {handle}/{slug}: CONFLICT git v{have} vs db v{current} - needs hands, \
                                 see runbook git-projection-catchup"
                            );
                        }
                        Err(e) => {
                            conflicts += 1;
                            println!("  WARN {handle}/{slug}: error - {e}");
                        }
                    }
                }
                println!(
                    "sync-repos: total {total}; in sync {in_sync}, created {boot}, main restored {restored}, \
                     caught up {appended}, projected {projected}, conflicts {conflicts}"
                );
                if conflicts > 0 {
                    return Err(format!("{conflicts} repos need manual intervention").into());
                }
                return Ok(());
            }
            // не CLI-команда → падём в режим сервера ниже
            _ => {}
        }
    }

    // Fail-fast: обязательное окружение сервера проверяем на старте, а не паникой
    // при первом запросе (аудит 2026-07-20, P1-7). Golden-CLI выше работают без
    // GIT_DATA_DIR (материализация во временные каталоги) — их не ужесточаем.
    require_git_data_dir()?;

    // Гейт записи (ADR-0015) бесполезен без адреса приложения — а «бесполезен»
    // здесь означает «запись никто не проверяет». Поэтому не предупреждение, а
    // остановка: молча деградировать до открытой двери нельзя. Явный локальный
    // dev — тем же опт-аутом, что и для токена канала.
    //
    // Проверяем ФОРМУ адреса, а не только его наличие: мусорное значение
    // стартовало бы молча и отклоняло КАЖДУЮ запись по fail-closed, причём в
    // логах было бы пусто, пока никто не пишет (инцидент .com 01.08 —
    // SETFORK_APP_URL=setfork-frontend-zpyzi8, имя без схемы и порта).
    match setfork_core::gate::parse_app_url(std::env::var("SETFORK_APP_URL").ok().as_deref()) {
        Ok(url) => tracing::info!(app_url = %url, "write precondition is asked here"),
        Err(e) if cfg.allow_insecure => {
            tracing::warn!(
                ?e,
                "SETFORK_APP_URL is unusable: the write precondition is NOT checked \
                 (a frozen list will accept writes). Local dev only."
            );
        }
        Err(setfork_core::gate::BadAppUrl::Missing) => {
            eprintln!(
                "setfork-core: STOPPED - SETFORK_APP_URL is not set. Without it the core cannot \
                 ask the app whether a write is allowed (ADR-0015), so freezing and archiving a \
                 list stop working on git paths. Set the app address (e.g. http://app:3000), or \
                 for local dev set SETFORK_ALLOW_INSECURE=1."
            );
            std::process::exit(1);
        }
        Err(setfork_core::gate::BadAppUrl::NotAbsolute(got)) => {
            eprintln!(
                "setfork-core: STOPPED - SETFORK_APP_URL='{got}' is not an absolute \
                 address. A scheme and full host are required, e.g. http://setfork-frontend:3000 \
                 (the app service name in the docker network plus its port). With this value the \
                 core never reaches the app, and EVERY write is refused by fail-closed."
            );
            std::process::exit(1);
        }
        Err(setfork_core::gate::BadAppUrl::TlsUnsupported(got)) => {
            eprintln!(
                "setfork-core: STOPPED - SETFORK_APP_URL='{got}' uses https, but the \
                 write precondition client is built without TLS: the call targets the internal \
                 network where encryption is not needed, and rustls is not pulled in for it. Use \
                 the http address of the service inside the docker network. If the app really is \
                 reachable ONLY over https, that needs a TLS connector in gate.rs."
            );
            std::process::exit(1);
        }
    }

    // Отдельный мини-пул под advisory-локи: RepoGuard держит соединение на всё
    // время git-операции — не выедаем основной пул (аудит 2026-07-20, P1-5).
    git::repo::set_lock_pool(db::connect_lock_pool(&cfg.database_url).await?);

    let n = db::published_count(&pool).await?;
    tracing::info!(published_lists = n, "connected to Postgres");

    let addr = cfg.addr;

    // Метрики Prometheus на отдельном HTTP-порту (/metrics). '0'/'off' — выключить.
    if let Some(sock) = cfg.metrics_addr {
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(sock)
            .set_buckets(&[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0])?
            .install()?;
        tracing::info!("Prometheus metrics: http://{sock}/metrics");
    }

    // gRPC health-check (grpc.health.v1) — для проб оркестратора/LB.
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter.set_serving::<GitCoreServer<GitCoreSvc>>().await;

    // Health ← реальное состояние БД: фоновая проба SELECT 1 раз в 5с.
    // БД недоступна → NOT_SERVING (оркестратор уводит трафик), восстановилась →
    // SERVING обратно. Заодно публикуем гейджи пула. Флаг shutting_down не даёт
    // пробе вернуть SERVING во время дренажа.
    let shutting_down = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let hr = health_reporter.clone();
        let pool_h = pool.clone();
        let shutting = shutting_down.clone();
        tokio::spawn(async move {
            let mut healthy = true;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if shutting.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                // Снимок пула — ДО пробы, а не после. Проба берёт соединение из этого же
                // пула и держит его, пока идёт `select 1`: замер после неё показывал бы
                // на одно свободное меньше, чем есть. При пуле из одного соединения это
                // ровно «db_pool_idle 0» ВСЕГДА — то есть прибор, по которому судят о
                // насыщении пула, врал бы в сторону тревоги (замер на проде 24.08).
                metrics::gauge!("db_pool_size").set(pool_h.size() as f64);
                metrics::gauge!("db_pool_idle").set(pool_h.num_idle() as f64);
                let ok = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    sqlx::query("select 1").execute(&pool_h),
                )
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false);
                metrics::gauge!("db_healthy").set(if ok { 1.0 } else { 0.0 });
                if ok != healthy {
                    healthy = ok;
                    if ok {
                        tracing::info!("db is reachable again, health SERVING");
                        hr.set_serving::<GitCoreServer<GitCoreSvc>>().await;
                    } else {
                        tracing::error!("db unreachable, health NOT_SERVING");
                        hr.set_not_serving::<GitCoreServer<GitCoreSvc>>().await;
                    }
                }
            }
        });
    }

    // gRPC server reflection (v1 + v1alpha для старых grpcurl). Без auth: схема
    // не секрет (proto лежат в репо), канал и так внутренний.
    let reflection_v1 = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
        .build_v1()?;
    let reflection_v1a = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
        .build_v1alpha()?;

    // Graceful shutdown: по SIGTERM/SIGINT сперва снимаем SERVING (оркестратор
    // уводит трафик), затем serve_with_shutdown перестаёт принимать и до-обслуживает
    // текущие RPC (напр. идущий receive-pack не обрывается на середине).
    let shutdown = {
        let shutting = shutting_down.clone();
        let health_reporter = health_reporter.clone();
        async move {
            wait_for_signal().await;
            shutting.store(true, std::sync::atomic::Ordering::Relaxed);
            health_reporter.set_not_serving::<GitCoreServer<GitCoreSvc>>().await;
            tracing::info!("shutdown signal received, draining active RPCs");
        }
    };

    tracing::info!(%addr, "setfork-core git-core listening");
    // Auth канала Next↔ядро: общий Bearer-токен (SETFORK_CORE_TOKEN). Ядро НЕ делает
    // пользовательской авторизации (BFF-модель: весь гейт владения/модерации — на фронте),
    // поэтому токен канала — ЕДИНСТВЕННАЯ граница доступа ко всей записи/чтению контента.
    // Fail-closed: без токена сервер не стартует — иначе любой, кто дотянулся до порта,
    // получает суперправа над всем контентом всех пользователей. Явный локальный dev без
    // токена — только с SETFORK_ALLOW_INSECURE=1. Health без авторизации (docker/k8s-пробы).
    if cfg.token.is_none() && !cfg.allow_insecure {
        eprintln!(
            "setfork-core: STOPPED - SETFORK_CORE_TOKEN is not set. Without it the channel is \
             open to anyone on the network (a full bypass of ownership and moderation). Set \
             the token (the same one for the app), or for local dev set \
             SETFORK_ALLOW_INSECURE=1."
        );
        std::process::exit(1);
    }
    let token: Option<&'static str> = cfg.token.clone().map(|t| &*format!("Bearer {t}").leak());
    match token {
        Some(_) => tracing::info!("channel secured with a Bearer token"),
        None => tracing::warn!("SETFORK_ALLOW_INSECURE=1: channel WITHOUT authorization (local dev only)"),
    }
    let check_auth = move |req: Request<()>| -> Result<Request<()>, Status> {
        let got = req.metadata().get("authorization").and_then(|v| v.to_str().ok());
        if auth_ok(token, got) { Ok(req) } else { Err(Status::unauthenticated("invalid core token")) }
    };

    // Потолок ПРИЁМА — ЯВНО на каждом сервисе. Дефолты tonic асимметричны
    // (codec/mod.rs 0.14.6): приём 4 МиБ, отдача usize::MAX. Необъявленный
    // потолок есть ровно на входе и бьёт по ReceivePack — тело пуша едет одним
    // сообщением, и на 4 МиБ push молча перестал бы проходить.
    //
    // Отдачу НЕ ограничиваем сознательно: сейчас она без потолка, и конечное
    // число СОЗДАЛО бы ограничение клона и бандла там, где его нет. Памяти это
    // не сэкономит — пак и так собирается в Vec<u8> целиком.
    let max_recv = cfg.max_recv_bytes;
    tracing::info!(max_recv_mb = max_recv / (1024 * 1024), "inbound gRPC message limit");

    Server::builder()
        // telemetry — СНАРУЖИ rate-limit: отказы 8/16 тоже попадают в метрики.
        .layer(telemetry::TelemetryLayer)
        .layer(ratelimit::RateLimitLayer::new(cfg.rpm, cfg.rpm_heavy, cfg.token.as_deref()))
        .add_service(health_service)
        .add_service(reflection_v1)
        .add_service(reflection_v1a)
        // capped! навешивает лимит на СГЕНЕРИРОВАННЫЙ сервер: `with_interceptor`
        // возвращает уже InterceptedService, у которого таких методов нет.
        // Макрос, а не функция: max_decoding_message_size — ИНХЕРЕНТНЫЙ метод
        // каждого сгенерированного типа, общего трейта под него нет.
        .add_service(capped!(GitCoreServer::new(GitCoreSvc { pool: pool.clone() }), max_recv, check_auth))
        .add_service(capped!(
            pb_domain::list_read_server::ListReadServer::new(services::list::ListReadSvc {
                pool: pool.clone(),
            }),
            max_recv,
            check_auth,
        ))
        .add_service(capped!(
            pb_domain::curation_read_server::CurationReadServer::new(services::curation::CurationReadSvc {
                pool: pool.clone()
            },),
            max_recv,
            check_auth,
        ))
        .add_service(capped!(
            pb_domain::curation_write_server::CurationWriteServer::new(
                services::curation::CurationWriteSvc { pool: pool.clone() },
            ),
            max_recv,
            check_auth,
        ))
        .add_service(capped!(
            pb_domain::collab_write_server::CollabWriteServer::new(services::collab::CollabWriteSvc {
                pool: pool.clone(),
            }),
            max_recv,
            check_auth,
        ))
        .add_service(capped!(
            pb_domain::list_write_server::ListWriteServer::new(services::list::ListWriteSvc { pool }),
            max_recv,
            check_auth,
        ))
        .serve_with_shutdown(addr, shutdown)
        .await?;
    tracing::info!("stopped cleanly");
    Ok(())
}

/// Логи: уровень из RUST_LOG (дефолт info), SETFORK_LOG_JSON=1 — JSON-строки
/// (структурные логи для сборщиков; по умолчанию человекочитаемый формат).
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let json = std::env::var("SETFORK_LOG_JSON").map(|v| v == "1").unwrap_or(false);
    if json {
        tracing_subscriber::fmt().with_env_filter(filter).json().init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

/// Сверка канала: заголовок authorization против ожидаемого `Bearer <token>`.
/// None = канал открыт явным опт-аутом (SETFORK_ALLOW_INSECURE=1, локальный dev).
/// constant-time не нужен: токен длинный и случайный, тайминг не течёт полезно,
/// но сравнение всё равно полное (eq по всей строке).
fn auth_ok(expected: Option<&str>, got: Option<&str>) -> bool {
    match expected {
        None => true,
        Some(exp) => got == Some(exp),
    }
}

/// Fail-fast проверка GIT_DATA_DIR: задан и доступен на запись (создаём при отсутствии).
/// Нужен серверу и reproject; golden-CLI-режимы работают без него.
fn require_git_data_dir() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::var("GIT_DATA_DIR").unwrap_or_default();
    if root.trim().is_empty() {
        return Err("GIT_DATA_DIR is not set (git object volume shared with the frontend, see README)".into());
    }
    std::fs::create_dir_all(&root).map_err(|e| format!("GIT_DATA_DIR '{root}' is not writable: {e}"))?;
    Ok(())
}

/// Ждёт первый из сигналов остановки: Ctrl-C (SIGINT) или SIGTERM (docker stop / k8s).
async fn wait_for_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("could not install the Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("could not install the SIGTERM handler")
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

#[cfg(test)]
mod tests {
    use super::auth_ok;

    #[test]
    fn auth_open_channel_allows_everyone() {
        // Явный опт-аут (SETFORK_ALLOW_INSECURE=1): проверка отключена.
        assert!(auth_ok(None, None));
        assert!(auth_ok(None, Some("Bearer anything")));
    }

    #[test]
    fn auth_closed_channel_requires_exact_token() {
        let exp = Some("Bearer secret-token");
        assert!(auth_ok(exp, Some("Bearer secret-token")));
        assert!(!auth_ok(exp, None), "без заголовка — отказ");
        assert!(!auth_ok(exp, Some("Bearer wrong")), "чужой токен — отказ");
        assert!(!auth_ok(exp, Some("secret-token")), "без префикса Bearer — отказ");
        assert!(!auth_ok(exp, Some("Bearer secret-token ")), "хвостовой пробел — отказ");
        assert!(!auth_ok(exp, Some("bearer secret-token")), "регистр схемы — отказ (сверка строгая)");
    }
}
