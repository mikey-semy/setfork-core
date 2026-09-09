# setfork-core

Rust git-ядро SetFork (Gitaly-стиль): обслуживает тяжёлые git-операции и доменные
read/write-порты для Next-BFF по gRPC. Контракты — `proto/git.proto` (GitCore) и
`proto/domain_read.proto` (ListRead/ListWrite/CurationRead/CurationWrite/CollabWrite);
это копии из `setfork-app`, держать синхронными (гейт — `scripts/check-proto-sync.sh`).

## Структура

```
src/
├─ main.rs             точка входа (bin): CLI-режимы, auth-интерсептор, wiring сервисов
├─ lib.rs              крейт-библиотека (модули видны tests/)
├─ config.rs           вся конфигурация из env, один раз на старте, с валидацией
├─ pb.rs, pb_domain.rs сгенерённые tonic-модули (setfork.git.v1 / setfork.domain.v1)
├─ blocks.rs           блочная модель: is_step/нормализация type/content (одно место)
├─ gate.rs             проверка права на запись: обратный вызов в приложение (ADR-0015)
├─ reason.rs           машинный код причины отказа в трейлере ответа (И1, AIP-193)
├─ throttle.rs         схлопывание фоновых пушей зеркала окном (throttle, не debounce)
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
│  ├─ version.rs       ЕДИНСТВЕННЫЙ путь записи версии + самолечение репо (Ф1)
│  ├─ write.rs         коммит list.json: сборка дерева и родителя
│  ├─ update.rs        движение main: правило состава дерева, не-fast-forward, гонка
│  ├─ canon.rs         канонический вид list.json (байты, по которым считается sha)
│  ├─ history.rs       чтение истории: ветки, теги, дифф версий
│  ├─ magic.rs         пространство имён веток `u/<id>/…` и refs/for/main
│  ├─ messages.rs      каталог сообщений хука, двуязычный (И2)
│  ├─ mirror.rs        пуш зеркала в GitHub/GitLab (Ф3), шифрование токена
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

Тесты, которым нужен Postgres, помечены `#[ignore]` — обычный `cargo test` их
**молча пропускает**. Поднять базу отдельно от прогона:

```sh
eval "$(bash scripts/itest-env.sh)"        # поднимает и задаёт TEST_DATABASE_URL
cargo test -- --include-ignored            # весь набор, включая БД
cargo test --test projection -- --include-ignored   # один набор, секунды
bash scripts/itest-env.sh --down           # убрать
```

Контейнер именованный и переживает прогон: следующий запуск не ждёт подъёма заново.
Порт — `CORE_PG_PORT` (дефолт 55439); для параллельных сессий смещать, как у фронта.
Пробы (`--features probes`) без флага не собираются вовсе.

Обязательное окружение сервера — **четыре** переменных, и ядро останавливается на старте,
если любой нет (fail-fast: молча деградировать до открытой двери нельзя):

| переменная | без неё |
|---|---|
| `DATABASE_URL` | не к чему подключаться |
| `GIT_DATA_DIR` | нет тома bare-репо (общий с фронтом) |
| `SETFORK_CORE_TOKEN` | канал без авторизации — полный обход владения и модерации |
| `SETFORK_APP_URL` | некого спросить «можно ли писать» (ADR-0015): заморозка и архив перестают действовать на git-путях |

Последние две можно снять только явным опт-аутом `SETFORK_ALLOW_INSECURE=1` — это локальный
dev, не режим прода.

Полный перечень остального окружения с объяснением «зачем» — в `.env.example`; ручки границ
приёма (`SETFORK_MAX_RECV_MB`, `SETFORK_MAX_PACK_MB`, `SETFORK_REPO_LIMIT_MB`), окно зеркала,
размер lock-пула и rate-limit (`SETFORK_RPC_RPM`, `SETFORK_RPC_RPM_HEAVY`, 0 = выключить)
живут там.

⚠️ Дефолты в образе ДРУГИЕ, чем при `cargo run`: `Dockerfile` ставит `SETFORK_CORE_ADDR=0.0.0.0:50051`
и `SETFORK_METRICS_ADDR=0.0.0.0:9464` — иначе снаружи контейнера не достучаться.

## Наблюдаемость

