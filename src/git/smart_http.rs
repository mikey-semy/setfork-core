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
    run_git_io_env(args, input, git_protocol, lang, None)
}

/// То же плюс ник пушащего для хука (Ф4: магический реф `refs/for/<base>` без
/// него отвергается — коммиты некуда класть).
fn run_git_io_env(
    args: &[&str],
    input: Option<&[u8]>,
    git_protocol: Option<&str>,
    lang: Option<&str>,
    actor: Option<&str>,
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
    // Снимаем так же, как язык: унаследованный от сервиса ник означал бы, что
    // чужие коммиты лягут в ветку случайного человека.
    match actor.filter(|a| !a.is_empty()) {
        Some(a) => cmd.env("SETFORK_ACTOR", a),
        None => cmd.env_remove("SETFORK_ACTOR"),
    };
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
    actor: Option<&str>,
) -> io::Result<Vec<u8>> {
    let dir = repo_dir.to_string_lossy().to_string();
    run_git_io_env(&["receive-pack", "--stateless-rpc", &dir], Some(body), git_protocol, lang, actor)
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
    fn язык_сервиса_не_протекает_в_дочерний_git() {
        // Язык запросили — он и уедет.
        assert_eq!(lang_env(Some("ru")), Some(Some("ru".to_string())));
        // Языка нет — переменная СНИМАЕТСЯ, а не наследуется от сервиса. Именно
        // здесь была дыра: «не трогать окружение» означало бы, что SETFORK_LANG
        // инстанса протечёт всем пушащим.
        assert_eq!(lang_env(None), Some(None), "переменная должна сниматься явно");
        assert_eq!(lang_env(Some("")), Some(None), "пустой язык = явный английский");
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
