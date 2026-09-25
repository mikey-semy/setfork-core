# setfork-core (Rust git-ядро). Multi-stage: cargo build --release → тонкий runtime.
# protoc не нужен вовсе — protobuf компилирует protox (чистый Rust) в build.rs.
# Базовые образы: trixie (bookworm тянул известные CVE) и запинены на digest
# (supply chain, воспроизводимость). Обновление пинов — осознанное:
#   docker buildx imagetools inspect rust:1-trixie / debian:trixie-slim /
#   ghcr.io/grpc-ecosystem/grpc-health-probe:<версия>

FROM rust:1-trixie@sha256:9a2cd304a852f05d3352f75bc2775242371c0169a72dbb40d5d881379d571989 AS builder
WORKDIR /app
# Слой зависимостей ОТДЕЛЬНО от кода: правка src/ не пересобирает весь стек.
# Фиктивные main/lib прогревают deps; touch ниже — чтобы cargo не принял
# артефакты фиктивной сборки за свежие (mtime COPY может быть старше).
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
# Опубликованная схема списка нужна САМОЙ СБОРКЕ: `src/git/canon.rs` вшивает её
# через include_str!. Локально файл на месте, поэтому расхождение видно только в
# образе — с #86 сборка падала «couldn't read ../../schema/list.v1.json», деплой
# ядра уходил skipped, а CI фронта продолжал тестировать ghcr :latest от 05.08.
# Слой рядом с proto намеренно: схема меняется редко, кэш зависимостей остаётся цел.
COPY schema ./schema
RUN mkdir src \
  && echo 'fn main() {}' > src/main.rs \
  && touch src/lib.rs \
  && cargo build --release \
  && rm -rf src
COPY src ./src
RUN find src -type f -exec touch {} + && cargo build --release

# Настоящая gRPC-проба (grpc.health.v1): во время graceful-дренажа сервис отдаёт
# NOT_SERVING, и оркестратор уводит трафик (TCP-проба nc -z этого не увидела бы).
# Бинарь — из образа релиза проекта (так советует его README), запиненного на digest,
# как и базовые образы: digest держит целостность вместо sha256 файла. Раньше качали
# curl'ом с github.com — сборка падала, когда провайдер резал узлы github.com
# (25.09.2026), а ghcr.io на других адресах. Обновление — тем же imagetools inspect.
FROM ghcr.io/grpc-ecosystem/grpc-health-probe:v0.4.57@sha256:f77f1257805ecb57f1f0c36c5825d3c2e47aa81cce87128675e768463ed6487a AS grpc-health-probe

FROM debian:trixie-slim@sha256:020c0d20b9880058cbe785a9db107156c3c75c2ac944a6aa7ab59f2add76a7bd AS runtime
# git — материализация/receive-pack через шелл; ca-certificates для TLS.
# Непривилегированный пользователь + /data (владелец = setfork): именованный том git-объектов
# при первом создании наследует владельца точки монтирования, поэтому non-root может писать.
RUN apt-get update \
  && apt-get install -y --no-install-recommends git ca-certificates \
  && rm -rf /var/lib/apt/lists/* \
  && useradd -r -u 10001 -m -d /home/setfork setfork \
  && mkdir -p /data/git && chown -R setfork:setfork /data
WORKDIR /app
COPY --from=builder /app/target/release/setfork-core /usr/local/bin/setfork-core
COPY --from=grpc-health-probe /ko-app/grpc-health-probe /usr/local/bin/grpc_health_probe
# слушать снаружи контейнера; том git-объектов по умолчанию; метрики Prometheus
ENV SETFORK_CORE_ADDR=0.0.0.0:50051 GIT_DATA_DIR=/data/git SETFORK_METRICS_ADDR=0.0.0.0:9464
USER setfork
EXPOSE 50051 9464
HEALTHCHECK --interval=30s --timeout=4s --start-period=10s --retries=3 \
  CMD grpc_health_probe -addr=127.0.0.1:50051 || exit 1
CMD ["setfork-core"]