- **Логи** — `tracing`: уровень через `RUST_LOG` (дефолт `info`), `SETFORK_LOG_JSON=1` —
  JSON-строки. Каждый RPC с ошибкой логируется (метод, gRPC-код, латентность, msg);
  успешные — на `debug`. Ошибки проекции — `ERROR` с owner/slug и подсказкой `reproject`.
- **Метрики** — Prometheus на `SETFORK_METRICS_ADDR` (дефолт `127.0.0.1:9464`, `/metrics`;
  `0`/`off` — выключить): `rpc_requests_total{method,code}`, `rpc_duration_seconds{method}`,
  `rpc_rate_limited_total{method}`, `projection_failures_total{op}`, `db_healthy`,
  `db_pool_size`, `db_pool_idle`, `write_gate_denied_total{reason}` (отказы гейта записи),
  `repo_bytes`, `repo_catchup_versions_total`, `version_tag_conflicts_total`,
  `mirror_push_total{result}`, `mirror_push_duration_seconds{result}`.
- **Health** (grpc.health.v1) — привязан к БД: фоновая проба `SELECT 1` каждые 5с,
  при недоступности БД сервис уходит в NOT_SERVING (оркестратор уводит трафик) и
  возвращается в SERVING после восстановления.
- **Reflection** (v1 + v1alpha) — `grpcurl -plaintext host:50051 list` работает без
  локальных proto-файлов.

## Режимы CLI

Бинарь ядра — не только сервер: первый аргумент выбирает режим, и без аргументов
запускается gRPC-сервер. ⚠️ Неизвестный режим (опечатка в имени команды) тоже запускает
сервер, а не сообщает об ошибке.

### Golden-сверка с TS

Гоняют тот же код-пас, что и RPC, без транспорта — байты/JSON сверяются со скриптами
фронта:

```sh
cargo run -- bundle            <owner> <slug> <out>
cargo run -- advertise-upload  <owner> <slug> <out>
cargo run -- advertise-receive <owner> <slug> <out>
cargo run -- upload-pack       <owner> <slug> <body> <out>
cargo run -- domain-read       <owner> <slug> <out.json>
```

### Операторские режимы

```sh
cargo run -- reproject  <owner> <slug>   # проекция одного списка из текущего main-tip
cargo run -- sync-repos                  # выровнять ВСЕ репо с БД (идемпотентно)
cargo run -- gc-repos                    # показать бесхозные репо; --apply чтобы удалить
```

Порядок разбора и все исходы `sync-repos` — в рунбуке
[git-projection-catchup](https://github.com/mikey-semy/setfork-hq/blob/master/runbooks/git-projection-catchup.md).

## Восстановление после сбоя проекции

Push/merge принимаются, даже если проекция версии в БД упала (git-объекты целы) —
такая ошибка пишется в лог с пометкой «ОШИБКА проекции». Восстановление вручную
(создаёт новую версию из текущего main-tip; требует `GIT_DATA_DIR`):

```sh
cargo run -- reproject <owner> <slug>
```

---

## For contributors (English)

`setfork-core` is the Rust git service behind [SetFork](https://setfork.com): it serves
heavy git operations and the domain read/write ports over gRPC. Contracts are in
`proto/`; the application repository keeps byte-identical copies of them, and a gate
checks that they have not drifted apart.

Licensed under **AGPL-3.0-only** — see [LICENSE](LICENSE). Section 13 applies to network
use: if you run a modified version as a service, its users must be able to obtain the
source.

```sh
cargo build
eval "$(bash scripts/itest-env.sh)"     # brings up Postgres, sets TEST_DATABASE_URL
cargo test -- --include-ignored        # database tests are #[ignore] by default
bash scripts/itest-env.sh --down       # tear it down
```

⚠️ A plain `cargo test` **passes while skipping every test that needs a database**. CI
runs them; run them the same way before you push.

- [CONTRIBUTING.md](CONTRIBUTING.md) — how to propose a change, the AI policy, DCO, and
  the CLA that will be required for the first external pull request
- [SECURITY.md](SECURITY.md) — how to report a vulnerability (not as a public issue)
- [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)

The project is maintained by one person and pull requests are reviewed about once a
week. That is a promise of an answer, not of speed.
