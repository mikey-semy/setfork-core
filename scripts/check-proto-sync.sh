#!/usr/bin/env bash
# Сверка proto-контрактов с копией во фронте: README требует держать их
# синхронными (аудит 2026-07-20, P2-10). Кросс-репный гейт — гоняется локально
# (ci-local.sh); в GitHub CI чекаута фронта нет, там пропускается сам собой.
#
# Сравниваем с ЗАКОММИЧЕННЫМ ref'ом фронта (дефолт origin/master), а не с его
# рабочей копией: рабочая копия бывает несвежей/грязной и давала ложные разъезды
# (поймано 2026-07-20). Переопределения:
#   SETFORK_FRONTEND_DIR — путь к клону фронта (дефолт ../setfork-frontend)
#   SETFORK_FRONTEND_REF — ref для сверки (дефолт origin/master; для парных
#                          веток укажи ветку фронта)
set -euo pipefail
cd "$(dirname "$0")/.."

FRONT="${SETFORK_FRONTEND_DIR:-../setfork-frontend}"
REF="${SETFORK_FRONTEND_REF:-origin/master}"
if [[ ! -d "$FRONT/.git" && ! -f "$FRONT/.git" ]]; then
  echo "proto-sync: фронт не найден ($FRONT) — пропуск (задай SETFORK_FRONTEND_DIR)"
  exit 0
fi

fail=0
for f in proto/*.proto; do
  if ! expected=$(git -C "$FRONT" show "$REF:$f" 2>/dev/null); then
    echo "proto-sync: $REF:$f не читается из $FRONT — пропуск файла"
    continue
  fi
  # tr -d '\r': локальный чекаут может быть CRLF (Windows autocrlf), блоб — LF.
  if ! diff -u <(printf '%s\n' "$expected" | tr -d '\r') <(tr -d '\r' <"$f") >/tmp/proto-sync-diff.$$ 2>&1; then
    echo "proto-sync: РАЗЪЕЗД $f ↔ $FRONT@$REF"
    cat /tmp/proto-sync-diff.$$
    fail=1
  fi
  rm -f /tmp/proto-sync-diff.$$
done
if [[ $fail -eq 0 ]]; then
  echo "proto-sync: OK (идентичны с $FRONT@$REF)"
fi
exit $fail
