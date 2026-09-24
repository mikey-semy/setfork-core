//! H15-002: содержимое, приехавшее пушем, судится тем же правилом, что и
//! содержимое из формы.
//!
//! До этой правки `pre-receive` проверял только ФОРМУ дерева — защиту main,
//! fast-forward, обязательный `list.json`, состав путей, — а `command` шага
//! оставался непрозрачным JSON. Список с `rm -rf /` в шаге приезжал пушем и жил
//! в каноне, хотя из редактора продукт его бы не принял.
//!
//! Проверяем НАСТОЯЩИМ `git push` в настоящий bare-репозиторий с настоящим
//! хуком, который зовёт настоящий бинарь ядра, ходящий в настоящий HTTP. Мок
//! вместо любого из звеньев проверил бы что угодно, кроме того, ради чего эта
//! цепочка и собрана: шелл-хук HTTP не умеет, и вся конструкция держится на том,
//! что подкоманда бинаря доступна ему, получает канон на stdin и умеет вернуть
//! код возврата.
//!
//! Заглушка приложения тут не отдаёт заготовленный ответ, а СМОТРИТ В ТЕЛО и
//! считает шаг сама. Иначе тест был бы зелёным и у ядра, которое `blocks` не
//! посылает вовсе, — то есть не проверял бы главного.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Каталог-однодневка: удаляется на Drop (в т.ч. при panic внутри теста).
struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn tmp(prefix: &str) -> Tmp {
    let p = std::env::temp_dir().join(format!("setfork-{prefix}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).expect("create tmp");
    Tmp(p)
}

// ── Заглушка приложения ──────────────────────────────────────────────────────

/// Отвечает на `POST /api/internal/write-allowed`. Режим решает, насколько она
/// похожа на сегодняшнее приложение.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AppKind {
    /// Умеет `blocks`: ищет запрещённую команду и называет её место.
    New,
    /// Старее этой правки: поля не знает и отвечает прежним `{"allow":true}`.
    Old,
}

struct App {
    addr: String,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl App {
    fn start(kind: AppKind) -> App {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = format!("http://{}", listener.local_addr().expect("addr"));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_t.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(mut s) = stream else { continue };
                serve_one(&mut s, kind);
            }
        });
        App { addr, stop, handle: Some(handle) }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr.trim_start_matches("http://"));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve_one(s: &mut TcpStream, kind: AppKind) {
    let mut reader = BufReader::new(s.try_clone().expect("clone"));
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut body = vec![0u8; len];
    let _ = reader.read_exact(&mut body);
    let verdict = verdict_for(&body, kind);
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        verdict.len(),
        verdict
    );
    let _ = s.write_all(resp.as_bytes());
    let _ = s.flush();
}

/// Правило заглушки — миниатюра продуктового: первая команда с `rm -rf` и место
/// находки. Номер шага считается КАК В ПРИЛОЖЕНИИ — индекс в присланном массиве
/// плюс единица, — поэтому тест заодно проверяет, что ядро шлёт блоки, ничего не
/// выбрасывая.
fn verdict_for(body: &[u8], kind: AppKind) -> String {
    if kind == AppKind::Old {
        return r#"{"allow":true}"#.to_string();
    }
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return r#"{"allow":true}"#.to_string(),
    };
    let blocks = v.get("blocks").and_then(|b| b.as_array()).cloned().unwrap_or_default();
    for (i, b) in blocks.iter().enumerate() {
        let cmd = b.get("command").and_then(|c| c.as_str()).unwrap_or("");
        if cmd.contains("rm -rf") {
            return serde_json::json!({
                "allow": false,
                "reason": "destructive",
                "step": i + 1,
                "rule": "rm_rf",
                "fragment": "rm -rf",
            })
            .to_string();
        }
    }
    // Ключ доступа — миниатюра `secret-scan.ts`: метка вместо шаблона провайдера, место —
    // файл из присланных и строка, посчитанная тут же. Так тест проверяет, что ядро шлёт
    // тексты коммитов, а не то, что заглушка умеет отвечать.
    let files = v.get("files").and_then(|f| f.as_array()).cloned().unwrap_or_default();
    for f in &files {
        let text = f.get("text").and_then(|t| t.as_str()).unwrap_or("");
        if let Some(at) = text.find(LEAK) {
            return serde_json::json!({
                "allow": false,
                "reason": "secret",
                "path": f.get("path").and_then(|p| p.as_str()).unwrap_or(""),
                "step": 0,
                "line": text[..at].matches('\n').count() + 1,
                "rule": "test-key",
                "provider": "Test",
                "fragment": "LEAKED…",
            })
            .to_string();
        }
    }
    r#"{"allow":true}"#.to_string()
}

