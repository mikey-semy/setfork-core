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
        en: "SetFork: only README.md, list.json and .gitattributes are allowed in a list tree.",
        ru: "SetFork: в дереве списка разрешены только README.md, list.json и .gitattributes.",
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
        // тест `тексты_безопасны_для_шелла_и_printf`.
        en: "Remove them from the commit: list content lives in list.json.",
        ru: "Уберите их из коммита: содержимое списка живёт в list.json.",
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
        en: "Propose a change by pushing to refs/for/main, or keep a draft in refs/heads/u/%s/<name>.",
        ru: "Предложите правку пушем в refs/for/main или держите черновик в refs/heads/u/%s/<имя>.",
    },
    HookMessage {
        key: "contributor_needs_actor",
        en: "SetFork: cannot tell who is pushing — re-authenticate with your API token",
        ru: "SetFork: непонятно, кто пушит — повторите вход со своим API-токеном",
    },
];

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
    fn каталог_полон_и_без_дублей() {
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
    fn тексты_безопасны_для_шелла_и_printf() {
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

    #[test]
    fn функция_содержит_оба_языка() {
        let f = shell_msg_fn();
        assert!(f.contains("SETFORK_LANG:-en"), "дефолт — английский");
        for m in HOOK_MESSAGES {
            assert!(f.contains(m.en), "{}: нет английского текста", m.key);
            assert!(f.contains(m.ru), "{}: нет русского текста", m.key);
        }
    }
}
