#!/usr/bin/env bash
# Сверка proto-контрактов с копией во фронте: README требует держать их
# синхронными (аудит 2026-07-20, P2-10). Кросс-репный гейт: гоняется и локально
# (ci-local.sh), и в GitHub-CI — с 12.08 там есть клон фронта по секрету
# FRONTEND_RO_TOKEN (шаг «Свежий клон фронта» в ci.yml). До этого чекаута не было,
# и шапка ещё год утверждала бы, что гейт «пропускается сам собой».
#
# Сравниваем с ЗАКОММИЧЕННЫМ ref'ом фронта (дефолт origin/master), а не с его
# рабочей копией: рабочая копия бывает несвежей/грязной и давала ложные разъезды
# (поймано 2026-07-20). Переопределения:
#   SETFORK_FRONTEND_DIR — путь к клону фронта (дефолт ../setfork-app)
#   SETFORK_FRONTEND_REF — ref для сверки (дефолт origin/master; для парных
#                          веток укажи ветку фронта)
set -euo pipefail
cd "$(dirname "$0")/.."

FRONT="${SETFORK_FRONTEND_DIR:-../setfork-app}"
REF="${SETFORK_FRONTEND_REF:-origin/master}"
if [[ ! -d "$FRONT/.git" && ! -f "$FRONT/.git" ]]; then
  # ГРОМКО, а не молча. Раньше здесь стоял тихий «пропуск», и в GitHub-CI гейт не
  # работал вообще — а выглядел выполненным. За это время в master уехал разъезд
  # block_id (реестр проверки 28.07, п.7): контракты разошлись, и никто не увидел.
  #
  # Кросс-репный чекаут требует токена с доступом на чтение обоих приватных репо;
  # пока его нет, в GitHub-CI гейт остаётся невыполнимым — и говорит об этом вслух.
  echo "::warning::proto-sync НЕ ВЫПОЛНЕН: клона фронта нет ($FRONT). Локально задай"
  echo "::warning::SETFORK_FRONTEND_DIR; в GitHub-CI нужен токен на чтение setfork-app."
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
  # Сюда мы попадаем, только если клон фронта ЕСТЬ (его отсутствие обработано
  # выше и там же громко объявлено). Значит нечитаемый файл — это не «нет
  # окружения», а переименование или удаление, то есть настоящий разъезд
  # контракта. Молчать нельзя: именно за тихий пропуск этот скрипт уже платил —
  # гейт выглядел выполненным, а в master уехало расхождение block_id.
  if ! front_src=$(git -C "$FRONT" show "$REF:$theirs" 2>/dev/null); then
    echo "proto-sync: $REF:$theirs НЕ ЧИТАЕТСЯ — переименован или удалён?"
    echo "proto-sync: сверка причин невозможна; поправь путь в check_reasons или верни файл"
    fail=1
    return 0
  fi
  local ours_list front_list
  # Значения объявлены внутри `reasons! { … }` в форме `Variant => "WIRE",`.
  # Берём только этот блок: то же написание встречается в комментариях и тестах.
  ours_list=$(sed -n '/^reasons! {/,/^}/p' src/reason.rs | grep -oE '=> "[A-Z0-9_]+"' | grep -oE '[A-Z0-9_]+' | sort -u)
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

# Общая часть для зеркал: сравнить два множества и назвать, чего где нет.
# Заведено линзой 01 §3: гейт сверял ЧЕТЫРЕ вещи, а копий контракта оказалось
# больше, и каждая несверяемая копия расходится молча.
compare_sets() {
  local what="$1" ours="$2" theirs="$3" ours_hint="$4" theirs_hint="$5"
  if [[ -z "$ours" ]]; then
    echo "proto-sync: не удалось извлечь $what из $ours_hint — проверка сломана, чини её"
    fail=1
    return 0
  fi
  local only_core only_front
  only_core=$(comm -23 <(printf '%s\n' "$ours") <(printf '%s\n' "$theirs"))
  only_front=$(comm -13 <(printf '%s\n' "$ours") <(printf '%s\n' "$theirs"))
  if [[ -n "$only_core" ]]; then
    echo "proto-sync: $what есть в ядре, но не у фронта ($theirs_hint):"
    printf '  %s\n' $only_core
    fail=1
  fi
  if [[ -n "$only_front" ]]; then
    echo "proto-sync: $what есть у фронта, но не в ядре ($ours_hint):"
    printf '  %s\n' $only_front
    fail=1
  fi
}

# ВИДЫ СПИСКА. Комментарий в `serialize.rs` прямо называет фронтовый файл
# зеркалом, но сверки не было. Цена разъезда: проекция САНИТИЗИРУЕТ вид по этому
# списку, поэтому вид, заведённый на фронте и незнакомый ядру, молча отбрасывается —
# автор увидит успех и потерянный тип (01-F1).
check_list_kinds() {
  local theirs='src/shared/ai/list-kind.ts' front_src
  if ! front_src=$(git -C "$FRONT" show "$REF:$theirs" 2>/dev/null); then
    echo "proto-sync: $REF:$theirs НЕ ЧИТАЕТСЯ — переименован или удалён?"
    fail=1
    return 0
  fi
  local ours_list front_list
  # Берём ЛЮБОЕ значение в кавычках, а не только латиницу: выборка, молча
  # пропускающая непонятное, и есть тот самый гейт, который «зелен, потому что
  # ничего не увидел» (проверено мутацией — кириллическое значение так и прошло).
  ours_list=$(sed -n '/pub const LIST_KINDS/,/;/p' src/git/serialize.rs | grep -oE '"[^"]+"' | tr -d '"' | sort -u)
  front_list=$(printf '%s\n' "$front_src" | sed -n '/export const LIST_KINDS/,/]/p' | grep -oE "'[^']+'" | tr -d "'" | sort -u)
  compare_sets "виды списка" "$ours_list" "$front_list" "src/git/serialize.rs" "src/shared/ai/list-kind.ts"
}
check_list_kinds