/// Метка «ключа» для заглушки: настоящий шаблон провайдера тут не нужен — его судит приложение.
const LEAK: &str = "LEAKED-KEY";

// ── Git от лица владельца ────────────────────────────────────────────────────

/// Окружение, которое в проде выставляет `smart_http::run_git_io_env`. Здесь
/// пуш идёт напрямую, поэтому переменные ставит тест — ровно те же самые.
struct Env {
    app_url: String,
    core_bin: String,
    actor: String,
}

impl Env {
    fn new(app_url: &str) -> Env {
        Env {
            app_url: app_url.to_string(),
            core_bin: env!("CARGO_BIN_EXE_setfork-core").to_string(),
            actor: "00000000-0000-0000-0000-000000000001".to_string(),
        }
    }
}

fn git(dir: &Path, env: &Env, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(dir)
        .env("SETFORK_ROLE", "owner")
        .env("SETFORK_ACTOR", &env.actor)
        .env("SETFORK_OWNER", "mike")
        .env("SETFORK_SLUG", "deploy")
        .env("SETFORK_APP_URL", &env.app_url)
        .env("SETFORK_CORE_BIN", &env.core_bin)
        // Токен канала из окружения разработчика не должен утекать в заглушку.
        .env_remove("SETFORK_CORE_TOKEN")
        .env_remove("SETFORK_LANG")
        .args(args)
        .output()
        .expect("git запустился")
}

