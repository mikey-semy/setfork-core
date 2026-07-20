#!/usr/bin/env bash
# Локальный CI-гейт ядра — ОСНОВНАЯ проверка перед мержем (решение владельца
# 2026-07-14: CI на локали, GitHub-минуты не тратим; workflows в .github —
# дублирующий барьер, когда Actions доступен).
#
#   bash scripts/ci-local.sh          # полный прогон (поднимет эфемерный Postgres в docker)
#   bash scripts/ci-local.sh --fast   # без интеграционных тестов с БД
#
# Требуется: rust toolchain, git; для полного прогона — docker.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== cargo fmt --check =="
cargo fmt --check

echo "== proto-sync с фронтом =="
bash scripts/check-proto-sync.sh

echo "== cargo clippy (--all-targets, -D warnings) =="
cargo clippy --all-targets -- -D warnings

echo "== cargo test (юнит + roundtrip с настоящим git) =="
cargo test

if [[ "${1:-}" == "--fast" ]]; then
  echo "ci-local: OK (--fast, без интеграционных)"
  exit 0
fi

echo "== интеграционные + golden (эфемерный Postgres) =="
PORT="${CI_PG_PORT:-55439}"
CID=$(docker run -d --rm \
  -e POSTGRES_USER=t -e POSTGRES_PASSWORD=t -e POSTGRES_DB=t \
  -p "${PORT}:5432" pgvector/pgvector:pg16)
trap 'docker stop "$CID" >/dev/null 2>&1 || true' EXIT
for _ in $(seq 1 60); do
  docker exec "$CID" pg_isready -U t -d t >/dev/null 2>&1 && break
  sleep 1
done
# Порог покрытия (Фаза 2 плана): при установленном cargo-llvm-cov интеграционные
# гоняются С инструментацией и fail-under; замер 2026-07-20 после проекционного
# теста — 61.4% строк, порог чуть ниже (растить, не опускать). Слепые зоны:
# services/git_core.rs (gRPC-обвязка GitCore), telemetry.rs, main.rs — tracks/core.md.
# Без cargo-llvm-cov — обычный прогон + подсказка (гейт не роняем на dev-машинах).
if command -v cargo-llvm-cov >/dev/null 2>&1; then
  echo "== интеграционные + golden + coverage-порог (cargo-llvm-cov) =="
  TEST_DATABASE_URL="postgresql://t:t@localhost:${PORT}/t" \
    cargo llvm-cov --summary-only --fail-under-lines 58 -- --include-ignored
else
  echo "== интеграционные + golden (cargo-llvm-cov не установлен — без порога покрытия) =="
  echo "   установка: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview"
  TEST_DATABASE_URL="postgresql://t:t@localhost:${PORT}/t" cargo test -- --include-ignored
fi

echo "ci-local: OK"
