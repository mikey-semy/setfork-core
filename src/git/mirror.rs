//! Ф3: исходящее push-зеркало списка в GitHub/GitLab.
//!
//! SetFork — источник истины, форджа — витрина и резервная копия. Пушим через
//! шелл-git (как smart_http/bundle; сетевые фичи git2 не включаем) ЯВНЫМ
//! refspec'ом — ⚠️ НЕ `--mirror`: он гонит всё под refs/, а refs/pull/* у
//! GitHub read-only, и пуш падал бы частично (сверка в треке, Gitaly#1913).
//!
//! Что зеркалим: `main` + теги (версии vN и релизы). Ветки предложений — НЕТ
//! (сознательно: это черновики, чужой фордже их шум не нужен; по спросу).
//! `+` в refspec (force) — зеркало обязано догонять истину, даже если его
//! трогали руками; `--prune` убирает на зеркале теги, которых больше нет у нас.
//!
//! Токен: фронт шифрует его AES-256-GCM ключом sha256("mirror:" + секрет), где
//! секрет — SETFORK_MIRROR_SECRET, ОБЩИЙ у фронта и ядра (как SETFORK_CORE_TOKEN).
//! Формат — base64(iv[12] | tag[16] | ct), совместим с shared/auth/totp.ts.
use std::path::Path;
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use sha2::{Digest, Sha256};

/// Общий секрет расшифровки токена зеркала. None — фича выключена: пуш зеркала
/// честно запишет ошибку в статус, а не промолчит.
pub fn mirror_secret() -> Option<&'static str> {
    static SECRET: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    SECRET
        .get_or_init(|| std::env::var("SETFORK_MIRROR_SECRET").ok().filter(|s| !s.trim().is_empty()))
        .as_deref()
}

/// Расшифровка токена (формат фронта: base64(iv[12]|tag[16]|ct), AES-256-GCM,
/// ключ sha256("mirror:"+secret)). None — битые данные или чужой секрет.
pub fn decrypt_token(stored_b64: &str, secret: &str) -> Option<String> {
    let raw = base64::engine::general_purpose::STANDARD.decode(stored_b64.trim()).ok()?;
    if raw.len() < 28 {
        return None;
    }
    let (iv, rest) = raw.split_at(12);
    let (tag, ct) = rest.split_at(16);
    let key_bytes = Sha256::digest(format!("mirror:{secret}").as_bytes());
    let key = Key::<Aes256Gcm>::try_from(key_bytes.as_slice()).expect("sha256 = 32 байта");
    let cipher = Aes256Gcm::new(&key);
    let nonce = Nonce::try_from(iv).ok()?;
    // RustCrypto ждёт постфиксный тег (ct||tag); Node кладёт tag ПЕРЕД ct.
    let mut ct_tag = ct.to_vec();
    ct_tag.extend_from_slice(tag);
    let plain = cipher.decrypt(&nonce, ct_tag.as_ref()).ok()?;
    String::from_utf8(plain).ok()
}

/// Шифрование тем же форматом — для тестов и CLI-диагностики (прод шифрует фронт).
#[cfg(test)]
pub fn encrypt_token(plain: &str, secret: &str) -> String {
    let key_bytes = Sha256::digest(format!("mirror:{secret}").as_bytes());
    let key = Key::<Aes256Gcm>::try_from(key_bytes.as_slice()).expect("sha256 = 32 байта");
    let cipher = Aes256Gcm::new(&key);
    // Случайный nonce из os-rng через uuid (фича getrandom у aes-gcm не включена;
    // для теста формата этого достаточно, прод шифрует фронт).
    let mut iv = [0u8; 12];
    iv.copy_from_slice(&uuid::Uuid::new_v4().as_bytes()[..12]);
    let nonce = Nonce::try_from(&iv[..]).expect("nonce");
    let ct_tag = cipher.encrypt(&nonce, plain.as_bytes()).expect("encrypt");
    let (ct, tag) = ct_tag.split_at(ct_tag.len() - 16);
    let mut out = iv.to_vec();
    out.extend_from_slice(tag);
    out.extend_from_slice(ct);
    base64::engine::general_purpose::STANDARD.encode(out)
}

/// Валидный URL зеркала: только https:// (не file/ssh/git — токен встраиваем в
/// https-креды, а произвольные схемы открывали бы SSRF-поверхность), без
/// собственных кредов в URL (их место — поле токена).
pub fn valid_mirror_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split('/').next().unwrap_or("");
    !host.is_empty() && !host.contains('@') && rest.contains('/')
}

/// Юзернейм для https-кредов по хосту: у GitHub токен идёт с x-access-token,
/// у GitLab — с oauth2; остальным форджам обычно всё равно (token-as-user).
fn cred_user(host: &str) -> &'static str {
    let h = host.to_ascii_lowercase();
    if h.ends_with("github.com") {
        "x-access-token"
    } else if h.contains("gitlab") {
        "oauth2"
    } else {
        "git"
    }
}

