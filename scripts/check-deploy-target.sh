#!/usr/bin/env bash
# Сторож: ядро и фронт должны выкатываться на ОДИН хост.
#
# Зачем. 04.08 канон вернулся на setfork.com (ADR-0017). У фронта цель выкатки
# поправили 09.08, у ядра — забыли, и оно пять суток каталось на списанный
# сервер. Опасно не отставание само по себе: запись версий списков идёт ТОЛЬКО
# через ядро, а protobuf молча игнорирует незнакомые поля — новое поле шага
# доехало бы до старого ядра и пропало без единой ошибки. Нашла это не проверка,
# а линза, случайно (R1 реестра 2026-08-11). Проверка обязана быть машинной:
# «оба репозитория смотрят на один адрес» — ровно то утверждение, которое было
# неверным пять суток и никого не разбудило.
#
# Где искать адрес — НЕ фиксируем: у фронта он жил в `deploy.yml`, а после
# перевода деплоя в джобу CI переезжает в `ci.yml`. Сторож, знающий один путь,
# сломался бы при первом же переезде и молча стал бы «зелёным». Поэтому ищем
# по всем воркфлоу обоих репозиториев.
#
# Клон фронта делает CI тем же токеном, что и для proto-sync (FRONTEND_RO_TOKEN).
# Без клона проверка НЕ выполняется и говорит об этом вслух — молчаливый пропуск
# был бы повторением той же ошибки: до 12.08 гейт proto-sync годами выглядел
# зелёным, не выполняясь ни разу.
set -euo pipefail
cd "$(dirname "$0")/.."

FRONT="${SETFORK_FRONTEND_DIR:-../setfork-frontend}"
REF="${SETFORK_FRONTEND_REF:-origin/master}"

# Адрес выкатки = значение `DEPLOY:` в любом воркфлоу. Берём уникальные.
targets_here() {
  grep -rhoE '^[[:space:]]*DEPLOY:[[:space:]]*[^[:space:]]+' .github/workflows/*.yml 2>/dev/null \
    | sed -E 's/^[[:space:]]*DEPLOY:[[:space:]]*//' | sort -u
}

targets_there() {
  local files
  files=$(git -C "$FRONT" ls-tree --name-only "$REF" .github/workflows/ 2>/dev/null || true)
  [ -z "$files" ] && return 1
  local f
  for f in $files; do
    git -C "$FRONT" show "$REF:$f" 2>/dev/null \
      | grep -oE '^[[:space:]]*DEPLOY:[[:space:]]*[^[:space:]]+' | sed -E 's/^[[:space:]]*DEPLOY:[[:space:]]*//' || true
  done | sort -u
}

OURS=$(targets_here)
if [ -z "$OURS" ]; then
  echo "::error::в воркфлоу ядра нет ни одного DEPLOY: — сторож не нашёл, что сверять"
  exit 1
fi

if [[ ! -d "$FRONT/.git" && ! -f "$FRONT/.git" ]]; then
  echo "::warning::deploy-target НЕ ВЫПОЛНЕН: клона фронта нет ($FRONT)."
  echo "::warning::Локально задай SETFORK_FRONTEND_DIR; в CI нужен секрет FRONTEND_RO_TOKEN."
  echo "deploy-target: НЕ ВЫПОЛНЕН (нет клона фронта) — это не «ок», это отсутствие проверки"
  exit 0
fi

THEIRS=$(targets_there) || {
  echo "::warning::deploy-target НЕ ВЫПОЛНЕН: не читаются воркфлоу фронта из $REF"
  exit 0
}

if [ -z "$THEIRS" ]; then
  echo "::error::в воркфлоу фронта ($REF) нет ни одного DEPLOY: — либо переименовали переменную, либо сторож смотрит не туда"
  exit 1
fi

if [ "$OURS" != "$THEIRS" ]; then
  echo "::error::цели выкатки разъехались — ядро и фронт поедут на разные серверы"
  echo "  ядро: $(echo "$OURS" | tr '\n' ' ')"
  echo "  фронт ($REF): $(echo "$THEIRS" | tr '\n' ' ')"
  echo "Так уже было: 04.08 канон сменился, фронт починили 09.08, ядро — только 12.08."
  exit 1
fi

echo "deploy-target: OK (ядро и фронт → $(echo "$OURS" | tr '\n' ' '))"
