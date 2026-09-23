//! Тексты, которые человек читает ПРЯМО из ядра — вывод `pre-receive`.
//!
//! # Почему это отдельный модуль, а не строки по месту
//!
//! Это единственная поверхность ядра, где переводить некому: `git push` показывает
//! stderr хука дословно, между ядром и человеком никого нет. Всё остальное —
//! `Status` — уходит машинными кодами (И1), и человеческий текст собирает фронт из
//! своего словаря.
//!
//! Строки живут одной таблицей, где оба языка видны рядом: пропуск заметен глазом,
//! а не всплывает у пользователя. Крейт локализации здесь бесполезен по построению —
//! `pre-receive` это ШЕЛЛ-СКРИПТ, он исполняется отдельным процессом уже после того,
//! как Rust отработал, и никакой Rust-рантайм не отформатирует его строку в момент
//! пуша. Поэтому оба набора кладутся В САМ скрипт, а выбор делает `$SETFORK_LANG`.
//!
//! # Почему оба набора в скрипте, а не генерация под язык
//!
//! `install_hook` зовётся при КАЖДОМ обращении к репозиторию. Если бы скрипт
//! генерировался под язык, содержимое хука зависело бы от того, кто трогал репо
//! последним, — и русский пуш переписывал бы хук для англичанина.
//!
//! # Язык по умолчанию — английский
//!
//! Решение владельца 2026-07-31: git-инструментарий говорит по-английски, и
//! незнакомый язык в выводе `git push` читается как поломка. Русский — только по
//! явному сигналу (профиль пользователя или `Accept-Language`).

/// Сообщение хука: ключ для шелла + оба перевода.
///
/// `%s` в тексте — подстановка, аргументы передаются в `msg` после ключа.
pub struct HookMessage {
    pub key: &'static str,
    pub en: &'static str,
    pub ru: &'static str,
}

