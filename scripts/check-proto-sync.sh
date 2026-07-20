#!/usr/bin/env bash
# Сверка proto-контрактов с копией во фронте: README требует держать их
# синхронными, но до сих пор никто не проверял (аудит 2026-07-20, P2-10).
# Кросс-репный гейт — гоняется локально (ci-local.sh); в GitHub CI чекаута
# фронта нет, там пропускается сам собой.
set -euo pipefail
cd "$(dirname "$0")/.."

FRONT="${SETFORK_FRONTEND_DIR:-../setfork-frontend}"
if [[ ! -d "$FRONT/proto" ]]; then
  echo "proto-sync: фронт не найден ($FRONT) — пропуск (задай SETFORK_FRONTEND_DIR)"
  exit 0
fi

fail=0
for f in proto/*.proto; do
  if ! diff -q "$f" "$FRONT/$f" >/dev/null 2>&1; then
    echo "proto-sync: РАЗЪЕЗД $f ↔ $FRONT/$f"
    diff -u "$FRONT/$f" "$f" || true
    fail=1
  fi
done
if [[ $fail -eq 0 ]]; then
  echo "proto-sync: OK (идентичны с $FRONT)"
fi
exit $fail
