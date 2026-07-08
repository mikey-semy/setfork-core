use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

// Минимальная реализация git smart-HTTP поверх материализованного репо
// (точный порт sethub-app/src/features/git/smart-http.ts).

/// pkt-line: 4-символьный hex-префикс длины (len включает сами 4 байта) + payload.
fn pkt_line(s: &str) -> Vec<u8> {
    let len = s.len() + 4;
    let mut out = format!("{:04x}", len).into_bytes();
    out.extend_from_slice(s.as_bytes());
    out
}

/// Запуск git с опциональным stdin и GIT_PROTOCOL; возвращает stdout.
/// Тело negotiation мало и умещается в буфер пайпа, поэтому пишем целиком
/// до чтения stdout (для больших тел позже перейдём на gix без шелла).
fn run_git_io(args: &[&str], input: Option<&[u8]>, git_protocol: Option<&str>) -> io::Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(p) = git_protocol {
        if !p.is_empty() {
            cmd.env("GIT_PROTOCOL", p);
        }
    }
    let mut child = cmd.spawn()?;
    {
        // take() + drop в конце блока закрывает stdin (EOF), даже если input=None.
        let mut stdin = child.stdin.take().expect("stdin piped");
        if let Some(data) = input {
            stdin.write_all(data)?;
        }
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr)),
        ));
    }
    Ok(out.stdout)
}

/// GET /info/refs?service=git-upload-pack — реклама ссылок (smart-HTTP).
pub fn upload_pack_advertise(repo_dir: &Path, git_protocol: Option<&str>) -> io::Result<Vec<u8>> {
    let dir = repo_dir.to_string_lossy().to_string();
    let refs = run_git_io(
        &["upload-pack", "--stateless-rpc", "--advertise-refs", &dir],
        None,
        git_protocol,
    )?;
    let mut out = pkt_line("# service=git-upload-pack\n");
    out.extend_from_slice(b"0000");
    out.extend_from_slice(&refs);
    Ok(out)
}

/// POST /git-upload-pack — согласование + packfile.
pub fn upload_pack_rpc(repo_dir: &Path, body: &[u8], git_protocol: Option<&str>) -> io::Result<Vec<u8>> {
    let dir = repo_dir.to_string_lossy().to_string();
    run_git_io(&["upload-pack", "--stateless-rpc", &dir], Some(body), git_protocol)
}

/// GET /info/refs?service=git-receive-pack — реклама для push.
pub fn receive_pack_advertise(repo_dir: &Path, git_protocol: Option<&str>) -> io::Result<Vec<u8>> {
    let dir = repo_dir.to_string_lossy().to_string();
    let refs = run_git_io(
        &["receive-pack", "--stateless-rpc", "--advertise-refs", &dir],
        None,
        git_protocol,
    )?;
    let mut out = pkt_line("# service=git-receive-pack\n");
    out.extend_from_slice(b"0000");
    out.extend_from_slice(&refs);
    Ok(out)
}

/// POST /git-receive-pack — приём пака (обновляет ref'ы в bare-репо).
pub fn receive_pack_rpc(repo_dir: &Path, body: &[u8], git_protocol: Option<&str>) -> io::Result<Vec<u8>> {
    let dir = repo_dir.to_string_lossy().to_string();
    run_git_io(&["receive-pack", "--stateless-rpc", &dir], Some(body), git_protocol)
}