fn git_ok(dir: &Path, env: &Env, args: &[&str]) {
    let out = git(dir, env, args);
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Канон с одним шагом и его командой.
fn canon(command: &str) -> String {
    serde_json::json!({
        "title": "Развёртывание",
        "steps": [
            { "type": "text", "content": { "md": "вводная" } },
            { "title": "Убрать за собой", "command": command },
        ],
    })
    .to_string()
}

/// Bare с хуком + рабочая копия с одной безобидной версией в main.
fn repo_pair(root: &Path, env: &Env) -> (PathBuf, PathBuf) {
    let bare = root.join("bare.git");
    git_ok(root, env, &["init", "-q", "--bare", "--initial-branch=main", bare.to_str().unwrap()]);
    setfork_core::git::bundle::install_hook(&bare).expect("хук установлен");

    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("mkdir work");
    git_ok(root, env, &["init", "-q", "--initial-branch=main", work.to_str().unwrap()]);
    git_ok(&work, env, &["config", "user.email", "t@example.com"]);
    git_ok(&work, env, &["config", "user.name", "Тест"]);
    git_ok(&work, env, &["remote", "add", "origin", bare.to_str().unwrap()]);
    std::fs::write(work.join("list.json"), canon("echo hi")).expect("list.json");
    std::fs::write(work.join("README.md"), b"# t\n").expect("README");
    git_ok(&work, env, &["add", "-A"]);
    git_ok(&work, env, &["commit", "-q", "-m", "v1"]);
    git_ok(&work, env, &["push", "-q", "origin", "main"]);
    (bare, work)
}

/// Новый коммит с данной командой в шаге.
fn commit_command(work: &Path, env: &Env, command: &str, message: &str) {
    std::fs::write(work.join("list.json"), canon(command)).expect("list.json");
    git_ok(work, env, &["add", "-A"]);
    git_ok(work, env, &["commit", "-q", "-m", message]);
}

fn tip(bare: &Path, env: &Env, refname: &str) -> String {
    let out = git(bare, env, &["rev-parse", refname]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ── Сами проверки ────────────────────────────────────────────────────────────

/// Находка H15-002 целиком: `rm -rf /` в шаге не доезжает до канона, а человек
/// узнаёт КАКОЙ шаг и КАКАЯ команда — иначе отказ бесполезен.
#[test]
fn a_destructive_command_does_not_get_into_the_canon() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("content-deny");
    let (bare, work) = repo_pair(&root.0, &env);
    let before = tip(&bare, &env, "main");

    commit_command(&work, &env, "rm -rf /", "почистить");
    let out = git(&work, &env, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "разрушительная команда обязана быть отвергнута: {err}");
    assert!(err.contains("step 2"), "отказ обязан назвать ШАГ, а не только факт: {err}");
    assert!(err.contains("rm -rf"), "отказ обязан назвать КОМАНДУ: {err}");
    assert_eq!(tip(&bare, &env, "main"), before, "main не имеет права сдвинуться после отказа");
}

/// Обратная сторона той же монеты и цена ошибки: пуш — рабочий путь, им правят
/// списки каждый день. Ложный отказ здесь дороже пропуска.
#[test]
fn an_ordinary_push_still_goes_through() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("content-allow");
    let (bare, work) = repo_pair(&root.0, &env);
    let before = tip(&bare, &env, "main");

    commit_command(&work, &env, "make build", "обычная правка");
    let out = git(&work, &env, &["push", "origin", "main"]);
    assert!(out.status.success(), "обычный пуш: {}", String::from_utf8_lossy(&out.stderr));
    assert_ne!(tip(&bare, &env, "main"), before, "main обязан сдвинуться");
}

/// ОКНО ВЫКАТКИ, прямой порядок: ядро новое, приложение ещё старое. Оно поля
/// `blocks` не знает, отвечает прежним `{"allow":true}` — и пуш обязан вести
/// себя ровно как вчера, а не упираться в отказ.
#[test]
fn an_app_that_predates_blocks_does_not_stop_the_push() {
    let app = App::start(AppKind::Old);
    let env = Env::new(&app.addr);
    let root = tmp("content-oldapp");
    let (bare, work) = repo_pair(&root.0, &env);
    let before = tip(&bare, &env, "main");

    commit_command(&work, &env, "rm -rf /", "почистить");
    let out = git(&work, &env, &["push", "origin", "main"]);
    assert!(
        out.status.success(),
        "старое приложение = сегодняшнее поведение: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_ne!(tip(&bare, &env, "main"), before);
}

/// ОКНО ВЫКАТКИ, обратный порядок: хук новый, а бинаря по указанному пути нет
/// (или он не исполняется). Это НАША поломка, а не приговор пушу: остановить
/// запись всем сразу из-за неверного пути было бы дороже пропуска. Но и тихой
/// такая дыра быть не должна — про неё сказано в выводе.
#[test]
fn a_missing_core_binary_lets_the_push_through_but_says_so() {
    let app = App::start(AppKind::New);
    let mut env = Env::new(&app.addr);
    let root = tmp("content-nobin");
    let (bare, work) = repo_pair(&root.0, &env);
    let before = tip(&bare, &env, "main");

    env.core_bin = root.0.join("no-such-binary").to_string_lossy().into_owned();
    commit_command(&work, &env, "rm -rf /", "почистить");
    let out = git(&work, &env, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "сломанный путь к бинарю не имеет права останавливать пуш: {err}");
    assert!(err.contains("went unchecked"), "окно несовместимости не должно быть тихим: {err}");
    assert_ne!(tip(&bare, &env, "main"), before);
}

/// Приложение не отвечает. Fail-closed, как и у предусловия записи: дверь,
/// открытая при сбое, обесценила бы проверку — уронить фронт проще, чем обойти
/// правило. Отказ при этом называет СВОЮ природу: советовать человеку править
/// шаг, когда дело в связи, значит отправить его чинить исправное.
#[test]
fn an_unreachable_app_stops_the_push_and_names_the_reason() {
    // Репозиторий заводим при ЖИВОМ приложении (первая версия тоже несёт
    // команду и тоже спрашивается), и только потом уводим адрес в пустоту.
    let app = App::start(AppKind::New);
    let mut env = Env::new(&app.addr);
    let root = tmp("content-down");
    let (bare, work) = repo_pair(&root.0, &env);
    let before = tip(&bare, &env, "main");

    env.app_url = {
        // Порт, который никто не слушает: занимаем и сразу отпускаем.
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        format!("http://{}", l.local_addr().expect("addr"))
    };
    commit_command(&work, &env, "make build", "обычная правка");
    let out = git(&work, &env, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "недоступное приложение не открывает запись: {err}");
    assert!(err.contains("did not answer"), "отказ обязан отличаться от отказа по содержимому: {err}");
    assert!(err.contains("try the push again"), "человеку нужен верный совет: {err}");
    assert_eq!(tip(&bare, &env, "main"), before);
}

/// Предложение (`refs/for/main`) судится тоже: приложение проверяет предложения
/// своим фасадом, и git-дверь обязана быть не мягче формы. Иначе запрет
/// обходился бы в два шага — предложить и принять.
#[test]
fn a_suggestion_is_judged_as_well() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("content-magic");
    let (_bare, work) = repo_pair(&root.0, &env);

    commit_command(&work, &env, "rm -rf /", "предложение");
    let out = git(&work, &env, &["push", "origin", "HEAD:refs/for/main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "предложение с разрушительной командой не принимается: {err}");
    assert!(err.contains("step 2"), "{err}");
}

/// ГРАНИЦА, выбранная осознанно: черновая ветка не судится.
///
/// В канон она не проецируется, зато на неё законно уезжает УЖЕ СУЩЕСТВУЮЩАЯ
/// история — а в ней команды, приехавшие до этой проверки, есть по условию
/// самой находки. Судить черновики значило бы отказывать человеку за то, что
/// давно лежит в его же репозитории, и «ложный отказ дороже пропуска» здесь
/// перевешивает: из черновика в канон всё равно нет пути мимо main.
#[test]
fn a_draft_branch_is_left_alone() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("content-draft");
    let (_bare, work) = repo_pair(&root.0, &env);

    commit_command(&work, &env, "rm -rf /", "черновик");
    let out = git(&work, &env, &["push", "origin", "HEAD:refs/heads/draft"]);
    assert!(out.status.success(), "черновик не судится: {}", String::from_utf8_lossy(&out.stderr));
}

/// Русский отказ приезжает по той же переменной, что и остальной вывод хука, —
/// значит `SETFORK_LANG` доезжает не только до шелла, но и до подкоманды.
#[test]
fn the_refusal_speaks_the_language_of_the_push() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("content-ru");
    let (_bare, work) = repo_pair(&root.0, &env);

    commit_command(&work, &env, "rm -rf /", "почистить");
    let out = Command::new("git")
        .current_dir(&work)
        .env("SETFORK_ROLE", "owner")
        .env("SETFORK_OWNER", "mike")
        .env("SETFORK_SLUG", "deploy")
        .env("SETFORK_APP_URL", &env.app_url)
        .env("SETFORK_CORE_BIN", &env.core_bin)
        .env("SETFORK_LANG", "ru")
        .env_remove("SETFORK_CORE_TOKEN")
        .args(["push", "origin", "main"])
        .output()
        .expect("git запустился");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("в шаге 2"), "русский отказ обязан прийти по-русски: {err}");
}

