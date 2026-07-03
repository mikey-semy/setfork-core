# setfork-core

Rust git-ядро SetFork (Gitaly-стиль): реализует `proto/git.proto` (`service GitCore`)
и обслуживает тяжёлые git-операции для Next-BFF. Часть Фазы 2 плана
`sethub-app/docs/rust-core-plan.md` + `sethub-app/docs/phase1-wire-contract.md`.

## Статус: собирается, запускается, читает Postgres ✅

- ✅ Cargo-проект: `tonic` (gRPC) + `prost` + `tokio` + `sqlx` (Postgres); `build.rs` кодогенит из
  `proto/git.proto` (protoc — из крейта `protoc-bin-vendored`, системный не нужен).
- ✅ `src/main.rs`: tonic-сервер + трейт `GitCore` (5 RPC пока `unimplemented`).
- ✅ `src/db.rs`: sqlx-подключение (`DATABASE_URL` из `.env`, та же БД что у Next), резолв
  owner/slug→list, self-check при старте. **Проверено:** `cargo build` ок; запуск подключается к
  Postgres, считает списки и резолвит `demo/redis-…` → uuid+версия.

## Сборка / запуск

```sh
cp .env.example .env   # или задать DATABASE_URL (та же Postgres, что у sethub-app)
cargo build            # скачает tonic/tokio/prost/sqlx, сгенерит стабы из proto
cargo run              # self-check БД + gRPC-сервер на 127.0.0.1:50051 (SETFORK_CORE_ADDR — override)
```

## Что дальше (Фаза 2, послойно)

1. **sqlx** (та же Postgres, `DATABASE_URL`) — резолв owner/slug→list, загрузка истории версий.
2. **Материализация репо** — на старте можно **шеллить `git`** (как TS `store.ts`), детерминированные
   SHA (фикс. автор/даты) должны совпасть с TS → golden-сверка.
3. Реализовать RPC по одному: `CreateBundle` → `InfoRefsUploadPack` → `UploadPack` (clone/pull) →
   `ReceivePack` (push + проекция `list.json`→версия). Позже: **gix** (read) / **git2** (write) вместо шелла.
4. На TS-стороне: `buf` + Connect-ES кодоген из `sethub-app/proto/git.proto` → реализовать
   `gitCoreRemote` в `sethub-app/src/features/git/core.ts`, включить `SETFORK_CORE_URL`,
   **golden-сверить** байты/SHA с inproc.

Контракт — единственный источник правды: `proto/git.proto` (копия `sethub-app/proto/git.proto`,
держать синхронными; позже — общий proto-пакет/submodule).
