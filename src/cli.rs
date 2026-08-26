//! Режимы CLI ядра: golden-сверка с TS и операторские команды.
//!
//! Вынесено из `main` (линза 08): это ОПЕРАТОРСКИЙ инструмент, а не часть сервера.
//! Смешанные в одной функции, они читались через раз — а читают их в разное время и
//! разные люди: golden-режимы при сверке с фронтом, `sync-repos`/`gc-repos` в аварии.
//!
//! Возвращает `None`, если первый аргумент не оказался командой: тогда `main` идёт
//! поднимать сервер. ⚠️ Неизвестная команда (опечатка) — это тоже `None`, то есть
//! запуск СЕРВЕРА, а не сообщение об ошибке; про это сказано в README.

use sqlx::PgPool;

use setfork_core::git;
use setfork_core::git::bundle::{self, VersionData};
use setfork_core::git::smart_http;
use setfork_core::services;
use setfork_core::services::git_core::GitCoreSvc;

type Итог = Result<(), Box<dyn std::error::Error>>;

// Материализует репо во временный каталог, выполняет `op` над ним и гарантированно
// удаляет каталог. `op` синхронна (шелл git) — весь блок идёт в spawn_blocking.
fn with_materialized<F>(versions: Vec<VersionData>, op: F) -> std::io::Result<Vec<u8>>
where
    F: FnOnce(&std::path::Path) -> std::io::Result<Vec<u8>>,
{
    let dir = bundle::materialize_repo(&versions)?;
    let res = op(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    res
}

/// Команды, которые ядро исполняет вместо запуска сервера.
///
/// Список ЯВНЫЙ и продублирован с `match` ниже — сознательно: `run` обязана решить
/// «команда или сервер» ДО того, как войдёт в исполнение, иначе `?` внутри некуда
/// возвращать. Расхождение списка с ветками ловится тестом в конце файла: правило,
/// живущее в двух местах, обязано иметь сверку (корень K38).
const КОМАНДЫ: &[&str] = &[
    "bundle",
    "advertise-upload",
    "advertise-receive",
    "domain-read",
    "upload-pack",
    "reproject",
    "gc-repos",
    "sync-repos",
];

/// `None` — первый аргумент не команда, `main` идёт поднимать сервер.
pub async fn run(pool: &PgPool) -> Option<Итог> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1)?.to_string();
    if !КОМАНДЫ.contains(&cmd.as_str()) {
        return None;
    }
    Some(выполнить(pool, &cmd, &args).await)
}

