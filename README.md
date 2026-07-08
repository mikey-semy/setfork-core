# setfork-core

Rust git-ядро SetFork (Gitaly-стиль): обслуживает тяжёлые git-операции и доменные
read/write-порты для Next-BFF по gRPC. Контракты — `proto/git.proto` (GitCore) и
`proto/domain_read.proto` (ListRead/ListWrite/CurationRead/CurationWrite/CollabWrite);
это копии из sethub-app, держать синхронными.

## Структура

```
src/
├─ main.rs             точка входа: env/CLI golden-режимы, auth-интерсептор, wiring сервисов
├─ pb.rs, pb_domain.rs сгенерённые tonic-модули (setfork.git.v1 / setfork.domain.v1)
├─ services/           gRPC-сервисы по доменам (транспортный слой)
│  ├─ git_core.rs      GitCore: smart-HTTP, ветки/теги/merge/bundle
│  ├─ list.rs          ListRead + ListWrite (зеркало list-store.adapter.ts, golden-сверка)
│  ├─ curation.rs      CurationRead + CurationWrite (звёзды/watch)
│  ├─ collab.rs        CollabWrite (issues/suggestions/комментарии)
│  └─ util.rs          общие хелперы (ошибки, uuid, LocaleText/refs ↔ jsonb)
├─ git/                git-подсистема (git2, ниже уровня gRPC)
│  ├─ bundle.rs        сериализация версий в git-дерево (зеркало serialize.ts), материализация
│  ├─ project.rs       обратное чтение состояния списка из git-дерева (проекция в БД)
│  ├─ repo.rs          персистентные bare-репо (GIT_DATA_DIR), пер-репо локи
│  └─ smart_http.rs    git smart-HTTP (порт smart-http.ts)
├─ db.rs               sqlx/Postgres: пул, запросы под git-проекцию
├─ ratelimit.rs        tower-layer: скользящее окно per-метод (heavy/обычный бюджеты)
└─ roundtrip_tests.rs  интеграционный тест: bootstrap → clone → push → append → pull
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
