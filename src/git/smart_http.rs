//! Минимальная реализация git smart-HTTP поверх bare-репо: advertise/RPC для
//! upload-pack (clone/pull) и receive-pack (push) через шелл `git` —
//! точный порт sethub-app/src/features/git/smart-http.ts.
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// pkt-line: 4-символьный hex-префикс длины (len включает сами 4 байта) + payload.
fn pkt_line(s: &str) -> Vec<u8> {
    let len = s.len() + 4;
    let mut out = format!("{:04x}", len).into_bytes();
    out.extend_from_slice(s.as_bytes());
    out
}

/// Выставляет язык дочернему git — или СНИМАЕТ переменную, если языка нет.
///
/// Снимает, а не «оставляет как есть»: пустой язык означает явное «английский».
/// Если сам сервис запущен с SETFORK_LANG (окружение контейнера, compose, шелл
/// оператора), дочерний git унаследовал бы его, и человек, не просивший русского,
/// получил бы русский отказ — одинаково у всех, кто пушит в этот инстанс
/// (авто-ревью core#74, P2).
fn apply_lang(cmd: &mut Command, lang: Option<&str>) {
    match lang.filter(|l| !l.is_empty()) {
        Some(l) => cmd.env("SETFORK_LANG", l),
        None => cmd.env_remove("SETFORK_LANG"),
    };
}

/// Запуск git с опциональным stdin, GIT_PROTOCOL и языком; возвращает stdout.
/// Тело пишется в stdin ПАРАЛЛЕЛЬНО чтению stdout (scoped-поток): раньше
/// запись шла целиком до чтения, и большой push дедлочил оба конца пайпа —
/// git блокировался на записи sideband-прогресса при полном stdout-пайпе,
/// мы — на записи пака (аудит 2026-07-20, P1-4; регрессионный тест внизу).
fn run_git_io(
    args: &[&str],
    input: Option<&[u8]>,
    git_protocol: Option<&str>,
    lang: Option<&str>,
) -> io::Result<Vec<u8>> {
    run_git_io_env(args, input, git_protocol, lang, PushEnv::default())
}

/// Контекст пуша для `pre-receive` — всё, чего хук не может узнать сам.
///
/// Структурой, а не отдельными параметрами: полей ровно столько, сколько
/// переменных окружения ядро ставит хуку, и держать их вместе значит не забыть
/// СНЯТЬ очередную — а именно от этого защищается каждое `env_remove` ниже.
#[derive(Default, Clone, Copy)]
pub struct PushEnv<'a> {
    /// Ф4: ник. Без него магический реф `refs/for/<base>` отвергается — коммиты
    /// некуда класть.
    pub actor: Option<&'a str>,
    /// Ф5: роль пушащего — правило пространства имён для посторонних.
    pub role: Option<&'a str>,
    /// H15-002: чей это список. Приложение спрашивают про него по имени.
    pub owner: Option<&'a str>,
    pub slug: Option<&'a str>,
}

/// Путь к бинарю ядра для хука (H15-002).
///
/// Хук — шелл, HTTP он не умеет, а curl в runtime-образе нет; вопрос о
/// содержимом задаёт подкоманда ЭТОГО ЖЕ бинаря. `current_exe` берёт ровно тот
/// файл, который сейчас обслуживает пуш, — проверка и приём не могут разъехаться
/// по версиям. Переменная-override оставлена ради тестов и аварийного случая.
fn core_bin() -> Option<String> {
    if let Ok(v) = std::env::var("SETFORK_CORE_BIN")
        && !v.trim().is_empty()
    {
        return Some(v);
    }
    std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned())
}

/// Кладёт контекст пуша в окружение дочернего git — оттуда его наследует хук.
///
/// КАЖДАЯ переменная либо ставится, либо СНИМАЕТСЯ, и второе не менее важно:
/// унаследованное от сервиса значение означало бы чужие коммиты в ветке
/// случайного человека (ник), право постороннего писать в main (роль) или
/// вопрос про ОДИН список, заданный глядя на ДРУГОЙ (имя списка).
///
/// Отдельной функцией — чтобы это можно было ПРОВЕРИТЬ: `run_git_io_env` зовёт
/// настоящий `git receive-pack`, и добраться до его окружения из теста нельзя, а
/// до этой функции можно любым дочерним процессом.
fn apply_push_env(cmd: &mut Command, push: PushEnv<'_>) {
    for (key, value) in [
        ("SETFORK_ACTOR", push.actor.map(str::to_string)),
        ("SETFORK_ROLE", push.role.map(str::to_string)),
        ("SETFORK_OWNER", push.owner.map(str::to_string)),
        ("SETFORK_SLUG", push.slug.map(str::to_string)),
        ("SETFORK_CORE_BIN", core_bin()),
    ] {
        match value.filter(|v| !v.is_empty()) {
            Some(v) => cmd.env(key, v),
            None => cmd.env_remove(key),
        };
    }
}

