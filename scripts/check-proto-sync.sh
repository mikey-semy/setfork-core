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
  # ГРОМКО, а не молча. Раньше здесь стоял тихий «пропуск», и в GitHub-CI гейт не
  # работал вообще — а выглядел выполненным. За это время в master уехал разъезд
  # block_id (реестр проверки 28.07, п.7): контракты разошлись, и никто не увидел.
  #
  # Кросс-репный чекаут требует токена с доступом на чтение обоих приватных репо;
  # пока его нет, в GitHub-CI гейт остаётся невыполнимым — и говорит об этом вслух.
  echo "::warning::proto-sync НЕ ВЫПОЛНЕН: клона фронта нет ($FRONT). Локально задай"
  echo "::warning::SETFORK_FRONTEND_DIR; в GitHub-CI нужен токен на чтение setfork-frontend."
  echo "proto-sync: НЕ ВЫПОЛНЕН (нет клона фронта) — это не «ок», это отсутствие проверки"
  exit 0
fi

fail=0
# Кросс-репные артефакты-копии: proto-контракты + JSON Schema манифеста (Ф2a:
# схема живёт в ядре — владельце формата, фронт раздаёт копию из public/).
sync_file() {
  local ours="$1" theirs="$2"
  if ! expected=$(git -C "$FRONT" show "$REF:$theirs" 2>/dev/null); then
    echo "proto-sync: $REF:$theirs не читается из $FRONT — пропуск файла"
    return 0
  fi
  # tr -d '\r': локальный чекаут может быть CRLF (Windows autocrlf), блоб — LF.
  if ! diff -u <(printf '%s\n' "$expected" | tr -d '\r') <(tr -d '\r' <"$ours") >/tmp/proto-sync-diff.$$ 2>&1; then
    echo "proto-sync: РАЗЪЕЗД $ours ↔ $FRONT@$REF:$theirs"
    cat /tmp/proto-sync-diff.$$
    fail=1
  fi
  rm -f /tmp/proto-sync-diff.$$
}
for f in proto/*.proto; do
  sync_file "$f" "$f"
done
sync_file schema/list.v1.json public/schema/list.v1.json

# Причины отказа (И1): значения из src/reason.rs — контракт провода, их читает
# фронт. Байт-в-байт сверить нельзя (там Rust-перечисление, здесь TS-таблица),
# поэтому сверяем МНОЖЕСТВА значений. Ловим ровно ту поломку, ради которой
# затевалось: причина добавлена в ядре и забыта во фронте — человек снова видит
# общую ошибку вместо конкретной.
check_reasons() {
  local theirs='src/features/git/core.remote.ts'
  local front_src
  if ! front_src=$(git -C "$FRONT" show "$REF:$theirs" 2>/dev/null); then
    echo "proto-sync: $REF:$theirs не читается — сверка причин пропущена"
    return 0
  fi
  local ours_list front_list
  ours_list=$(grep -oE 'Reason::[A-Za-z]+ => "[A-Z0-9_]+"' src/reason.rs | grep -oE '"[A-Z0-9_]+"' | tr -d '"' | sort -u)
  front_list=$(printf '%s
' "$front_src" | sed -n '/REASON_TO_CODE/,/^}/p' | grep -oE '^  [A-Z0-9_]+:' | tr -d ' :' | sort -u)
  if [[ -z "$ours_list" ]]; then
    echo "proto-sync: причины не извлеклись из src/reason.rs — проверка сломана, чини её"
    fail=1
    return 0
  fi
  local only_core only_front
  only_core=$(comm -23 <(printf '%s
' "$ours_list") <(printf '%s
' "$front_list"))
  only_front=$(comm -13 <(printf '%s
' "$ours_list") <(printf '%s
' "$front_list"))
  if [[ -n "$only_core" ]]; then
    echo "proto-sync: причины есть в ядре, но НЕ разбираются фронтом (человек увидит общую ошибку):"
    printf '  %s
' $only_core
    fail=1
  fi
  if [[ -n "$only_front" ]]; then
    echo "proto-sync: фронт ждёт причины, которых ядро не шлёт (мёртвые ветки разбора):"
    printf '  %s
' $only_front
    fail=1
  fi
}
check_reasons

if [[ $fail -eq 0 ]]; then
  echo "proto-sync: OK (идентичны с $FRONT@$REF)"
fi
exit $fail
