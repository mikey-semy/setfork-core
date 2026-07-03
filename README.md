# setfork-core

Rust git-ядро SetFork (Gitaly-стиль): реализует `proto/git.proto` (`service GitCore`)
и обслуживает тяжёлые git-операции для Next-BFF. Часть Фазы 2 плана
`sethub-app/docs/rust-core-plan.md` + `sethub-app/docs/phase1-wire-contract.md`.

## Статус: СКЕЛЕТ (не собран в среде разработки — см. ниже)

- ✅ Cargo-проект: `tonic` (gRPC) + `prost` + `tokio`; `build.rs` кодогенит из `proto/git.proto`
  (protoc — из крейта `protoc-bin-vendored`, системный не нужен).
- ✅ `src/main.rs`: tonic-сервер + трейт `GitCore` со всеми 5 RPC (пока `unimplemented`).
- ⚠️ **НЕ скомпилировано в текущей среде: заблокирован `crates.io`** (npm-реестр доступен, cargo —
  нет; firewall-allowlist). Собери там, где у cargo есть сеть (твоя машина / CI с доступом к crates.io).

## Сборка / запуск (где cargo имеет сеть)

```sh
cargo build            # скачает tonic/tokio/prost, сгенерит стабы из proto
cargo run              # поднимет gRPC-сервер на 127.0.0.1:50051 (SETFORK_CORE_ADDR — переопределить)
```

Если crates.io недоступен и на твоей стороне — варианты: `cargo vendor` на машине с доступом →
коммит `vendor/` + `.cargo/config.toml [source.crates-io] replace-with="vendored-sources"`; либо
корпоративный зеркало-реестр в `~/.cargo/config.toml`.

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
