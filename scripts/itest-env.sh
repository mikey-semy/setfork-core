#!/usr/bin/env bash
# Postgres для интеграционных тестов ядра — ОТДЕЛЬНО от прогона (Задача 0 плана ревью).
#
#   bash scripts/itest-env.sh          # поднять (идемпотентно) и напечатать export
#   bash scripts/itest-env.sh --down   # убрать
#   eval "$(bash scripts/itest-env.sh)"   # поднять и сразу задать TEST_DATABASE_URL
#
# ЗАЧЕМ отдельно. Раньше единственный способ получить Postgres — полный `ci-local.sh`:
# fmt + clippy + весь набор + снос контейнера в конце. Прогнать один тест стоило
# минут вместо секунд, и это ровно та цена, которую фронт снял своим `itest:env`.
#
# Контейнер ИМЕНОВАННЫЙ и без `--rm`: он обязан пережить прогон, иначе следующий
# запуск снова ждёт подъёма. Убирается явно, флагом `--down`.
#
# Порт — из CORE_PG_PORT (дефолт 55439). Смещение на сессию: CORE_PG_PORT=5544<NN>,
# как у фронта. Схему поднимать не нужно: `tests/support/pool_with_schema()` создаёт
# свою случайную схему на каждый вызов, поэтому параллельные прогоны не мешают друг
# другу даже в одной базе.
set -euo pipefail

PORT="${CORE_PG_PORT:-55439}"
NAME="setfork-core-itest-${PORT}"
IMAGE="pgvector/pgvector:pg16"
URL="postgresql://t:t@127.0.0.1:${PORT}/t"

if [[ "${1:-}" == "--down" ]]; then
  # Спрашиваем о наличии ОТДЕЛЬНО: `docker rm -f` идемпотентен и возвращает 0 даже
  # для несуществующего контейнера, поэтому по его коду выхода «убрали» и «нечего
  # убирать» неразличимы — а человеку разница важна.
  if [[ -n "$(docker ps -aq -f "name=^${NAME}$" 2>/dev/null)" ]]; then
    docker rm -f "$NAME" >/dev/null
    echo "itest-env: $NAME убран"
  else
    echo "itest-env: $NAME и так нет"
  fi
  exit 0
fi

if [[ -n "$(docker ps -q -f "name=^${NAME}$" 2>/dev/null)" ]]; then
  echo "# itest-env: $NAME уже поднят" >&2
else
  docker rm -f "$NAME" >/dev/null 2>&1 || true   # остановленный тёзка после прошлого падения
  docker run -d --name "$NAME" \
    -e POSTGRES_USER=t -e POSTGRES_PASSWORD=t -e POSTGRES_DB=t \
    -p "${PORT}:5432" "$IMAGE" >/dev/null
  echo "# itest-env: поднимаю $NAME на порту $PORT" >&2
fi

# ⚠️ Имена переменных здесь латиницей: bash не принимает кириллицу в идентификаторах
# (в отличие от Rust, где она в проекте норма). Наступил на это дважды за день.
#
# ⚠️ Ожидание С ВЕРДИКТОМ. Прежние циклы в ci-local.sh и ci.yml просто заканчивались
# после 60 попыток и шли дальше — а дальше тесты падали с `PoolTimedOut`, и причина
# читалась как «сломались тесты», а не «база не поднялась». Молчаливое ожидание хуже
# отсутствия ожидания: оно ПОХОЖЕ на проверку.
ready=0
for _ in $(seq 1 60); do
  if docker exec "$NAME" pg_isready -U t -d t >/dev/null 2>&1; then ready=1; break; fi
  sleep 1
done
if [[ "$ready" -ne 1 ]]; then
  echo "itest-env: Postgres в $NAME НЕ ПОДНЯЛСЯ за 60 секунд. Дальше идти нельзя —" >&2
  echo "  тесты упадут с PoolTimedOut, и причина будет выглядеть как их поломка." >&2
  echo "  Последние строки лога контейнера:" >&2
  docker logs --tail 20 "$NAME" 2>&1 | sed 's/^/    /' >&2
  exit 1
fi

echo "export TEST_DATABASE_URL='${URL}'"
