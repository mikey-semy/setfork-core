# setfork-core

Rust git-ядро SetFork (Gitaly-стиль): обслуживает тяжёлые git-операции и доменные
read/write-порты для Next-BFF по gRPC. Контракты — `proto/git.proto` (GitCore) и
`proto/domain_read.proto` (ListRead/ListWrite/CurationRead/CurationWrite/CollabWrite);
это копии из sethub-app, держать синхронными.

## Структура

```
src/
├─ main.rs             точка входа (bin): CLI-режимы, auth-интерсептор, wiring сервисов
├─ lib.rs              крейт-библиотека (модули видны tests/)
├─ config.rs           вся конфигурация из env, один раз на старте, с валидацией
├─ pb.rs, pb_domain.rs сгенерённые tonic-модули (setfork.git.v1 / setfork.domain.v1)
├─ blocks.rs           блочная модель: is_step/нормализация type/content (одно место)
├─ services/           gRPC-сервисы по доменам (транспортный слой)
│  ├─ git_core.rs      GitCore: smart-HTTP, ветки/теги/merge/bundle
│  ├─ list.rs          ListRead + ListWrite (зеркало list-store.adapter.ts)
│  ├─ curation.rs      CurationRead + CurationWrite (звёзды/watch)
│  ├─ collab.rs        CollabWrite (issues/suggestions/комментарии)
│  ├─ golden.rs        канонический JSON для golden-сверки с TS (CLI domain-read)
│  └─ util.rs          общие хелперы (таксономия ошибок db_status, uuid, jsonb)
├─ git/                git-подсистема (git2, ниже уровня gRPC)
│  ├─ serialize.rs     версия → файлы git-дерева (зеркало serialize.ts)
│  ├─ bundle.rs        материализация репо из истории версий, bundle, pre-receive hook
│  ├─ project.rs       обратное чтение состояния списка из git-дерева (проекция в БД)
│  ├─ repo.rs          персистентные bare-репо (GIT_DATA_DIR), пер-репо локи + lock-пул
│  └─ smart_http.rs    git smart-HTTP (порт smart-http.ts; стриминговый stdin)
├─ db.rs               sqlx/Postgres: пулы (основной + lock), запросы под git-проекцию
├─ ratelimit.rs        tower-layer: скользящее окно per-метод (heavy/обычный бюджеты)
└─ telemetry.rs        tower-layer: метрики Prometheus + per-RPC логи
tests/                 roundtrip (настоящий git), domain (Postgres), golden (фикстуры)
```

Правило слоёв: `services → { git, db }`; `git` и `db` про gRPC не знают.

## Сборка / запуск

```sh
cp .env.example .env   # или задать DATABASE_URL (та же Postgres, что у sethub-app)
cargo build            # protobuf компилирует protox (чистый Rust) — protoc не нужен вовсе
cargo test             # юнит + roundtrip с настоящим git
cargo run              # gRPC-сервер на 127.0.0.1:50051 (SETFORK_CORE_ADDR — override)
```

Обязательное окружение сервера: `DATABASE_URL`, `GIT_DATA_DIR` (общий с фронтом том
bare-репо), `SETFORK_CORE_TOKEN` (Bearer-токен канала; без него старт только с
`SETFORK_ALLOW_INSECURE=1` — локальный dev). Rate-limit: `SETFORK_RPC_RPM`,
`SETFORK_RPC_RPM_HEAVY` (0 = выключить).

## Наблюдаемость

- **Логи** — `tracing`: уровень через `RUST_LOG` (дефолт `info`), `SETFORK_LOG_JSON=1` —
  JSON-строки. Каждый RPC с ошибкой логируется (метод, gRPC-код, латентность, msg);
  успешные — на `debug`. Ошибки проекции — `ERROR` с owner/slug и подсказкой `reproject`.
- **Метрики** — Prometheus на `SETFORK_METRICS_ADDR` (дефолт `127.0.0.1:9464`, `/metrics`;
  `0`/`off` — выключить): `rpc_requests_total{method,code}`, `rpc_duration_seconds{method}`,
  `rpc_rate_limited_total{method}`, `projection_failures_total{op}`, `db_healthy`,
  `db_pool_size`, `db_pool_idle`.
- **Health** (grpc.health.v1) — привязан к БД: фоновая проба `SELECT 1` каждые 5с,
  при недоступности БД сервис уходит в NOT_SERVING (оркестратор уводит трафик) и
  возвращается в SERVING после восстановления.
- **Reflection** (v1 + v1alpha) — `grpcurl -plaintext host:50051 list` работает без
  локальных proto-файлов.

## Golden-сверка с TS

CLI-режимы гоняют тот же код-пас, что и RPC, без транспорта — байты/JSON сверяются
со скриптами sethub-app:

```sh
cargo run -- bundle            <owner> <slug> <out>
cargo run -- advertise-upload  <owner> <slug> <out>
cargo run -- advertise-receive <owner> <slug> <out>
cargo run -- upload-pack       <owner> <slug> <body> <out>
cargo run -- domain-read       <owner> <slug> <out.json>
```

## Восстановление после сбоя проекции

Push/merge принимаются, даже если проекция версии в БД упала (git-объекты целы) —
такая ошибка пишется в лог с пометкой «ОШИБКА проекции». Восстановление вручную
(создаёт новую версию из текущего main-tip; требует `GIT_DATA_DIR`):

```sh
cargo run -- reproject <owner> <slug>
```