/// URL с встроенными кредами: https://user:token@host/path.
fn url_with_token(url: &str, token: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split('/').next().unwrap_or("");
    Some(format!("https://{}:{}@{}", cred_user(host), token, rest))
}

/// Прячет креды в тексте (URL из ошибок git попадает в статус и логи).
/// Однопроходно по каждому https://-URL: наивный «цикл до отсутствия @»
/// не завершался бы — замена сама содержит '@'.
fn redact(text: &str, token: &str) -> String {
    // Пустой токен НЕ подставляем в replace: `"abc".replace("", "***")` вставляет
    // звёздочки между каждым символом, и владелец увидел бы кашу вместо причины
    // (05-F3). Пустой токен — состояние ненормальное, но сообщение об этом должно
    // остаться читаемым.
    let with_token_hidden = if token.is_empty() { text.to_string() } else { text.replace(token, "***") };
    let mut result = String::with_capacity(with_token_hidden.len());
    let mut rest = with_token_hidden.as_str();
    while let Some(i) = rest.find("https://") {
        let (before, from_scheme) = rest.split_at(i);
        result.push_str(before);
        let after_scheme = &from_scheme["https://".len()..];
        // Креды живут в authority — до первого '/'.
        let authority_end = after_scheme.find('/').unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        result.push_str("https://");
        match authority.rfind('@') {
            Some(at) => {
                result.push_str("***@");
                result.push_str(&authority[at + 1..]);
            }
            None => result.push_str(authority),
        }
        rest = &after_scheme[authority_end..];
    }
    result.push_str(rest);
    result
}

/// Аргументы `git push` одной строкой — отдельно от запуска, чтобы их можно было
/// проверить тестом. Проверять есть что: разница между пушем и проверкой держится
/// ровно на этом списке.
fn push_args(bare: &str, pushurl: &str, flags: &[&str], refspecs: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = vec!["--git-dir".into(), bare.into(), "push".into()];
    args.extend(flags.iter().map(|f| (*f).to_string()));
    args.push(pushurl.into());
    args.extend(refspecs.iter().map(|r| (*r).to_string()));
    args
}

/// Общий запуск `git push` с кредами в URL: и для настоящего пуша, и для
/// проверки доступа. Одна функция сознательно — иначе проверка и пуш однажды
/// разъедутся в кредах, таймауте или сокрытии токена, и «доступ есть» перестанет
/// значить «пуш пройдёт».
async fn run_push(
    bare: &Path,
    url: &str,
    token: &str,
    flags: &[&str],
    refspecs: &[&str],
) -> Result<(), String> {
    if !valid_mirror_url(url) {
        return Err("mirror URL must be https://host/owner/repo (без кредов в URL)".to_string());
    }
    let pushurl = url_with_token(url, token).ok_or("bad mirror URL")?;
    let bare_s = bare.to_string_lossy().to_string();
    let args = push_args(&bare_s, &pushurl, flags, refspecs);
    let child = tokio::process::Command::new("git")
        .args(&args)
        // Никаких интерактивных запросов кредов: лучше быстрая ошибка в статус.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(60), child).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("git push не запустился: {e}")),
        Err(_) => return Err("git push: таймаут 60с (сеть/форджа не отвечает)".to_string()),
    };
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: String =
            err.lines().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        Err(redact(tail.trim(), token))
    }
}

/// Пуш зеркала: main + все теги, с force и prune, таймаут 60с.
/// Ok(()) — зеркало догнало истину; Err(text) — текст БЕЗ кредов (для статуса).
pub async fn mirror_push(bare: &Path, url: &str, token: &str) -> Result<(), String> {
    run_push(
        bare,
        url,
        token,
        &["--prune"],
        &["+refs/heads/main:refs/heads/main", "+refs/tags/*:refs/tags/*"],
    )
    .await
}

/// Имя рефа, которого заведомо нет на зеркале. Участвует ТОЛЬКО в refspec'е
/// удаления под `--dry-run`, то есть не создаётся и не удаляется; имя выбрано
/// говорящим, чтобы человек, увидевший его в журнале форджи, понял, что это.
const ACCESS_PROBE_REF: &str = ":refs/heads/setfork-access-check-do-not-create";

