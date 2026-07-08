# setfork-core (Rust git-ядро). Multi-stage: cargo build --release → тонкий runtime.
# protoc не нужен вовсе — protobuf компилирует protox (чистый Rust) в build.rs.

FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
COPY src ./src
RUN cargo build --release

# grpc_health_probe качаем в builder (тут есть curl) — в runtime только COPY.
# Настоящая gRPC-проба (grpc.health.v1): во время graceful-дренажа сервис отдаёт
# NOT_SERVING, и оркестратор уводит трафик (TCP-проба nc -z этого не увидела бы).
# Цель деплоя — linux/amd64 (VPS); под другой arch поменяйте суффикс бинарника.
ARG GRPC_HEALTH_PROBE_VERSION=v0.4.34
RUN curl -fsSL -o /grpc_health_probe \
      https://github.com/grpc-ecosystem/grpc-health-probe/releases/download/${GRPC_HEALTH_PROBE_VERSION}/grpc_health_probe-linux-amd64 \
  && chmod +x /grpc_health_probe

FROM debian:bookworm-slim AS runtime
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
# слушать снаружи контейнера; том git-объектов по умолчанию
ENV SETFORK_CORE_ADDR=0.0.0.0:50051 GIT_DATA_DIR=/data/git
USER setfork
EXPOSE 50051
HEALTHCHECK --interval=30s --timeout=4s --start-period=10s --retries=3 \
  CMD grpc_health_probe -addr=127.0.0.1:50051 || exit 1
CMD ["setfork-core"]