/// То же плюс контекст пушащего для хука (см. `PushEnv`).
fn run_git_io_env(
    args: &[&str],
    input: Option<&[u8]>,
    git_protocol: Option<&str>,
    lang: Option<&str>,
    push: PushEnv<'_>,
) -> io::Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(p) = git_protocol
        && !p.is_empty()
    {
        cmd.env("GIT_PROTOCOL", p);
    }
    // Язык доезжает до pre-receive именно так: хук — отдельный процесс, он
    // наследует окружение receive-pack (проверено экспериментом 31.07, см.
    // HQ tracks/core-i18n.md §2).
    //
    // Пустой язык означает ЯВНОЕ «английский», а не «не трогать окружение»:
    // наследование здесь опасно. Если сам сервис запущен с SETFORK_LANG (в
    // окружении контейнера, в compose, локально в шелле), дочерний git унаследует
    // его, и человек, не просивший русского, получит русский отказ — причём
    // одинаково у всех, кто пушит в этот инстанс (авто-ревью core#74, P2).
    apply_lang(&mut cmd, lang);
    apply_push_env(&mut cmd, push);
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().expect("stdin piped");
    let out = std::thread::scope(|s| -> io::Result<std::process::Output> {
        let writer = s.spawn(move || -> io::Result<()> {
            if let Some(data) = input {
                stdin.write_all(data)?;
            }
            Ok(()) // drop stdin → EOF
        });
        let out = child.wait_with_output()?;
        match writer.join() {
            Ok(Ok(())) => {}
            // git мог завершиться, не дочитав вход (ошибка протокола) — EPIPE
            // не маскирует причину: ниже отдадим stderr по exit-коду.
            Ok(Err(e)) if e.kind() == io::ErrorKind::BrokenPipe => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(io::Error::other("git stdin writer panicked")),
        }
        Ok(out)
    })?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(out.stdout)
}

/// GET /info/refs?service=git-upload-pack — реклама ссылок (smart-HTTP).
pub fn upload_pack_advertise(repo_dir: &Path, git_protocol: Option<&str>) -> io::Result<Vec<u8>> {
    let dir = repo_dir.to_string_lossy().to_string();
    let refs =
        run_git_io(&["upload-pack", "--stateless-rpc", "--advertise-refs", &dir], None, git_protocol, None)?;
    let mut out = pkt_line("# service=git-upload-pack\n");
    out.extend_from_slice(b"0000");
    out.extend_from_slice(&refs);
    Ok(out)
}

/// POST /git-upload-pack — согласование + packfile.
pub fn upload_pack_rpc(repo_dir: &Path, body: &[u8], git_protocol: Option<&str>) -> io::Result<Vec<u8>> {
    let dir = repo_dir.to_string_lossy().to_string();
    run_git_io(&["upload-pack", "--stateless-rpc", &dir], Some(body), git_protocol, None)
}

/// GET /info/refs?service=git-receive-pack — реклама для push.
pub fn receive_pack_advertise(repo_dir: &Path, git_protocol: Option<&str>) -> io::Result<Vec<u8>> {
    let dir = repo_dir.to_string_lossy().to_string();
    let refs =
        run_git_io(&["receive-pack", "--stateless-rpc", "--advertise-refs", &dir], None, git_protocol, None)?;
    let mut out = pkt_line("# service=git-receive-pack\n");
    out.extend_from_slice(b"0000");
    out.extend_from_slice(&refs);
    Ok(out)
}

/// POST /git-receive-pack — приём пака (обновляет ref'ы в bare-репо).
pub fn receive_pack_rpc(
    repo_dir: &Path,
    body: &[u8],
    git_protocol: Option<&str>,
    lang: Option<&str>,
    push: PushEnv<'_>,
) -> io::Result<Vec<u8>> {
    let dir = repo_dir.to_string_lossy().to_string();
    run_git_io_env(&["receive-pack", "--stateless-rpc", &dir], Some(body), git_protocol, lang, push)
}