/// Ф2: проверка доступа к зеркалу БЕЗ пуша — «Проверить доступ» в настройках.
///
/// Почему не `git ls-remote`, как просилось изначально: он проверяет ЧТЕНИЕ.
/// Проверено на живом GitHub — с заведомо мусорным токеном на публичном
/// репозитории `ls-remote` возвращает exit 0. Кнопка обещала бы доступ там, где
/// пуш откажет, а именно так и ведёт себя read-only токен.
///
/// `git push --dry-run` идёт по пути ЗАПИСИ: аутентифицируется на receive-pack
/// (у GitHub и GitLab этот эндпоинт требует прав на запись) и на этом
/// останавливается — команды обновления не отправляются, поэтому на фордже не
/// меняется ничего.
///
/// Refspec — УДАЛЕНИЕ несуществующего рефа, а не пуш `main`. Так проверка
/// работает и до первой публикации: у пустого репозитория `main` ещё нет, а
/// обычный refspec отваливается на локальном разборе, не доходя до сети
/// (проверено: «src refspec ... does not match any» приходит раньше соединения).
/// У refspec'а удаления локального источника нет, поэтому git идёт в сеть сразу.
///
/// ⚠️ `--prune` здесь БЫТЬ НЕ ДОЛЖНО, в отличие от настоящего пуша: с одним
/// refspec'ом удаления он означает «снести на зеркале всё, что не названо», то
/// есть весь репозиторий. Сейчас это спасал бы только `--dry-run`, а страховка в
/// один флаг — не страховка. Список аргументов держит тест.
pub async fn mirror_check(bare: &Path, url: &str, token: &str) -> Result<(), String> {
    run_push(bare, url, token, &["--dry-run"], &[ACCESS_PROBE_REF]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn токен_шифруется_и_расшифровывается_совместимым_форматом() {
        let secret = "test-mirror-secret";
        let enc = encrypt_token("ghp_abc123", secret);
        assert_eq!(decrypt_token(&enc, secret).as_deref(), Some("ghp_abc123"));
        assert_eq!(decrypt_token(&enc, "другой-секрет"), None, "чужой секрет не читает");
        assert_eq!(decrypt_token("мусор", secret), None);
        assert_eq!(decrypt_token("AAAA", secret), None, "короче iv+tag");
    }

    #[test]
    fn урл_зеркала_только_https_без_кредов() {
        assert!(valid_mirror_url("https://github.com/user/repo.git"));
        assert!(valid_mirror_url("https://gitlab.com/group/proj.git"));
        for bad in [
            "http://github.com/u/r",        // не https
            "https://token@github.com/u/r", // креды в URL — их место в поле токена
            "file:///etc/passwd",
            "ssh://git@host/r",
            "https://hostonly",
            "",
        ] {
            assert!(!valid_mirror_url(bad), "{bad:?}");
        }
    }

    #[test]
    fn креды_встраиваются_по_хосту() {
        assert_eq!(
            url_with_token("https://github.com/u/r.git", "T").as_deref(),
            Some("https://x-access-token:T@github.com/u/r.git")
        );
        assert_eq!(
            url_with_token("https://gitlab.com/g/p.git", "T").as_deref(),
            Some("https://oauth2:T@gitlab.com/g/p.git")
        );
        assert_eq!(
            url_with_token("https://forge.example/u/r.git", "T").as_deref(),
            Some("https://git:T@forge.example/u/r.git")
        );
    }

    /// Разница между «проверить» и «запушить» — это ровно список аргументов,
    /// поэтому он и проверяется. Цена ошибки несимметрична: лишний `--prune` у
    /// проверки означает refspec «снести на зеркале всё, что не названо», и
    /// удерживал бы репозиторий от смерти один-единственный `--dry-run`.
    #[test]
    fn проверка_доступа_ничего_не_меняет_на_фордже() {
        let args =
            push_args("/repo.git", "https://u:t@github.com/o/r.git", &["--dry-run"], &[ACCESS_PROBE_REF]);
        assert!(args.contains(&"--dry-run".to_string()), "без dry-run это уже не проверка: {args:?}");
        assert!(
            !args.contains(&"--prune".to_string()),
            "prune с удаляющим refspec снёс бы зеркало: {args:?}"
        );
        assert!(
            args.iter().all(|a| !a.starts_with('+')),
            "силовых refspec'ов у проверки быть не должно: {args:?}"
        );
        // Реф пробы — только в форме удаления (ведущее ':'), то есть создать его
        // не может даже опечатка.
        assert!(ACCESS_PROBE_REF.starts_with(':'), "проба обязана быть refspec'ом удаления");
    }

    #[test]
    fn пуш_зеркала_остаётся_силовым_и_с_prune() {
        let args = push_args(
            "/repo.git",
            "https://u:t@github.com/o/r.git",
            &["--prune"],
            &["+refs/heads/main:refs/heads/main", "+refs/tags/*:refs/tags/*"],
        );
        assert!(args.contains(&"--prune".to_string()), "{args:?}");
        assert!(!args.contains(&"--dry-run".to_string()), "настоящий пуш не может быть холостым: {args:?}");
        // URL идёт ПЕРЕД refspec'ами — иначе git примет его за refspec.
        let url_at = args.iter().position(|a| a.starts_with("https://")).expect("url");
        let first_spec = args.iter().position(|a| a.starts_with('+')).expect("refspec");
        assert!(url_at < first_spec, "{args:?}");
    }

    #[test]
    fn токен_не_утекает_в_текст_ошибки() {
        let msg = "fatal: unable to access 'https://x-access-token:ghp_SECRET@github.com/u/r.git/': 403";
        assert_eq!(redact("ошибка без кредов", ""), "ошибка без кредов", "пустой токен не крошит текст");
        let red = redact(msg, "ghp_SECRET");
        assert!(!red.contains("ghp_SECRET"), "{red}");
        assert!(!red.contains("x-access-token:"), "{red}");
        assert!(red.contains("github.com/u/r.git"), "путь остаётся читаемым: {red}");
    }
}
