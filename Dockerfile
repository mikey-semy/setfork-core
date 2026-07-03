# setfork-core (Rust git-ядро). Multi-stage: cargo build --release → тонкий runtime.
# protoc не нужен в системе — build.rs берёт его из крейта protoc-bin-vendored.

FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim AS runtime
# git — материализация/receive-pack идёт через шелл git; ca-certificates — на будущее.
RUN apt-get update \
  && apt-get install -y --no-install-recommends git ca-certificates \
  && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/setfork-core /usr/local/bin/setfork-core
# слушать снаружи контейнера; порт gRPC
ENV SETFORK_CORE_ADDR=0.0.0.0:50051
EXPOSE 50051
CMD ["setfork-core"]
