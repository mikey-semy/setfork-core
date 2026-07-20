# setfork-core (Rust git-ядро). Multi-stage: cargo build --release → тонкий runtime.
# protoc не нужен вовсе — protobuf компилирует protox (чистый Rust) в build.rs.
# Базовые образы: trixie (bookworm тянул известные CVE) и запинены на digest
# (supply chain, воспроизводимость). Обновление пинов — осознанное:
#   docker buildx imagetools inspect rust:1-trixie / debian:trixie-slim

FROM rust:1-trixie@sha256:9a2cd304a852f05d3352f75bc2775242371c0169a72dbb40d5d881379d571989 AS builder
WORKDIR /app
# Слой зависимостей ОТДЕЛЬНО от кода: правка src/ не пересобирает весь стек.
# Фиктивные main/lib прогревают deps; touch ниже — чтобы cargo не принял
# артефакты фиктивной сборки за свежие (mtime COPY может быть старше).
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
RUN mkdir src \
  && echo 'fn main() {}' > src/main.rs \
  && touch src/lib.rs \
  && cargo build --release \
  && rm -rf src
COPY src ./src
RUN find src -type f -exec touch {} + && cargo build --release

# grpc_health_probe качаем в builder (тут есть curl) — в runtime только COPY.
# Настоящая gRPC-проба (grpc.health.v1): во время graceful-дренажа сервис отдаёт
# NOT_SERVING, и оркестратор уводит трафик (TCP-проба nc -z этого не увидела бы).
# Бинарь проверяется по sha256 (supply chain). Цель деплоя — linux/amd64 (VPS).
ARG GRPC_HEALTH_PROBE_VERSION=v0.4.34
ARG GRPC_HEALTH_PROBE_SHA256=3ddaf85583613c97693e9b8aaa251dac07e73e366e159a7ccadbcf553117fcef
RUN curl -fsSL -o /grpc_health_probe \
      https://github.com/grpc-ecosystem/grpc-health-probe/releases/download/${GRPC_HEALTH_PROBE_VERSION}/grpc_health_probe-linux-amd64 \
  && echo "${GRPC_HEALTH_PROBE_SHA256}  /grpc_health_probe" | sha256sum -c - \
  && chmod +x /grpc_health_probe

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
COPY --from=builder /grpc_health_probe /usr/local/bin/grpc_health_probe
# слушать снаружи контейнера; том git-объектов по умолчанию; метрики Prometheus
ENV SETFORK_CORE_ADDR=0.0.0.0:50051 GIT_DATA_DIR=/data/git SETFORK_METRICS_ADDR=0.0.0.0:9464
USER setfork
EXPOSE 50051 9464
HEALTHCHECK --interval=30s --timeout=4s --start-period=10s --retries=3 \
  CMD grpc_health_probe -addr=127.0.0.1:50051 || exit 1
CMD ["setfork-core"]