/// Каталог. Добавляя строку в хук, добавляй её СЮДА, а не в тело скрипта.
pub const HOOK_MESSAGES: &[HookMessage] = &[
    HookMessage {
        key: "reserved_tag_name",
        en: "SetFork: tag %s is reserved for versions (v + digits) and is set by the server",
        ru: "SetFork: имя %s зарезервировано под версии (v и цифры), его ставит сервер",
    },
    HookMessage {
        key: "main_no_delete",
        en: "SetFork: the main branch is protected from deletion",
        ru: "SetFork: ветка main защищена от удаления",
    },
    HookMessage {
        key: "main_no_force",
        en: "SetFork: non-fast-forward push to main is forbidden (history rewrite)",
        ru: "SetFork: non-fast-forward push в main запрещён (перезапись истории)",
    },
    HookMessage {
        key: "list_json_required",
        en: "SetFork: list.json is required at the repo root",
        ru: "SetFork: в корне репозитория обязателен list.json",
    },
    HookMessage {
        key: "tree_allowlist",
        en: "SetFork: a list tree may hold only README.md, list.json, .gitattributes and scripts/, references/, assets/ (one level, text only).",
        ru: "SetFork: в дереве списка разрешены только README.md, list.json, .gitattributes и scripts/, references/, assets/ (один уровень, только текст).",
    },
    HookMessage {
        key: "tree_foreign_header",
        en: "Foreign paths (commit %s):",
        ru: "Лишние пути (коммит %s):",
    },
    HookMessage {
        key: "tree_foreign_hint",
        // Без апострофа намеренно: текст уезжает в одинарные кавычки шелла, и
        // `list's` порвал бы их — хук перестал бы разбираться целиком. Проверяет
        // тест `texts_are_safe_for_shell_and_printf`.
        en: "Remove them from the commit: list content lives in list.json.",
        ru: "Уберите их из коммита: содержимое списка живёт в list.json.",
    },
    // Авторские файлы скилла (scripts/, references/, assets/) — решение §4 исследования
    // Agent Skills: текст живёт в дереве, бинарь — в S3 по хешу.
    HookMessage {
        key: "authored_not_file",
        en: "SetFork: %s must be a regular file (no symlinks or submodules)",
        ru: "SetFork: %s обязан быть обычным файлом (без ссылок и подмодулей)",
    },
    HookMessage {
        key: "authored_binary",
        en: "SetFork: %s is binary; scripts/, references/ and assets/ hold text only (attach binaries as file blocks)",
        ru: "SetFork: %s — бинарный файл; в scripts/, references/ и assets/ только текст (бинарь — блоком «Файл»)",
    },
    HookMessage {
        key: "authored_too_many",
        en: "SetFork: scripts/, references/ and assets/ hold %s files; the limit is %s",
        ru: "SetFork: в scripts/, references/ и assets/ %s файлов; предел %s",
    },
    HookMessage {
        key: "authored_too_large",
        en: "SetFork: scripts/, references/ and assets/ hold %s bytes; the limit is %s",
        ru: "SetFork: scripts/, references/ и assets/ занимают %s байт; предел %s",
    },
    HookMessage {
        key: "magic_needs_actor",
        en: "SetFork: refs/for/... requires an authenticated push (no user in this request)",
        ru: "SetFork: refs/for/... требует авторизованного пуша (в запросе нет пользователя)",
    },
    HookMessage {
        key: "magic_bad_base",
        en: "SetFork: only refs/for/main is supported; got %s",
        ru: "SetFork: поддерживается только refs/for/main; получено %s",
    },
    HookMessage {
        key: "magic_accepted",
        en: "SetFork: change accepted, the suggestion will appear on the list page",
        ru: "SetFork: правка принята, предложение появится на странице списка",
    },
    // Ф5: посторонний пишет только в своё пространство. Текст обязан объяснить
    // ПРАВИЛО, а не просто отказать: человеку, которому нельзя в main, нужно
    // отсюда узнать, куда можно, — иначе он решит, что доступа нет вовсе.
    HookMessage {
        key: "contributor_namespace",
        en: "SetFork: you may not push to %s — this list is not yours",
        ru: "SetFork: в %s писать нельзя — список не ваш",
    },
    HookMessage {
        key: "contributor_namespace_hint",
        // Без апострофов намеренно: текст уезжает в одинарные кавычки шелла.
        en: "Propose a change instead: git push origin HEAD:refs/for/main",
        ru: "Предложите правку: git push origin HEAD:refs/for/main",
    },
    HookMessage {
        key: "contributor_needs_actor",
        en: "SetFork: cannot tell who is pushing — re-authenticate with your API token",
        ru: "SetFork: непонятно, кто пушит — повторите вход со своим API-токеном",
    },
    // ── H15-002: вопрос о СОДЕРЖИМОМ ────────────────────────────────────────
    //
    // Пять строк отсюда печатает не шелл, а Rust — подкоманда `check-content`,
    // которую хук зовёт вместо HTTP (шелл его не умеет, curl в runtime-образе
    // нет). Лежат они всё равно ЗДЕСЬ: поверхность одна и та же — stderr `git
    // push`, и оба языка обязаны быть видны рядом. В шелл-функцию они попадут
    // тоже (генератор берёт таблицу целиком), и это безвредно: этих ключей шелл
    // не зовёт. Шестую, `content_check_missing`, печатает как раз шелл.
    HookMessage {
        key: "content_destructive",
        en: "SetFork: step %s contains a command that is not allowed here: %s (rule %s)",
        ru: "SetFork: в шаге %s команда, которую здесь выполнять нельзя: %s (правило %s)",
    },
    HookMessage {
        key: "content_destructive_file",
        en: "SetFork: %s contains a command that is not allowed here: %s (rule %s)",
        ru: "SetFork: в %s команда, которую здесь выполнять нельзя: %s (правило %s)",
    },
    HookMessage {
        key: "content_hint_file",
        en: "Remove or rewrite that command in the script and push again.",
        ru: "Уберите или перепишите эту команду в скрипте и повторите пуш.",
    },
    HookMessage {
        key: "content_hint",
        // Без апострофа намеренно: текст уезжает в одинарные кавычки шелла.
        en: "Remove or rewrite that command in list.json and push again.",
        ru: "Уберите или перепишите эту команду в list.json и повторите пуш.",
    },
    HookMessage {
        key: "content_denied",
        en: "SetFork: the app refused this content: %s",
        ru: "SetFork: приложение отклонило это содержимое: %s",
    },
    HookMessage {
        key: "content_check_unavailable",
        en: "SetFork: the content check did not answer (%s); nothing was written, try the push again",
        ru: "SetFork: проверка содержимого не ответила (%s); ничего не записано, повторите пуш",
    },
    HookMessage {
        key: "content_check_not_understood",
        en: "SetFork: the content check answered something unexpected (%s); the push is stopped",
        ru: "SetFork: проверка содержимого ответила непонятным (%s); пуш остановлен",
    },
    HookMessage {
        key: "content_check_missing",
        en: "SetFork: the content check could not be started, this push went unchecked",
        ru: "SetFork: проверку содержимого запустить не удалось, этот пуш не проверен",
    },
];

/// Тот же текст, но собранный В РАСТЕ: подкоманда `check-content` печатает
/// человеку прямо в stderr хука, шелл ей в этом не посредник.
///
/// Язык берётся из того же `SETFORK_LANG`: переменная доезжает до хука вместе с
/// окружением receive-pack, а подкоманда наследует её от хука.
///
/// Подстановка идёт РАЗБИЕНИЕМ по `%s`, а не поиском-заменой: аргумент здесь —
/// кусок пользовательской команды, и `%s` может оказаться в нём самом. Замена по
/// месту тогда подставила бы следующий аргумент внутрь предыдущего.
pub fn say(key: &str, args: &[&str]) -> String {
    let ru = std::env::var("SETFORK_LANG").map(|l| l.starts_with("ru")).unwrap_or(false);
    let text = match HOOK_MESSAGES.iter().find(|m| m.key == key) {
        Some(m) if ru => m.ru,
        Some(m) => m.en,
        // Тот же запасной вариант, что у шелла: пустая строка читалась бы как
        // «сломалось», а не как «нельзя».
        None => return format!("SetFork: {key}"),
    };
    let mut parts = text.split("%s");
    let mut out = String::from(parts.next().unwrap_or_default());
    for (i, tail) in parts.enumerate() {
        out.push_str(args.get(i).copied().unwrap_or("?"));
        out.push_str(tail);
    }
    out
}