async fn выполнить(pool: &PgPool, cmd: &str, args: &[String]) -> Итог {
    // CLI-режимы для golden-проверки (тот же код-пас, что и RPC, без gRPC-транспорта):
    //   bundle           <owner> <slug> <out>
    //   advertise-upload <owner> <slug> <out>   (== GET /info/refs?service=git-upload-pack)
    //   advertise-receive<owner> <slug> <out>   (== GET /info/refs?service=git-receive-pack)
    //   upload-pack      <owner> <slug> <body> <out>  (== POST /git-upload-pack)
    let cli = |i: usize| args.get(i).cloned().unwrap_or_default();
    // pool клонируется (Arc внутри) — оригинал остаётся серверу ниже.
    let svc = GitCoreSvc { pool: pool.clone() };
    match cmd {
        "bundle" => {
            let data = svc.build(&cli(2), &cli(3)).await.map_err(|e| e.to_string())?;
            std::fs::write(cli(4), &data)?;
            println!("wrote {} bytes → {}", data.len(), cli(4));
            Ok(())
        }
        "advertise-upload" | "advertise-receive" => {
            let versions = svc.load(&cli(2), &cli(3)).await.map_err(|e| e.to_string())?;
            let up = cmd == "advertise-upload";
            let data = tokio::task::spawn_blocking(move || {
                with_materialized(versions, |dir| {
                    if up {
                        smart_http::upload_pack_advertise(dir, None)
                    } else {
                        smart_http::receive_pack_advertise(dir, None)
                    }
                })
            })
            .await??;
            std::fs::write(cli(4), &data)?;
            println!("wrote {} bytes → {}", data.len(), cli(4));
            Ok(())
        }
        // Golden-сверка READ-портов: канонический JSON (см. services::golden)
        //   domain-read <owner> <slug> <out.json>
        "domain-read" => {
            let v = services::golden::golden_json(pool, &cli(2), &cli(3)).await.map_err(|e| e.to_string())?;
            std::fs::write(cli(4), serde_json::to_string_pretty(&v)?)?;
            println!("wrote domain-read json → {}", cli(4));
            Ok(())
        }
        "upload-pack" => {
            let versions = svc.load(&cli(2), &cli(3)).await.map_err(|e| e.to_string())?;
            let body = std::fs::read(cli(4))?;
            let data = tokio::task::spawn_blocking(move || {
                with_materialized(versions, |dir| smart_http::upload_pack_rpc(dir, &body, None))
            })
            .await??;
            std::fs::write(cli(5), &data)?;
            println!("wrote {} bytes → {}", data.len(), cli(5));
            Ok(())
        }
        // ШТАТНЫЙ rebuild хвоста проекции (Ф1: git — канон, БД — read-model):
        // проецирует текущий main-tip в НОВУЮ версию списка. Применять, когда
        // git оказался впереди БД (лог «канон записан, проекция отстала»).
        // Не проверяет, была ли версия уже создана — инструмент оператора,
        // см. runbook git-projection-catchup.
        //   reproject <owner> <slug>
        "reproject" => {
            crate::require_git_data_dir()?;
            let (owner, slug) = (cli(2), cli(3));
            let Some((bare, id)) = git::repo::ensure_repo(pool, &owner, &slug).await? else {
                return Err(format!("list {owner}/{slug} not found").into());
            };
            match git::project::project_pushed_commit(pool, id, &bare).await? {
                Some(v) => println!("reproject {owner}/{slug}: created version v{v}"),
                // Оба случая называем: list.json может РАЗОБРАТЬСЯ и не иметь шагов, и тогда
                // «нет валидного list.json» отправило бы починку не туда.
                None => println!(
                    "reproject {owner}/{slug}: nothing to project (list.json is invalid or has no steps)"
                ),
            }
            Ok(())
        }
        // Осиротевшие репозитории: список удалён из БД, а его bare-репо осталось
        // на томе навсегда — с полной историей версий (находка F4 линзы 02).
        // Это и лишний диск, и содержимое, которое автор считает удалённым.
        //
        // РУЧНАЯ команда, а не автоматика: удаление данных обязано быть решением
        // человека. По умолчанию только ПОКАЗЫВАЕТ; сносит с `--apply`.
        //   gc-repos [--apply]
        "gc-repos" => {
            crate::require_git_data_dir()?;
            let apply = args.iter().any(|a| a == "--apply");
            let root = std::path::PathBuf::from(std::env::var("GIT_DATA_DIR")?);
            let ids: Vec<uuid::Uuid> = sqlx::query_scalar("select id from templates").fetch_all(pool).await?;
            // Fail-closed: пустая выборка почти наверняка значит «не та база», а не
            // «списков нет». Снести по такой выборке ВЕСЬ том нельзя.
            if ids.is_empty() {
                return Err("no lists in the database - refusing to treat every repo as orphaned".into());
            }
            let live: std::collections::HashSet<uuid::Uuid> = ids.into_iter().collect();
            let paths = git::repo::orphan_repo_dirs(&root, &live)?;
            let (mut orphans, mut bytes) = (0u64, 0u64);
            for path in paths {
                let size = git::bundle::repo_size_bytes(&path);
                orphans += 1;
                bytes += size;
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
                println!("  {} ({} KB){}", name, size / 1024, if apply { " - removing" } else { "" });
                if apply {
                    // Список мог родиться ПОКА мы обходили том: снимок живых id
                    // сделан до обхода, и по нему свежий репозиторий выглядит
                    // лишним. Перед сносом спрашиваем базу заново — версии из неё
                    // восстановимы, а принятые пуши, ветки и человеческие теги нет.
                    let stem = name.strip_suffix(".git").unwrap_or(&name);
                    let still_gone: Option<uuid::Uuid> =
                        sqlx::query_scalar("select id from templates where id = $1::uuid")
                            .bind(stem)
                            .fetch_optional(pool)
                            .await?;
                    if still_gone.is_some() {
                        println!("    {name}: list appeared during the sweep - leaving it alone");
                        orphans -= 1;
                        bytes -= size;
                        continue;
                    }
                    std::fs::remove_dir_all(&path)?;
                }
            }
            println!(
                "gc-repos: orphan repos {orphans}, {} KB{}",
                bytes / 1024,
                if apply { " - removed" } else { " (dry run; to delete: gc-repos --apply)" }
            );
            Ok(())
        }
        // Одноразовый догон после снятия ленивой досыпки (Ф1) и общий
        // инструмент выравнивания: каждому списку — репо, синхронное с БД.
        // Идемпотентен, безопасен к повторному запуску. Конфликты (посторонний
        // тег vN и т.п.) только печатает — чинить руками по runbook.
        //   sync-repos
        "sync-repos" => {
            crate::require_git_data_dir()?;
            let rows: Vec<(uuid::Uuid, String, String)> = sqlx::query_as(
                "select t.id, u.handle, t.slug from templates t join users u on u.id = t.owner_id \
                 order by u.handle, t.slug",
            )
            .fetch_all(pool)
            .await?;
            let total = rows.len();
            let (mut in_sync, mut boot, mut appended, mut projected, mut conflicts) = (0, 0, 0, 0, 0);
            let (mut restored, mut unversioned) = (0, 0);
            for (id, handle, slug) in rows {
                let bare = git::repo::repo_path(id);
                let _guard = git::repo::repo_guard(pool, id).await?;
                match git::version::sync_repo_with_db(pool, id, &bare).await {
                    Ok(git::version::SyncOutcome::InSync) => in_sync += 1,
                    Ok(git::version::SyncOutcome::Bootstrapped { versions }) => {
                        boot += 1;
                        println!("  {handle}/{slug}: repo created ({versions} versions)");
                    }
                    Ok(git::version::SyncOutcome::MainRestored { version }) => {
                        restored += 1;
                        println!("  {handle}/{slug}: main was missing, restored from tag v{version}");
                    }
                    Ok(git::version::SyncOutcome::Appended { from, to }) => {
                        appended += 1;
                        println!("  {handle}/{slug}: caught up v{from}..v{to}");
                    }
                    Ok(git::version::SyncOutcome::ProjectedTip { version }) => {
                        projected += 1;
                        println!("  {handle}/{slug}: tip projected into db -> v{version}");
                    }
                    Ok(git::version::SyncOutcome::TipNotVersioned { current }) => {
                        unversioned += 1;
                        println!(
                            "  {handle}/{slug}: main is ahead of v{current} but its tip has no \
                             readable list.json - no action needed, but the canon does not parse"
                        );
                    }
                    Ok(git::version::SyncOutcome::Conflict { have, current }) => {
                        conflicts += 1;
                        println!(
                            "  WARN {handle}/{slug}: CONFLICT git v{have} vs db v{current} - needs hands, \
                             see runbook git-projection-catchup"
                        );
                    }
                    Err(e) => {
                        conflicts += 1;
                        println!("  WARN {handle}/{slug}: error - {e}");
                    }
                }
            }
            println!(
                "sync-repos: total {total}; in sync {in_sync}, created {boot}, main restored {restored}, \
                 caught up {appended}, projected {projected}, tip not versioned {unversioned}, \
                 conflicts {conflicts}"
            );
            if conflicts > 0 {
                return Err(format!("{conflicts} repos need manual intervention").into());
            }
            Ok(())
        }
        // Сюда не попадаем: `run` выше отсеяла всё, чего нет в `КОМАНДЫ`.
        other => unreachable!("{other:?} is listed in the command table but has no arm"),
    }
}