#[cfg(test)]
mod tests {
    use super::{apply_lang, run_git_io};
    use std::ffi::OsStr;
    use std::process::Command;

    /// Что реально уедет дочернему процессу. `None` в значении = переменная СНЯТА.
    fn lang_env(lang: Option<&str>) -> Option<Option<String>> {
        let mut cmd = Command::new("git");
        apply_lang(&mut cmd, lang);
        cmd.get_envs()
            .find(|(k, _)| *k == OsStr::new("SETFORK_LANG"))
            .map(|(_, v)| v.map(|v| v.to_string_lossy().to_string()))
    }

    #[test]
    fn service_language_does_not_leak_into_child_git() {
        // Язык запросили — он и уедет.
        assert_eq!(lang_env(Some("ru")), Some(Some("ru".to_string())));
        // Языка нет — переменная СНИМАЕТСЯ, а не наследуется от сервиса. Именно
        // здесь была дыра: «не трогать окружение» означало бы, что SETFORK_LANG
        // инстанса протечёт всем пушащим.
        assert_eq!(lang_env(None), Some(None), "переменная должна сниматься явно");
        assert_eq!(lang_env(Some("")), Some(None), "пустой язык = явный английский");
    }

    /// H15-002: ЧТО РЕАЛЬНО ВИДИТ хук. Проверяем настоящим дочерним процессом, а
    /// не чтением карты окружения: между картой и процессом стоит `Command`, и
    /// ошибка вида «поставили не ту переменную» на карте была бы незаметна.
    ///
    /// Две стороны одной монеты. Имя списка ОБЯЗАНО доехать — иначе хук молча
    /// пропустит проверку, и весь этот путь окажется мёртвым. И оно обязано
    /// СНИМАТЬСЯ — иначе значение, оставшееся от соседнего пуша или от
    /// окружения сервиса, заставит спрашивать про один список, глядя на другой.
    #[test]
    fn the_list_name_reaches_the_hook_and_never_leaks_from_elsewhere() {
        let probe = "printf '%s|%s|%s' \"${SETFORK_OWNER:-нет}\" \
                     \"${SETFORK_SLUG:-нет}\" \"${SETFORK_CORE_BIN:+есть}\"";
        let run = |push: super::PushEnv<'_>| {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", probe]);
            // Заранее подложенное чужое значение — то самое, что осталось бы от
            // соседнего пуша, если бы переменная не снималась.
            cmd.env("SETFORK_OWNER", "чужой").env("SETFORK_SLUG", "чужой");
            super::apply_push_env(&mut cmd, push);
            String::from_utf8_lossy(&cmd.output().expect("sh").stdout).to_string()
        };

        assert_eq!(
            run(super::PushEnv { owner: Some("mike"), slug: Some("deploy"), ..Default::default() }),
            "mike|deploy|есть",
            "имя списка и путь к бинарю обязаны доехать до хука — на них держится вся проверка"
        );
        assert_eq!(
            run(super::PushEnv::default()),
            "нет|нет|есть",
            "чужое значение обязано СНИМАТЬСЯ, а не доживать до следующего пуша"
        );
    }

    // Регрессия дедлока (аудит P1-4): git пишет вывод, ПОКА мы пишем ввод.
    // `cat-file --batch` отвечает строкой на строку: >64КБ в обе стороны
    // забивали оба OS-пайпа при последовательной записи. До фикса тест ВИСНЕТ
    // (не падает!), после — проходит мгновенно.
    #[test]
    fn large_bidirectional_io_does_not_deadlock() {
        let dir = std::env::temp_dir().join(format!("setfork-sh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            Command::new("git").args(["init", "-q", "--bare"]).arg(&dir).status().unwrap().success(),
            "git init"
        );
        // ~1.2 МБ запросов; каждый даёт в ответ строку "<oid> missing".
        let line = "0123456789012345678901234567890123456789\n";
        let input = line.repeat(30_000);
        let dir_s = dir.to_string_lossy().to_string();
        let out = run_git_io(&["-C", &dir_s, "cat-file", "--batch"], Some(input.as_bytes()), None, None)
            .expect("cat-file --batch");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(out.len() > 30_000 * 8, "ответ построчный и не пуст ({} байт)", out.len());
    }
}