// ── Скрипты из `scripts/` (ADR-0028) ────────────────────────────────────────
//
// Дерево принимает авторские скрипты, и без этой проверки они становились
// обходом всей H15-002: `rm -rf /`, который форма не пустит в шаг, въезжал бы в
// `scripts/run.sh` пушем — и дальше в каждый поставленный скилл.

fn add_script(work: &Path, env: &Env, path: &str, text: &str, message: &str) {
    let p = work.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).expect("mkdir");
    std::fs::write(p, text).expect("script");
    git_ok(work, env, &["add", "-A"]);
    git_ok(work, env, &["commit", "-q", "-m", message]);
}

#[test]
fn a_destructive_script_does_not_get_in_and_is_named_by_file() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("content-script-deny");
    let (bare, work) = repo_pair(&root.0, &env);
    let before = tip(&bare, &env, "main");

    // Опасная строка — в СЕРЕДИНЕ многострочного файла, как в настоящем скрипте.
    add_script(
        &work,
        &env,
        "scripts/cleanup.sh",
        "#!/bin/sh\nset -e\necho cleanup\nrm -rf /\necho done\n",
        "скрипт",
    );
    let out = git(&work, &env, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "разрушительный скрипт обязан быть отвергнут: {err}");
    assert!(err.contains("scripts/cleanup.sh"), "отказ обязан назвать ФАЙЛ: {err}");
    // У списка два блока; «step 3» указывал бы в пустоту — такого шага нет нигде.
    assert!(!err.contains("step 3"), "файл назван номером несуществующего шага: {err}");
    assert_eq!(tip(&bare, &env, "main"), before, "main не имеет права сдвинуться после отказа");
}

