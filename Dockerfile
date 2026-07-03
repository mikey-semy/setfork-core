# setfork-core (Rust git-ядро). Multi-stage: cargo build --release → тонкий runtime.
# protoc не нужен в системе — build.rs берёт его из крейта protoc-bin-vendored.

FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim AS runtime
# git — материализация/receive-pack через шелл; ca-certificates; netcat — для HEALTHCHECK.
# Непривилегированный пользователь + /data (владелец = setfork): именованный том git-объектов
# при первом создании наследует владельца точки монтирования, поэтому non-root может писать.
RUN apt-get update \
  && apt-get install -y --no-install-recommends git ca-certificates netcat-openbsd \
  && rm -rf /var/lib/apt/lists/* \
  && useradd -r -u 10001 -m -d /home/setfork setfork \
  && mkdir -p /data/git && chown -R setfork:setfork /data
WORKDIR /app
COPY --from=builder /app/target/release/setfork-core /usr/local/bin/setfork-core
# слушать снаружи контейнера; том git-объектов по умолчанию
ENV SETFORK_CORE_ADDR=0.0.0.0:50051 GIT_DATA_DIR=/data/git
USER setfork
EXPOSE 50051
HEALTHCHECK --interval=30s --timeout=4s --start-period=10s --retries=3 \
  CMD nc -z 127.0.0.1 50051 || exit 1
CMD ["setfork-core"]
