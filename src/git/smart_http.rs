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

/// Запуск git с опциональным stdin и GIT_PROTOCOL; возвращает stdout.
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
    let mut cmd = Command::new("git");
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(p) = git_protocol
        && !p.is_empty()
    {
        cmd.env("GIT_PROTOCOL", p);
    }
    // Язык доезжает до pre-receive именно так: хук — отдельный процесс, он
    // наследует окружение receive-pack (проверено экспериментом 31.07, см.
    // HQ tracks/core-i18n.md §2). Пусто — не выставляем вовсе, хук возьмёт
    // английский по умолчанию.
    if let Some(l) = lang
        && !l.is_empty()
    {
        cmd.env("SETFORK_LANG", l);
    }
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
) -> io::Result<Vec<u8>> {
    let dir = repo_dir.to_string_lossy().to_string();
    run_git_io(&["receive-pack", "--stateless-rpc", &dir], Some(body), git_protocol, lang)
}

#[cfg(test)]
mod tests {
    use super::run_git_io;
    use std::process::Command;

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