#[test]
fn a_harmless_script_goes_through() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("content-script-allow");
    let (bare, work) = repo_pair(&root.0, &env);
    let before = tip(&bare, &env, "main");

    add_script(&work, &env, "scripts/run.sh", "#!/bin/sh\nmake build\n", "скрипт");
    let out = git(&work, &env, &["push", "origin", "main"]);
    assert!(out.status.success(), "безобидный скрипт: {}", String::from_utf8_lossy(&out.stderr));
    assert_ne!(tip(&bare, &env, "main"), before);
}

// Справка — не исполняемое: `references/` описывают опасное словами, и отказ за
// «rm -rf» в документе про то, почему так нельзя, был бы ложным.
#[test]
fn references_are_not_judged_as_commands() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("content-refs");
    let (_bare, work) = repo_pair(&root.0, &env);

    add_script(&work, &env, "references/why.md", "Never run `rm -rf /` on a server.\n", "справка");
    let out = git(&work, &env, &["push", "origin", "main"]);
    assert!(out.status.success(), "справка не команда: {}", String::from_utf8_lossy(&out.stderr));
}

// Обратная сторона сопоставления номера с файлом: команда ШАГА по-прежнему
// называется шагом, даже когда рядом лежат скрипты.
#[test]
fn a_destructive_step_is_still_named_as_a_step_next_to_scripts() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("content-step-and-script");
    let (_bare, work) = repo_pair(&root.0, &env);

    add_script(&work, &env, "scripts/run.sh", "#!/bin/sh\nmake build\n", "скрипт");
    commit_command(&work, &env, "rm -rf /", "почистить");
    let out = git(&work, &env, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("step 2"), "команда шага названа не шагом: {err}");
    assert!(!err.contains("scripts/run.sh"), "вину шага приписали безобидному скрипту: {err}");
}

// Манифест, который не разбирается (здесь — JSON-массив), раньше вёл в «судить
// нечего»: шагов не видно, версии из него не выйдет. Со `scripts/` это стало дырой —
// скрипт проходил непроверенным, а следующая правка с сайта переносит его в
// настоящую версию (перенос авторских каталогов из родителя). Скрипты судятся
// независимо от того, разобрался ли list.json.
#[test]
fn a_script_is_judged_even_when_the_manifest_does_not_parse() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("content-bad-canon");
    let (bare, work) = repo_pair(&root.0, &env);
    let before = tip(&bare, &env, "main");

    std::fs::write(work.join("list.json"), "[]").expect("list.json");
    add_script(
        &work,
        &env,
        "scripts/cleanup.sh",
        "#!/bin/sh\nrm -rf /\n",
        "манифест-массив и опасный скрипт",
    );
    let out = git(&work, &env, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "непроверенный скрипт проехал за неразборчивым манифестом: {err}");
    assert!(err.contains("scripts/cleanup.sh"), "{err}");
    assert_eq!(tip(&bare, &env, "main"), before);
}