# КОДЫ ПРИДИРОК КАНОНА. Разъезд не молчалив (неизвестный код падает на английский
# текст ядра), но человек читает объяснение не на своём языке — 01-F2.
check_issue_codes() {
  local theirs='src/features/library/list-editor/CanonPanel.tsx' front_src
  if ! front_src=$(git -C "$FRONT" show "$REF:$theirs" 2>/dev/null); then
    echo "proto-sync: $REF:$theirs НЕ ЧИТАЕТСЯ — переименован или удалён?"
    fail=1
    return 0
  fi
  local ours_list front_list
  ours_list=$(sed -n '/fn as_str(self)/,/^    }/p' src/git/canon.rs | grep -oE '=> "[^"]+"' | sed 's/=> //; s/"//g' | sort -u)
  front_list=$(printf '%s\n' "$front_src" | sed -n '/const known: Record<string, string>/,/^  }/p' | grep -oE '^    [A-Za-z_][A-Za-z0-9_]*:' | tr -d ' :' | sort -u)
  compare_sets "коды придирок канона" "$ours_list" "$front_list" "src/git/canon.rs" "$theirs"
}
check_issue_codes

# ФОРМА ИМЕНИ СЕРВЕРНОЙ ВЕТКИ. Ядро её СОЗДАЁТ (`magic.rs`, author_branch), фронт
# УЗНАЁТ регуляркой (`branch-label.ts`, SERVER_BRANCH) — одно правило, две реализации,
# и сверить их было нечем (09-F1). Форма уже менялась однажды: 03.08 ник заменили
# идентификатором, и обе стороны правили руками в один день.
#
# Цена разъезда тихая: серверная ветка перестаёт узнаваться, и в селекторе веток
# вместо подписи «правка из терминала» появляется сырой `u/<uuid>/main`.
#
# Сверяем не текст правил (они на разных языках), а ПОВЕДЕНИЕ на образцах: имя,
# собранное правилом ядра, фронт обязан узнать, а заведомо чужое — не узнать.
check_branch_shape() {
  local theirs='src/features/git/branch-label.ts' front_src
  if ! front_src=$(git -C "$FRONT" show "$REF:$theirs" 2>/dev/null); then
    echo "proto-sync: $REF:$theirs НЕ ЧИТАЕТСЯ — переименован или удалён?"
    fail=1
    return 0
  fi
  # Шаблон ядра: format!("u/{actor_id}/{base}") → образец с настоящим uuid и base=main.
  # Ищем по СМЫСЛУ (единственный format! с {actor_id}), а не по порядку строк:
  # format!-ов в файле пять, и «первый попавшийся» молча уехал бы на чужой.
  local core_fmt sample alien
  core_fmt=$(grep -oE 'format!\("[^"]*\{actor_id\}[^"]*"\)' src/git/magic.rs | head -1 | sed 's/format!("//; s/")//')
  if [[ -z "$core_fmt" ]]; then
    echo "proto-sync: не удалось извлечь шаблон имени ветки из src/git/magic.rs — проверка сломана, чини её"
    fail=1
    return 0
  fi
  sample=${core_fmt/\{actor_id\}/11111111-2222-3333-4444-555555555555}
  sample=${sample/\{base\}/main}
  if [[ "$sample" == *"{"* ]]; then
    echo "proto-sync: в шаблоне имени ветки появилась незнакомая подстановка ($core_fmt) —"
    echo "  образец собрать нечем, проверка сломана. Научи её новой подстановке."
    fail=1
    return 0
  fi
  alien="feature/my-branch"

  # Регулярка фронта → форма для grep: берём тело между `/^` и `$/i`; в JS слэш
  # экранирован (`\\/`), в ERE это лишнее.
  local rx
  rx=$(printf '%s\n' "$front_src" | sed -n 's|.*SERVER_BRANCH *= *//*\^\(.*\)[$]/i.*|\1|p' | head -1)
  if [[ -z "$rx" ]]; then
    echo "proto-sync: не удалось извлечь SERVER_BRANCH из $theirs — проверка сломана, чини её"
    fail=1
    return 0
  fi
  rx=${rx//\\\//\/}

  if ! printf '%s\n' "$sample" | grep -Eiq "^${rx}$"; then
    echo "proto-sync: имя ветки, которое СОЗДАЁТ ядро ($sample), фронт НЕ УЗНАЁТ:"
    echo "  ядро:  src/git/magic.rs → $core_fmt"
    echo "  фронт: $theirs → $rx"
    echo "  цена: в селекторе веток вместо подписи появится сырой идентификатор"
    fail=1
  fi
  if printf '%s\n' "$alien" | grep -Eiq "^${rx}$"; then
    echo "proto-sync: правило фронта узнаёт ЧУЖУЮ ветку ($alien) как серверную — оно слишком широкое"
    fail=1
  fi
}
check_branch_shape

if [[ $fail -eq 0 ]]; then
  echo "proto-sync: OK (идентичны с $FRONT@$REF)"
fi
exit $fail