#[cfg(test)]
mod cli_list_tests {
    /// Список команд и ветки `match` обязаны совпадать.
    ///
    /// Дублирование здесь вынужденное (см. `КОМАНДЫ`), поэтому оно закрыто сверкой, а
    /// не обещанием: команда, добавленная в `match` и забытая в списке, молча уехала бы
    /// в запуск СЕРВЕРА — то есть выглядела бы как «ядро не поняло и просто стартовало».
    #[test]
    fn список_команд_совпадает_с_ветками() {
        let src = include_str!("cli.rs");
        let список: Vec<&str> = super::КОМАНДЫ.to_vec();
        let mut в_match: Vec<String> = Vec::new();
        for l in src.lines() {
            let s = l.trim();
            if let Some(rest) = s.strip_suffix(" => {")
                && rest.starts_with('"')
            {
                for имя in rest.split('|') {
                    в_match.push(имя.trim().trim_matches('"').to_string());
                }
            }
        }
        let mut a: Vec<String> = список.iter().map(|s| s.to_string()).collect();
        a.sort();
        в_match.sort();
        в_match.dedup();
        assert_eq!(
            a, в_match,
            "КОМАНДЫ и ветки match разошлись: команда, забытая в списке, молча запустит СЕРВЕР"
        );
    }
}