// ── Ключи доступа: по истории пуша, а не по вершине ──────────────────────────
//
// `git clone` отдаёт историю целиком, поэтому ключ, добавленный коммитом и убранный
// следующим, утекает всё равно. Судится всё, что пуш приносит ВПЕРВЫЕ, — и только оно.

#[test]
fn a_key_in_a_reference_is_refused_by_file_and_line() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("secret-ref");
    let (bare, work) = repo_pair(&root.0, &env);
    let before = tip(&bare, &env, "main");

    add_script(&work, &env, "references/setup.md", &format!("# Setup\n\ntoken: {LEAK}\n"), "справка");
    let out = git(&work, &env, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "ключ в справке обязан остановить пуш: {err}");
    assert!(err.contains("references/setup.md, line 3"), "отказ называет файл и строку: {err}");
    assert!(!err.contains("references/setup.md@"), "файл вершины — без пометки истории: {err}");
    assert_eq!(tip(&bare, &env, "main"), before);
}

#[test]
fn a_key_removed_by_a_later_commit_is_still_refused() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("secret-history");
    let (bare, work) = repo_pair(&root.0, &env);
    let before = tip(&bare, &env, "main");

    add_script(&work, &env, "references/setup.md", &format!("token: {LEAK}\n"), "с ключом");
    add_script(&work, &env, "references/setup.md", "token: <ваш ключ>\n", "ключ убран");
    let out = git(&work, &env, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "ключ в истории пуша утекает с clone: {err}");
    assert!(err.contains("references/setup.md@"), "отказ говорит, что чинить историю: {err}");
    assert_eq!(tip(&bare, &env, "main"), before);
}

#[test]
fn a_key_in_a_step_description_is_found_in_list_json() {
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    let root = tmp("secret-canon");
    let (_bare, work) = repo_pair(&root.0, &env);

    // Не команда — описание шага: команды приложение судит отдельно, а ключ утекает отовсюду.
    let canon =
        serde_json::json!({ "title": "t", "steps": [{ "title": "Вход", "desc": format!("пароль {LEAK}") }] });
    std::fs::write(work.join("list.json"), canon.to_string()).expect("list.json");
    git_ok(&work, &env, &["add", "-A"]);
    git_ok(&work, &env, &["commit", "-q", "-m", "описание"]);
    let out = git(&work, &env, &["push", "origin", "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "ключ в описании шага: {err}");
    assert!(err.contains("list.json, line 1"), "назван list.json: {err}");
}

#[test]
fn a_key_already_on_the_server_is_not_judged_again() {
    let root = tmp("secret-old");
    // Ключ лёг в репозиторий, когда приложение ещё не искало ключей.
    let old = App::start(AppKind::Old);
    let env_old = Env::new(&old.addr);
    let (_bare, work) = repo_pair(&root.0, &env_old);
    add_script(&work, &env_old, "references/setup.md", &format!("token: {LEAK}\n"), "старое");
    git_ok(&work, &env_old, &["push", "-q", "origin", "main"]);

    // Новая правка его не трогает: отказ за то, что давно лежит в репозитории, стоил бы
    // человеку работы, а утечку не отменил бы.
    let app = App::start(AppKind::New);
    let env = Env::new(&app.addr);
    add_script(&work, &env, "scripts/run.sh", "#!/bin/sh\nmake\n", "новое");
    let out = git(&work, &env, &["push", "origin", "main"]);
    assert!(out.status.success(), "судится только новое: {}", String::from_utf8_lossy(&out.stderr));
}