/// Шелл-функция `msg <ключ> [аргументы…]`, печатающая строку на языке `$SETFORK_LANG`.
///
/// Формат берётся из таблицы и подставляется в `printf` — поэтому текст обязан быть
/// безопасным для `printf` и для одинарных кавычек шелла; это проверяет тест ниже.
/// Неизвестный язык и пустое значение дают английский (решение о дефолте — в шапке).
pub fn shell_msg_fn() -> String {
    let mut out =
        String::from("lang=\"${SETFORK_LANG:-en}\"\nmsg() {\n  k=\"$1\"; shift\n  case \"$lang\" in\n");
    for (lang_pat, pick) in [("ru*", true), ("*", false)] {
        out.push_str(&format!("  {lang_pat})\n    case \"$k\" in\n"));
        for m in HOOK_MESSAGES {
            let text = if pick { m.ru } else { m.en };
            out.push_str(&format!("      {}) f='{}' ;;\n", m.key, text));
        }
        // Неизвестный ключ не должен молча превращаться в пустую строку: пустой
        // отказ читается как «сломалось», а не как «нельзя».
        out.push_str("      *) f=\"SetFork: $k\" ;;\n    esac ;;\n");
    }
    out.push_str("  esac\n  # shellcheck disable=SC2059 -- формат наш, из таблицы сообщений\n  printf \"$f\\n\" \"$@\"\n}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_complete_and_free_of_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for m in HOOK_MESSAGES {
            assert!(!m.en.trim().is_empty(), "{}: пустой en", m.key);
            assert!(!m.ru.trim().is_empty(), "{}: пустой ru", m.key);
            assert!(seen.insert(m.key), "{}: ключ продублирован", m.key);
        }
    }

    /// Текст уезжает в одинарные кавычки шелла и в `printf`. Апостроф порвал бы
    /// кавычку, а лишний `%` — формат: и то и другое сломало бы ХУК, то есть
    /// приём пушей целиком. Дешевле запретить, чем экранировать.
    #[test]
    fn texts_are_safe_for_shell_and_printf() {
        for m in HOOK_MESSAGES {
            for (lang, text) in [("en", m.en), ("ru", m.ru)] {
                assert!(!text.contains('\''), "{} [{lang}]: апостроф порвёт кавычку шелла", m.key);
                assert!(!text.contains('\n'), "{} [{lang}]: перевод строки", m.key);
                // Единственная разрешённая подстановка — %s.
                assert_eq!(
                    text.matches('%').count(),
                    text.matches("%s").count(),
                    "{} [{lang}]: '%' вне %s сломает printf",
                    m.key
                );
            }
            // Число подстановок обязано совпадать: иначе на одном языке аргумент
            // потеряется, а на другом появится мусор.
            assert_eq!(
                m.en.matches("%s").count(),
                m.ru.matches("%s").count(),
                "{}: разное число подстановок в переводах",
                m.key
            );
        }
    }

    /// Подстановка в Rust-ветке. Главное здесь — второй случай: кусок команды
    /// приезжает от пользователя, и `%s` внутри него не имеет права втянуть
    /// следующий аргумент.
    #[test]
    fn say_substitutes_each_argument_once() {
        assert_eq!(
            say("content_destructive", &["3", "rm -rf /", "rm_rf"]),
            "SetFork: step 3 contains a command that is not allowed here: rm -rf / (rule rm_rf)"
        );
        assert_eq!(
            say("content_destructive", &["1", "printf %s", "fmt"]),
            "SetFork: step 1 contains a command that is not allowed here: printf %s (rule fmt)",
            "%s внутри аргумента — это данные, а не место для следующего аргумента"
        );
        // Недостача аргументов не имеет права съесть текст вокруг подстановки.
        assert_eq!(say("content_denied", &[]), "SetFork: the app refused this content: ?");
        assert_eq!(say("нет такого ключа", &[]), "SetFork: нет такого ключа");
    }

    #[test]
    fn generated_function_carries_both_languages() {
        let f = shell_msg_fn();
        assert!(f.contains("SETFORK_LANG:-en"), "дефолт — английский");
        for m in HOOK_MESSAGES {
            assert!(f.contains(m.en), "{}: нет английского текста", m.key);
            assert!(f.contains(m.ru), "{}: нет русского текста", m.key);
        }
    }
}
