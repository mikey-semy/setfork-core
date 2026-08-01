//! GitCore — gRPC-сервис поверх git-подсистемы: smart-HTTP (clone/push),
//! ветки/теги/merge, bundle. Обслуживает proto/git.proto (setfork.git.v1).
use sqlx::postgres::PgPool;
use std::path::PathBuf;
use tonic::{Code, Request, Response, Status};

use crate::reason::{self, Reason};
use uuid::Uuid;

use super::util::{db_status, internal};
use crate::db;
use crate::git::bundle::{SerStep, StepRef, VersionData};
use crate::git::update::{MainUpdateError, update_main};
use crate::git::{MAIN_REF, bundle, history, project, repo, serialize, smart_http, write};
use crate::pb::git_core_server::GitCore;
use crate::pb::{
    Branch, BranchOpResponse, BranchSnapshotRequest, BranchSnapshotResponse, BranchesResponse, BytesResponse,
    Commit, CommitToBranchRequest, CommitToBranchResponse, CommitsResponse, CreateBranchRequest,
    CreateTagRequest, DeleteBranchRequest, InfoRefsRequest, ListCommitsRequest, ListContent,
    MergeBranchRequest, MergeBranchResponse, MergeResolvedRequest, MergeStateRequest, MergeStateResponse,
    MirrorPushResponse, PostRequest, ReceivePackResponse, RepoRef, SnapshotRef, SnapshotStep, Tag,
    TagsResponse, UpdateBranchRequest, UpdateBranchResponse,
};

// Только простые имена веток — никаких путей/точек (защита от ref-инъекций).
//
// Зеркало `badBranch` на фронте (features/git/core.inproc.ts): форма обязана быть
// одной с обеих сторон контракта. Две прежние расхождения (линза 02, F5):
//   * `is_alphanumeric()` юникодный — сюда проходили кириллица и прочие алфавиты,
//     которые ASCII-регэксп фронта отвергает;
//   * ведущий '-' не запрещался. Само ядро создаёт ветки через API git2, но имя,
//     заведённое здесь, дальше попадает в `execFile('git', […])` на inproc-пути
//     фронта, где '-D' — уже флаг, а не имя (security-скан 2026-07-23, F3).
// Правило дублируется по обе стороны сознательно — значит и меняться обязано парой.
fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.contains("..")
}

// ТЕГИ — не ветки. Имя тега никуда не уходит в аргументы `git` (create_tag работает
// через git2::tag_lightweight), поэтому ASCII-ограничение ветвей здесь неуместно: под
// него не проходят живые релизные имена вроде «релиз-1» (P1 авто-ревью core #56 —
// парити с фронтом сломала бы уже созданные релизы).
//
// Что остаётся запрещённым — то, от чего ломается сам ref: пустое имя, ведущий '-',
// '..', пробелы и служебные символы git-refspec (~^:?*[\), завершающая точка и '/'.
fn valid_tag(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && !name.chars().any(|c| {
            // '\u{5c}' = обратный слэш (git-check-ref-format его запрещает).
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | ']' | '\u{5c}')
        })
}

/// Имя `v<число>` ЗАРЕЗЕРВИРОВАНО за версиями: их теги ставит система (bundle.rs,
/// project.rs), и по ним же считается «докуда версии уже записаны в git»
/// (bundle::max_tag_version → repo::ensure_repo).
///
/// Без резервирования релиз с именем «v20» на списке с 8 версиями поднимал бы
/// максимум до 20 — и новые версии переставали доезжать в git ВООБЩЕ, молча.
/// А релиз «v2» просто перевешивал существующий тег версии (create_tag ставит с
/// force), после чего история версий врёт.
///
/// Дробные и составные имена («v1.0», «v2-beta») версиями не считаются и остаются
/// доступны человеку — под запрет попадает ровно то, что парсит max_tag_version.
fn is_version_tag(name: &str) -> bool {
    name.strip_prefix('v').is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// GitCore: git-операции (smart-HTTP, ветки/теги/merge, bundle) поверх общего пула.
pub struct GitCoreSvc {
    pub pool: PgPool,
}

impl GitCoreSvc {
    // Резолв списка + загрузка всей истории версий (общее для всех RPC;
    // pub — используется golden-CLI в бинаре, см. main.rs).
    pub async fn load(&self, owner: &str, slug: &str) -> Result<Vec<VersionData>, Status> {
        let (id, _v) = db::resolve_list(&self.pool, owner, slug)
            .await
            .map_err(db_status)?
            .ok_or_else(|| reason::status(Code::NotFound, Reason::NotFound, "list not found"))?;
        db::load_bundle_data(&self.pool, id).await.map_err(db_status)
    }

    // Общая реализация bundle: загрузка версий → материализация → git bundle.
    // pub — используется golden-CLI в бинаре (main.rs).
    pub async fn build(&self, owner: &str, slug: &str) -> Result<Vec<u8>, Status> {
        let versions = self.load(owner, slug).await?;
        tokio::task::spawn_blocking(move || bundle::build_bundle(&versions))
            .await
            .map_err(internal)?
            .map_err(internal)
    }

    // Персистентный bare-репо (bootstrap/append под локом) — общий вход read/write RPC.
    async fn ensure(&self, owner: &str, slug: &str) -> Result<(PathBuf, Uuid), Status> {
        repo::ensure_repo(&self.pool, owner, slug)
            .await
            .map_err(db_status)?
            .ok_or_else(|| reason::status(Code::NotFound, Reason::NotFound, "list not found"))
    }
}

// Открывает bare-репо в spawn_blocking (git2-объекты не Send) и выполняет `f`;
// JoinError и ошибка открытия схлопываются в один internal-Status.
async fn with_repo<T, F>(bare: PathBuf, f: F) -> Result<T, Status>
where
    T: Send + 'static,
    F: FnOnce(&git2::Repository) -> Result<T, Status> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let repo = git2::Repository::open_bare(&bare).map_err(internal)?;
        f(&repo)
    })
    .await
    .map_err(internal)?
}

/**
 * Сообщение squash-коммита с трейлерами `Co-authored-by`.
 *
 * При squash история ветки в main не попадает, поэтому авторство её коммитов
 * иначе исчезло бы совсем — а это единственная запись о том, кто на самом деле
 * делал работу. GitHub решает ровно так же.
 *
 * Авторы берутся из коммитов ВКЛАДА ветки (то, чего нет в main), без дублей и в
 * порядке появления. Ошибку обхода глушим: слияние не должно падать из-за
 * украшения сообщения.
 */
pub fn with_coauthors(
    repo: &git2::Repository,
    branch_tip: git2::Oid,
    main_tip: git2::Oid,
    title: &str,
) -> String {
    let mut seen: Vec<String> = Vec::new();
    if let Ok(mut walk) = repo.revwalk() {
        let _ = walk.push(branch_tip);
        let _ = walk.hide(main_tip);
        for oid in walk.flatten() {
            let Ok(c) = repo.find_commit(oid) else { continue };
            let a = c.author();
            // Подпись может быть не-UTF8 — такую пропускаем, а не падаем.
            let (Ok(n), Ok(e)) = (a.name(), a.email()) else { continue };
            let (n, e) = (n.trim().to_string(), e.trim().to_string());
            // Пустое имя или почта дали бы ломаную строку «Co-authored-by:  <>»,
            // которую git трейлером не считает, а человек читает как мусор.
            if n.is_empty() || e.is_empty() {
                continue;
            }
            // Служебная подпись самого сервиса соавторством не является.
            // Регистр не важен: почта регистронезависима, и «GIT@SetFork.com»
            // — та же служебная подпись, а не соавтор.
            if e.eq_ignore_ascii_case(bundle::AUTHOR_EMAIL) {
                continue;
            }
            let line = format!("Co-authored-by: {n} <{e}>");
            if !seen.contains(&line) {
                seen.push(line);
            }
        }
    }
    if seen.is_empty() {
        return title.to_string();
    }
    // Пустая строка перед трейлерами обязательна: иначе git не считает их
    // трейлерами, и `git interpret-trailers` их не увидит.
    format!(
        "{title}

{}",
        seen.join(
            "
"
        )
    )
}

// Подпись merge-коммитов — та же идентичность, что у детерминированных коммитов bundle.
fn merge_sig() -> Result<git2::Signature<'static>, Status> {
    git2::Signature::now(bundle::AUTHOR_NAME, bundle::AUTHOR_EMAIL).map_err(internal)
}

/// Отказ единой точки обновления main → gRPC-статус.
///
/// MissingListJson — единственный «пользовательский» случай (дерево слияния без
/// канона); Stale — конкурентная запись (под репо-локом почти невозможна, но
/// CAS честный); NonFastForward под локом означает сломанный инвариант кода.
fn main_status(e: MainUpdateError) -> Status {
    match e {
        MainUpdateError::MissingListJson => reason::status(
            Code::FailedPrecondition,
            Reason::MissingListJson,
            "list.json is required at the repo root",
        ),
        // Как и MissingListJson — «пользовательский» случай: дерево собрано не по
        // формату. Текст несёт сам путь, иначе отказ нечего показать человеку.
        // По-английски — как ВСЕ остальные Status в ядре: их читает фронт и
        // переводит сам; язык пользователя ядру неизвестен.
        MainUpdateError::ForeignPath(p) => reason::status(
            Code::FailedPrecondition,
            Reason::ForeignPath,
            format!(
                "only README.md, list.json and .gitattributes are allowed in the list tree; foreign path: {p}"
            ),
        ),
        MainUpdateError::Stale => reason::status(Code::Aborted, Reason::Stale, "main moved concurrently"),
        MainUpdateError::NonFastForward => {
            Status::internal("non-fast-forward update of main (invariant breach)")
        }
        MainUpdateError::Git(e) => Status::internal(e),
    }
}

/// Текущий oid main (или None, если ветки ещё нет) — для проверки «push сдвинул main».
fn main_oid(bare: &std::path::Path) -> Option<String> {
    git2::Repository::open_bare(bare).ok()?.refname_to_id(MAIN_REF).ok().map(|o| o.to_string())
}

// Пустая proto-строка → None (proto3 не отличает '' от отсутствия поля).
fn opt(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// Пуш зеркала СЕЙЧАС (Ф3): читает настройки, расшифровывает токен, пушит,
/// записывает статус. «Не настроено» — не ошибка (Ok). Любой сбой — в статус
/// списка (mirror_error) и метрику: молчаливой деградации быть не должно.
pub(crate) async fn push_mirror_now(pool: &PgPool, id: Uuid, bare: &std::path::Path) -> Result<(), String> {
    let Some((url, token_enc)) = db::load_mirror(pool, id).await.map_err(|e| e.to_string())? else {
        return Ok(()); // зеркало не настроено
    };
    let outcome = match crate::git::mirror::mirror_secret() {
        None => {
            Err("SETFORK_MIRROR_SECRET не задан на сервере — зеркало не может расшифровать токен".to_string())
        }
        Some(secret) => match crate::git::mirror::decrypt_token(&token_enc, secret) {
            None => {
                Err("токен зеркала не расшифровался (секрет сменён?) — сохраните токен заново".to_string())
            }
            Some(token) => crate::git::mirror::mirror_push(bare, &url, &token).await,
        },
    };
    match &outcome {
        Ok(()) => {
            metrics::counter!("mirror_push_total", "result" => "ok").increment(1);
            tracing::info!(%id, url, "зеркало обновлено");
        }
        Err(e) => {
            metrics::counter!("mirror_push_total", "result" => "error").increment(1);
            tracing::warn!(%id, url, error = %e, "пуш зеркала не удался");
        }
    }
    if let Err(e) = db::record_mirror_result(pool, id, outcome.as_ref().err().map(|s| s.as_str())).await {
        tracing::error!(%id, error = %e, "статус зеркала не записан");
    }
    outcome
}

/// Фоновый пуш зеркала после записи в main (fire-and-forget: запись не ждёт
/// сети; исход виден в статусе настроек и метрике).
pub(crate) fn spawn_mirror(pool: PgPool, id: Uuid, bare: std::path::PathBuf) {
    tokio::spawn(async move {
        let _ = push_mirror_now(&pool, id, &bare).await;
    });
}

/// Проекция нового main-tip → версия БД, с одним повтором на транзиентный сбой.
///
/// Порядок канона: git уже записан и НЕ откатывается — БД догоняет. Поэтому сбой
/// проекции не отменяет git-операцию, но обязан быть громким: метрика + ERROR-лог;
/// восстановление — штатный rebuild проекции `reproject <owner> <slug>` (CLI).
/// «Спроецировать нечего» (битый/пустой list.json) — тот же исход, что и сбой:
/// git принят, версии нет; без метрики случай проходил мимо алерта (линза 02, F3).
async fn project_main_or_log(
    pool: &PgPool,
    id: Uuid,
    bare: &std::path::Path,
    op: &'static str,
    owner: &str,
    slug: &str,
) -> i32 {
    let outcome = match project::project_pushed_commit(pool, id, bare).await {
        Err(e) => {
            tracing::warn!(owner, slug, %id, error = %e, op, "сбой проекции — повтор через 200мс");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            project::project_pushed_commit(pool, id, bare).await
        }
        ok => ok,
    };
    match outcome {
        Ok(Some(v)) => v,
        Ok(None) => {
            metrics::counter!("projection_failures_total", "op" => format!("{op}_empty")).increment(1);
            tracing::warn!(
                owner, slug, %id, op,
                "проекция ничего не создала (list.json не разобран или без steps) — версия НЕ создана"
            );
            0
        }
        Err(e) => {
            metrics::counter!("projection_failures_total", "op" => op).increment(1);
            tracing::error!(
                owner, slug, %id, error = %e, op,
                "ОШИБКА проекции — git принят, версия НЕ создана; восстановление: reproject"
            );
            0
        }
    }
}

// BranchSnapshotData -> pb-снапшот (переиспользуется snapshot-RPC и merge-state).
fn to_snapshot_pb(sn: project::BranchSnapshotData) -> BranchSnapshotResponse {
    BranchSnapshotResponse {
        found: true,
        tip_sha: sn.tip,
        title: sn.title,
        desc: sn.desc,
        tags: sn.tags,
        ordered: sn.ordered,
        steps: sn
            .steps
            .iter()
            .enumerate()
            .map(|(i, st)| SnapshotStep {
                n: (i as i32) + 1,
                title: st.title.clone(),
                desc: st.desc.clone(),
                command: st.command.clone(),
                level: st.level.clone(),
                why: st.why.clone(),
                section: st.section.clone(),
                subtasks: st.subtasks.clone(),
                refs: st
                    .refs
                    .iter()
                    .map(|r| SnapshotRef { label: r.label.clone(), url: r.url.clone().unwrap_or_default() })
                    .collect(),
                r#type: st.block_type.clone(),
                content_json: if st.block_type.is_empty() { String::new() } else { st.content.to_string() },
                // Идентичность блока — сквозь провод: без неё дифф ветки читает
                // переименование как «удалён + добавлен» (ADR-0013).
                block_id: st.block_id.clone().unwrap_or_default(),
            })
            .collect(),
    }
}

/// Провод → домен сериализации: структура версии от клиента становится тем же
/// `VersionData`, из которого материализуются коммиты версий. Один тип на оба
/// пути записи — поэтому канон не может разойтись между «пуш» и «правка в ветке».
fn from_list_content(c: ListContent) -> VersionData {
    VersionData {
        version: c.version,
        note: String::new(), // сообщение коммита приходит отдельным полем запроса
        ts: 0,               // ветка коммитится «сейчас», дата берётся не отсюда
        title: c.title,
        desc: c.desc,
        tags: c.tags,
        ordered: c.ordered,
        // Провод kind не несёт: тип списка — свойство templates, канон получает
        // его из БД (canon_list_json), иначе каждая веточная запись стирала бы
        // kind из дерева (ловушка №1 разведки Ф2a).
        kind: None,
        steps: c
            .steps
            .into_iter()
            .map(|s| SerStep {
                n: s.n,
                // Провод не отличает '' от отсутствия: пустой type = шаг (blocks::is_step_type).
                block_type: if crate::blocks::is_step_type(&s.r#type) {
                    None
                } else {
                    Some(s.r#type.clone())
                },
                content: crate::blocks::content_value(&s.r#type, &s.content_json),
                block_id: Some(s.block_id).filter(|v| !v.trim().is_empty()),
                title: s.title,
                desc: s.desc,
                command: s.command,
                // Провод SnapshotStep этих полей не несёт: canon_list_json обогащает
                // их из текущей версии по block_id (иначе веточная запись стирала бы
                // картинку/пометку из канона — та же ловушка, что была с kind).
                image_key: None,
                level: s.level,
                needs_human: false,
                needs_human_ask: None,
                why: s.why,
                section: s.section,
                subtasks: s.subtasks,
                refs: s
                    .refs
                    .into_iter()
                    .map(|r| StepRef { label: r.label, url: Some(r.url).filter(|u| !u.is_empty()) })
                    .collect(),
            })
            .collect(),
    }
}

/// Коммит ручного резолва: дерево main с заменённым `list.json` и БЕЗ `steps/`
/// (md-оверрайды сбрасываются — канон разрешённых шагов один, см. proto).
///
/// `squash` определяет РОДИТЕЛЕЙ, а не дерево: дерево здесь всегда одно — то,
/// что человек разрешил руками. При squash родитель один (main), и история
/// ветки в main не уезжает; вклад авторов сохраняется трейлерами, как в
/// merge_branch. Раньше режима не было вовсе, и фронт на remote-пути просто
/// ОТКАЗЫВАЛ в резолве squash-списков, выдавая отказ за 'conflict'.
///
/// Вынесено из RPC отдельной функцией, чтобы поведение проверялось тестами на
/// настоящем git-репо, без Postgres и транспорта.
fn commit_resolved(
    repo: &git2::Repository,
    branch: &str,
    list_json: &[u8],
    squash: bool,
    message: &str,
) -> Result<String, Status> {
    let branch_tip = repo
        .refname_to_id(&format!("refs/heads/{branch}"))
        .map_err(|_| reason::status(Code::NotFound, Reason::NotFound, "branch not found"))?;
    let main_tip = repo.refname_to_id(MAIN_REF).map_err(internal)?;
    if main_tip == branch_tip {
        return Err(reason::status(
            Code::FailedPrecondition,
            Reason::NothingToMerge,
            "branch is not ahead of base",
        ));
    }
    let ours = repo.find_commit(main_tip).map_err(internal)?;
    let theirs = repo.find_commit(branch_tip).map_err(internal)?;
    let blob = repo.blob(list_json).map_err(internal)?;
    let mut tb = repo.treebuilder(Some(&ours.tree().map_err(internal)?)).map_err(internal)?;
    tb.insert("list.json", blob, 0o100644).map_err(internal)?;
    // Витрина соответствует канону (Ф2b): README перегенерируется из нового
    // list.json — раньше он тащился старым блобом и протухал до веб-версии.
    if let Some(readme) = project::readme_from_canon(list_json) {
        let rb = repo.blob(readme.as_bytes()).map_err(internal)?;
        tb.insert("README.md", rb, 0o100644).map_err(internal)?;
    }
    let ab = repo.blob(serialize::GITATTRIBUTES.as_bytes()).map_err(internal)?;
    tb.insert(".gitattributes", ab, 0o100644).map_err(internal)?;
    if tb.get("steps").map_err(internal)?.is_some() {
        tb.remove("steps").map_err(internal)?;
    }
    let tree_id = tb.write().map_err(internal)?;
    let tree = repo.find_tree(tree_id).map_err(internal)?;
    let sig = merge_sig()?;

    let (msg, parents): (String, Vec<&git2::Commit>) = if squash {
        let title = if message.trim().is_empty() {
            format!("Squashed branch '{branch}'")
        } else {
            message.trim().to_string()
        };
        (with_coauthors(repo, branch_tip, main_tip, &title), vec![&ours])
    } else {
        (format!("Merge branch '{branch}' (resolved)"), vec![&ours, &theirs])
    };
    // Коммит без ref-обновления; main двигает ТОЛЬКО update_main (валидация как у pre-receive).
    let merged = repo.commit(None, &sig, &sig, &msg, &tree, &parents).map_err(internal)?;
    update_main(repo, merged, Some(main_tip), &format!("merge {branch}: resolved")).map_err(main_status)?;
    Ok(merged.to_string())
}

/// Канонические байты list.json для записи в ветку — ВСЕГДА собираются здесь,
/// из присланной структуры. Прислать готовый файл больше нельзя: поле `list_json`
/// снято из контракта (`reserved`), потому что оно требовало от клиента знать
/// правила формата, а значит держать вторую его реализацию.
fn canon_list_json(
    content: Option<ListContent>,
    kind: Option<String>,
    carry: &std::collections::HashMap<Uuid, db::CarryOver>,
) -> Result<Vec<u8>, Status> {
    let c = content.ok_or_else(|| Status::invalid_argument("content required"))?;
    let mut v = from_list_content(c);
    v.kind = kind.filter(|k| serialize::is_valid_kind(k));
    // Ф2a-довесок: провод не несёт картинку/пометку — обогащаем из текущей версии
    // по идентичности блока, иначе веточная запись стирала бы их из канона
    // (та же ловушка, что была с kind).
    for s in &mut v.steps {
        let Some(bid) = s.block_id.as_deref().and_then(|b| Uuid::parse_str(b).ok()) else { continue };
        let Some(co) = carry.get(&bid) else { continue };
        s.image_key = co.image_key.clone();
        s.needs_human = co.needs_human;
        s.needs_human_ask = Some(db::loc(&co.needs_human_ask)).filter(|a| !a.is_empty() && co.needs_human);
    }
    Ok(serialize::list_json(&v).into_bytes())
}

#[tonic::async_trait]
impl GitCore for GitCoreSvc {
    async fn info_refs_upload_pack(
        &self,
        req: Request<InfoRefsRequest>,
    ) -> Result<Response<BytesResponse>, Status> {
        let InfoRefsRequest { repo, git_protocol } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let data =
            tokio::task::spawn_blocking(move || smart_http::upload_pack_advertise(&bare, opt(&git_protocol)))
                .await
                .map_err(internal)?
                .map_err(internal)?;
        Ok(Response::new(BytesResponse { data }))
    }
    async fn info_refs_receive_pack(
        &self,
        req: Request<InfoRefsRequest>,
    ) -> Result<Response<BytesResponse>, Status> {
        let InfoRefsRequest { repo, git_protocol } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let data = tokio::task::spawn_blocking(move || {
            smart_http::receive_pack_advertise(&bare, opt(&git_protocol))
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;
        Ok(Response::new(BytesResponse { data }))
    }
    async fn upload_pack(&self, req: Request<PostRequest>) -> Result<Response<BytesResponse>, Status> {
        let PostRequest { repo, body, git_protocol, .. } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let data = tokio::task::spawn_blocking(move || {
            smart_http::upload_pack_rpc(&bare, &body, opt(&git_protocol))
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;
        Ok(Response::new(BytesResponse { data }))
    }
    async fn receive_pack(&self, req: Request<PostRequest>) -> Result<Response<ReceivePackResponse>, Status> {
        let PostRequest { repo, body, git_protocol, lang, actor_handle } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Критическая секция: receive-pack + проекция под одним локом репо
        // (ленивый append не вклинивается между приёмом и проекцией).
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        // Порог размера репо (Ф0). Per-push потолок (receive.maxInputSize) не мешает
        // вырастить репозиторий серией мелких пушей, а квоты размера у git нет вовсе —
        // это уровень приложения (у GitLab так же: «Repository size limit» —
        // настройка приложения, пуш при превышении отклоняется). Проверяем ДО приёма:
        // принять и отказать потом значило бы оставить объекты на диске ровно в том
        // случае, ради которого порог и вводился.
        let limit = crate::config::repo_limit_bytes();
        if limit > 0 {
            let bare_size = bare.clone();
            let size = tokio::task::spawn_blocking(move || bundle::repo_size_bytes(&bare_size))
                .await
                .map_err(internal)?;
            metrics::gauge!("repo_bytes").set(size as f64);
            if size > limit {
                // По-английски — как все Status в ядре (перевод за фронтом).
                return Err(reason::status(
                    Code::ResourceExhausted,
                    Reason::RepoTooLarge,
                    format!(
                        "list repository is {} MB, over the {} MB limit; pushes are stopped",
                        size / (1024 * 1024),
                        limit / (1024 * 1024)
                    ),
                ));
            }
        }
        let bare_recv = bare.clone();
        // Внутри одного spawn_blocking: oid main до и после приёма пака — чтобы
        // проецировать версию ТОЛЬКО когда push реально сдвинул main. Пуш в
        // ветку-черновик main не двигает → иначе плодились бы дубли версий.
        let actor = actor_handle.clone();
        let (data, moved, magic) = tokio::task::spawn_blocking(
            move || -> std::io::Result<(Vec<u8>, bool, Vec<crate::git::magic::MagicPush>)> {
                let before = main_oid(&bare_recv);
                // Снимок магических рефов ДО приёма: без него чужой брошенный
                // refs/for/* присвоился бы текущему пушащему (авто-ревью, P1).
                let magic_before = match git2::Repository::open_bare(&bare_recv) {
                    Ok(r) => crate::git::magic::snapshot(&r).unwrap_or_default(),
                    Err(e) => return Err(std::io::Error::other(e.to_string())),
                };
                let data = smart_http::receive_pack_rpc(
                    &bare_recv,
                    &body,
                    opt(&git_protocol),
                    opt(&lang),
                    opt(&actor),
                )?;
                let after = main_oid(&bare_recv);
                // Ф4: магические рефы разбираем ВНУТРИ той же критической секции,
                // что и приём — между ними не должно вклиниться чужое чтение
                // рефов, иначе кто-то увидит refs/for/* как настоящую ветку.
                let magic = match git2::Repository::open_bare(&bare_recv) {
                    Ok(r) => crate::git::magic::take_magic_pushes(&r, &actor, &magic_before)
                        .map_err(|e| std::io::Error::other(e.to_string()))?,
                    Err(e) => return Err(std::io::Error::other(e.to_string())),
                };
                Ok((data, after.is_some() && after != before, magic))
            },
        )
        .await
        .map_err(internal)?
        .map_err(internal)?;
        // Проекция list.json нового main tip → новая версия (0 = не спроецировано).
        // Сбой проекции НЕ отменяет push (git-объекты целы) — тихая потеря версии
        // худший исход (аудит 2026-07-20, P0-1); громкость — в project_main_or_log.
        let new_version = if moved {
            let v = project_main_or_log(&self.pool, id, &bare, "push", &repo.owner, &repo.slug).await;
            spawn_mirror(self.pool.clone(), id, bare.clone()); // Ф3: зеркало догоняет истину
            v
        } else {
            0
        };
        Ok(Response::new(ReceivePackResponse {
            data,
            new_version,
            magic: magic
                .into_iter()
                .map(|m| crate::pb::MagicPush { base: m.base, branch: m.branch, tip_sha: m.tip_sha })
                .collect(),
        }))
    }
    async fn create_bundle(&self, req: Request<RepoRef>) -> Result<Response<BytesResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        // Персистентный репо (как TS bundleRepo) — bundle включает запушенные коммиты.
        let data = repo::bundle_repo(&self.pool, &owner, &slug)
            .await
            .map_err(db_status)?
            .ok_or_else(|| reason::status(Code::NotFound, Reason::NotFound, "list not found"))?;
        Ok(Response::new(BytesResponse { data }))
    }

    async fn list_branches(&self, req: Request<RepoRef>) -> Result<Response<BranchesResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        let (bare, _id) = self.ensure(&owner, &slug).await?;
        // git2-объекты не Send → всё в with_repo (spawn_blocking), наружу только owned-данные.
        let branches = with_repo(bare, |repo| {
            let main_tip = repo.refname_to_id(MAIN_REF).map_err(internal)?;
            let mut out: Vec<Branch> = Vec::new();
            for b in repo.branches(Some(git2::BranchType::Local)).map_err(internal)? {
                let (branch, _) = b.map_err(internal)?;
                let name = branch.name().ok().flatten().unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                let tip = match branch.get().target() {
                    Some(t) => t,
                    None => continue,
                };
                let (ahead, behind) = if name == "main" {
                    (0, 0)
                } else {
                    repo.graph_ahead_behind(tip, main_tip).unwrap_or((0, 0))
                };
                out.push(Branch {
                    name: name.clone(),
                    tip_sha: tip.to_string(),
                    is_default: name == "main",
                    ahead: ahead as i32,
                    behind: behind as i32,
                });
            }
            // main первым, остальные по имени.
            out.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name)));
            Ok(out)
        })
        .await?;
        Ok(Response::new(BranchesResponse { branches }))
    }

    async fn get_branch_snapshot(
        &self,
        req: Request<BranchSnapshotRequest>,
    ) -> Result<Response<BranchSnapshotResponse>, Status> {
        let BranchSnapshotRequest { repo, branch } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        if !valid_branch(&branch) {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let refname = format!("refs/heads/{branch}");
        let snap = tokio::task::spawn_blocking(move || project::branch_snapshot(&bare, &refname))
            .await
            .map_err(internal)?;
        let Some(sn) = snap else {
            return Ok(Response::new(BranchSnapshotResponse { found: false, ..Default::default() }));
        };
        Ok(Response::new(to_snapshot_pb(sn)))
    }

    async fn create_branch(
        &self,
        req: Request<CreateBranchRequest>,
    ) -> Result<Response<BranchOpResponse>, Status> {
        let CreateBranchRequest { repo, name, from } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        let from = if from.is_empty() { "main".to_string() } else { from };
        if !valid_branch(&name) || !valid_branch(&from) {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let tip = with_repo(bare, move |repo| {
            let base = repo
                .refname_to_id(&format!("refs/heads/{from}"))
                .map_err(|_| reason::status(Code::NotFound, Reason::NotFound, "base branch not found"))?;
            let commit = repo.find_commit(base).map_err(internal)?;
            // force=false: существующая ветка → ошибка (already_exists наружу).
            // .map(|_| ()) сразу дропает Branch<'_> (заимствует repo).
            match repo.branch(&name, &commit, false).map(|_| ()) {
                Ok(()) => Ok(base.to_string()),
                Err(e) if e.code() == git2::ErrorCode::Exists => {
                    Err(reason::status(Code::AlreadyExists, Reason::Exists, "branch exists"))
                }
                Err(e) => Err(internal(e)),
            }
        })
        .await?;
        Ok(Response::new(BranchOpResponse { tip_sha: tip }))
    }

    async fn delete_branch(
        &self,
        req: Request<DeleteBranchRequest>,
    ) -> Result<Response<BranchOpResponse>, Status> {
        let DeleteBranchRequest { repo, name } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        if !valid_branch(&name) {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        if name == "main" {
            return Err(reason::status(Code::FailedPrecondition, Reason::Protected, "main is protected"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        with_repo(bare, move |repo| {
            let mut branch = repo
                .find_branch(&name, git2::BranchType::Local)
                .map_err(|_| reason::status(Code::NotFound, Reason::NotFound, "branch not found"))?;
            branch.delete().map_err(internal)
        })
        .await?;
        Ok(Response::new(BranchOpResponse { tip_sha: String::new() }))
    }

    async fn merge_branch(
        &self,
        req: Request<MergeBranchRequest>,
    ) -> Result<Response<MergeBranchResponse>, Status> {
        let MergeBranchRequest { repo, name, mode, message } = req.into_inner();
        let squash = mode == "squash";
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        if !valid_branch(&name) || name == "main" {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Merge двигает main → критическая секция с проекцией (как receive_pack).
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let (tip, ff) = with_repo(bare.clone(), move |repo| {
            let branch_tip = repo
                .refname_to_id(&format!("refs/heads/{name}"))
                .map_err(|_| reason::status(Code::NotFound, Reason::NotFound, "branch not found"))?;
            let main_tip = repo.refname_to_id(MAIN_REF).map_err(internal)?;
            let (ahead, _behind) = repo.graph_ahead_behind(branch_tip, main_tip).map_err(internal)?;
            if ahead == 0 {
                return Err(reason::status(
                    Code::FailedPrecondition,
                    Reason::NothingToMerge,
                    "branch is not ahead of base",
                ));
            }
            // SQUASH: один коммит с ОДНИМ родителем (main). История ветки в main
            // не уезжает — это и есть смысл режима. Вклад авторов не теряется:
            // он переносится трейлерами Co-authored-by, как это делает GitHub.
            //
            // Проверка идёт ДО fast-forward: при squash даже перематываемую ветку
            // сплющиваем, иначе выбор режима работал бы через раз — в зависимости
            // от того, ушёл ли main вперёд.
            if squash {
                let ours = repo.find_commit(main_tip).map_err(internal)?;
                let theirs = repo.find_commit(branch_tip).map_err(internal)?;
                // Дерево берём merge-ом, а не деревом ветки: main мог уйти вперёд,
                // и дерево ветки откатило бы чужие изменения.
                let mut idx = repo.merge_commits(&ours, &theirs, None).map_err(internal)?;
                if idx.has_conflicts() {
                    return Err(reason::status(
                        Code::FailedPrecondition,
                        Reason::Conflict,
                        "merge does not apply cleanly",
                    ));
                }
                let tree_id = idx.write_tree_to(repo).map_err(internal)?;
                let tree = repo.find_tree(tree_id).map_err(internal)?;
                let title = if message.trim().is_empty() {
                    format!("Squashed branch '{name}'")
                } else {
                    message.trim().to_string()
                };
                let msg = with_coauthors(repo, branch_tip, main_tip, &title);
                let sig = merge_sig()?;
                let squashed = repo.commit(None, &sig, &sig, &msg, &tree, &[&ours]).map_err(internal)?;
                update_main(repo, squashed, Some(main_tip), &format!("merge {name}: squash"))
                    .map_err(main_status)?;
                return Ok((squashed.to_string(), false));
            }
            // main — предок ветки → fast-forward: двигаем ref (через единую точку).
            if repo.graph_descendant_of(branch_tip, main_tip).unwrap_or(false) {
                update_main(repo, branch_tip, Some(main_tip), &format!("merge {name}: fast-forward"))
                    .map_err(main_status)?;
                return Ok((branch_tip.to_string(), true));
            }
            // Расхождение → merge-commit; конфликт индекса = failed_precondition.
            let ours = repo.find_commit(main_tip).map_err(internal)?;
            let theirs = repo.find_commit(branch_tip).map_err(internal)?;
            let mut idx = repo.merge_commits(&ours, &theirs, None).map_err(internal)?;
            if idx.has_conflicts() {
                return Err(reason::status(
                    Code::FailedPrecondition,
                    Reason::Conflict,
                    "merge does not apply cleanly",
                ));
            }
            let tree_id = idx.write_tree_to(repo).map_err(internal)?;
            let tree = repo.find_tree(tree_id).map_err(internal)?;
            let sig = merge_sig()?;
            let msg = format!("Merge branch '{name}'");
            let merged = repo.commit(None, &sig, &sig, &msg, &tree, &[&ours, &theirs]).map_err(internal)?;
            update_main(repo, merged, Some(main_tip), &format!("merge {name}")).map_err(main_status)?;
            Ok((merged.to_string(), false))
        })
        .await?;
        // main сдвинулся → проекция новой версии (0 = list.json не изменился).
        // Сбой проекции не отменяет merge, но громок — см. project_main_or_log.
        let new_version = project_main_or_log(&self.pool, id, &bare, "merge", &repo.owner, &repo.slug).await;
        spawn_mirror(self.pool.clone(), id, bare.clone()); // Ф3
        Ok(Response::new(MergeBranchResponse { tip_sha: tip, new_version, fast_forward: ff }))
    }

    async fn get_merge_state(
        &self,
        req: Request<MergeStateRequest>,
    ) -> Result<Response<MergeStateResponse>, Status> {
        let MergeStateRequest { repo, branch } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Гейта записи здесь НЕТ намеренно: это чтение — три материализации для
        // сравнения. Смотреть на замороженный список можно, менять нельзя.
        if !valid_branch(&branch) || branch == "main" {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let state = tokio::task::spawn_blocking(move || -> Result<Option<(String, project::BranchSnapshotData, project::BranchSnapshotData, project::BranchSnapshotData)>, String> {
            let repo = git2::Repository::open_bare(&bare).map_err(|e| e.to_string())?;
            let Ok(branch_tip) = repo.refname_to_id(&format!("refs/heads/{branch}")) else {
                return Ok(None);
            };
            let main_tip = repo.refname_to_id(MAIN_REF).map_err(|e| e.to_string())?;
            let Ok(base_oid) = repo.merge_base(main_tip, branch_tip) else {
                return Ok(None);
            };
            drop(repo); // commit_snapshot открывает репо сам
            let base = project::commit_snapshot(&bare, &base_oid.to_string());
            let ours = project::branch_snapshot(&bare, MAIN_REF);
            let theirs = project::branch_snapshot(&bare, &format!("refs/heads/{branch}"));
            match (base, ours, theirs) {
                (Some(b), Some(o), Some(t)) => Ok(Some((base_oid.to_string(), b, o, t))),
                _ => Ok(None),
            }
        })
        .await
        .map_err(internal)?
        .map_err(Status::internal)?;
        let Some((base_sha, b, o, t)) = state else {
            return Ok(Response::new(MergeStateResponse { found: false, ..Default::default() }));
        };
        Ok(Response::new(MergeStateResponse {
            found: true,
            merge_base_sha: base_sha,
            base: Some(to_snapshot_pb(b)),
            ours: Some(to_snapshot_pb(o)),
            theirs: Some(to_snapshot_pb(t)),
        }))
    }

    async fn merge_resolved(
        &self,
        req: Request<MergeResolvedRequest>,
    ) -> Result<Response<MergeBranchResponse>, Status> {
        let MergeResolvedRequest { repo, branch, content, mode, message } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        if !valid_branch(&branch) || branch == "main" {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Канон собирает ядро: содержимое — из запроса, kind и надстройки блоков
        // (картинка/пометка, по идентичности) — из БД (Ф2a).
        let kind = db::load_list_kind(&self.pool, id).await.map_err(db_status)?;
        let carry = db::current_marks(&self.pool, id).await.map_err(db_status)?;
        let list_json = canon_list_json(content, kind, &carry)?;
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let tip = with_repo(bare.clone(), move |repo| {
            commit_resolved(repo, &branch, &list_json, mode == "squash", &message)
        })
        .await?;
        let new_version =
            project_main_or_log(&self.pool, id, &bare, "merge_resolved", &repo.owner, &repo.slug).await;
        spawn_mirror(self.pool.clone(), id, bare.clone()); // Ф3
        Ok(Response::new(MergeBranchResponse { tip_sha: tip, new_version, fast_forward: false }))
    }

    async fn create_tag(&self, req: Request<CreateTagRequest>) -> Result<Response<BranchOpResponse>, Status> {
        let CreateTagRequest { repo, name, version } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        if !valid_tag(&name) {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad tag name"));
        }
        // Отдельным кодом от «плохого имени»: имя корректно, но принадлежит версиям.
        // Клиенту нужно показать РАЗНЫЕ подсказки, поэтому и сообщения разные.
        if is_version_tag(&name) {
            return Err(reason::status(Code::InvalidArgument, Reason::ReservedTagName, "reserved tag name"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let sha = with_repo(bare, move |repo| {
            // Коммит версии: у каждой версии уже есть лёгкий тег vN.
            let target = repo
                .refname_to_id(&format!("refs/tags/v{version}"))
                .map_err(|_| Status::not_found("version not found"))?;
            let obj = repo.find_object(target, None).map_err(internal)?;
            // force=true: повторный релиз с тем же именем перевесит тег.
            repo.tag_lightweight(&name, &obj, true).map_err(internal)?;
            Ok(target.to_string())
        })
        .await?;
        // Ф3: релизный тег — тоже контент зеркала (refspec тянет все теги).
        let (bare_m, id_m) = self.ensure(&repo.owner, &repo.slug).await?;
        spawn_mirror(self.pool.clone(), id_m, bare_m);
        Ok(Response::new(BranchOpResponse { tip_sha: sha }))
    }

    async fn list_tags(&self, req: Request<RepoRef>) -> Result<Response<TagsResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        let (bare, _id) = self.ensure(&owner, &slug).await?;
        let tags = with_repo(bare, |repo| {
            let mut out: Vec<Tag> = Vec::new();
            repo.tag_foreach(|oid, name_bytes| {
                if let Ok(name) = std::str::from_utf8(name_bytes) {
                    let name = name.strip_prefix("refs/tags/").unwrap_or(name).to_string();
                    // peel до коммита (лёгкий тег указывает прямо на коммит).
                    let sha = repo
                        .find_object(oid, None)
                        .and_then(|o| o.peel_to_commit())
                        .map(|c| c.id().to_string())
                        .unwrap_or_else(|_| oid.to_string());
                    out.push(Tag { name, target_sha: sha });
                }
                true
            })
            .map_err(internal)?;
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
        .await?;
        Ok(Response::new(TagsResponse { tags }))
    }

    /// Обновить ветку из main — обратное слияние. main НЕ двигается, поэтому
    /// проекции версии нет: ветка это черновик, версии рождаются только из main.
    async fn update_branch(
        &self,
        req: Request<UpdateBranchRequest>,
    ) -> Result<Response<UpdateBranchResponse>, Status> {
        let UpdateBranchRequest { repo, name } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        if !valid_branch(&name) || name == "main" {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Под тем же локом, что merge/push: tip ветки нельзя читать до захвата —
        // конкурентный пуш иначе потерялся бы (та же причина, что в merge_branch).
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let (tip, ff) = with_repo(bare, move |repo| {
            let branch_ref = format!("refs/heads/{name}");
            let branch_tip = repo
                .refname_to_id(&branch_ref)
                .map_err(|_| reason::status(Code::NotFound, Reason::NotFound, "branch not found"))?;
            let main_tip = repo.refname_to_id(MAIN_REF).map_err(internal)?;
            // Ветка уже содержит main → обновлять нечего.
            if repo.graph_descendant_of(branch_tip, main_tip).unwrap_or(false) || branch_tip == main_tip {
                return Err(reason::status(
                    Code::FailedPrecondition,
                    Reason::NothingToMerge,
                    "branch is not ahead of base",
                ));
            }
            // Ветка — предок main (в ней нет своих коммитов) → просто двигаем ref.
            if repo.graph_descendant_of(main_tip, branch_tip).unwrap_or(false) {
                repo.reference(
                    &branch_ref,
                    main_tip,
                    true,
                    &format!("update {name} from main: fast-forward"),
                )
                .map_err(internal)?;
                return Ok((main_tip.to_string(), true));
            }
            // Расхождение → merge-commit В ВЕТКЕ: ours = ветка, theirs = main
            // (порядок обратный merge_branch — сливаем main в ветку, а не наоборот).
            let ours = repo.find_commit(branch_tip).map_err(internal)?;
            let theirs = repo.find_commit(main_tip).map_err(internal)?;
            let mut idx = repo.merge_commits(&ours, &theirs, None).map_err(internal)?;
            if idx.has_conflicts() {
                return Err(reason::status(
                    Code::FailedPrecondition,
                    Reason::Conflict,
                    "merge does not apply cleanly",
                ));
            }
            let tree_id = idx.write_tree_to(repo).map_err(internal)?;
            let tree = repo.find_tree(tree_id).map_err(internal)?;
            let sig = merge_sig()?;
            let merged = repo
                .commit(Some(&branch_ref), &sig, &sig, "Merge branch 'main'", &tree, &[&ours, &theirs])
                .map_err(internal)?;
            Ok((merged.to_string(), false))
        })
        .await?;
        Ok(Response::new(UpdateBranchResponse { tip_sha: tip, fast_forward: ff }))
    }

    /// Записать list.json в ветку одним коммитом («предложенные правки»).
    ///
    /// Отличие от merge_resolved: пишем в ВЕТКУ, main не двигается, родитель один
    /// — а значит и проекции версии здесь нет (версии рождаются только из main).
    async fn commit_to_branch(
        &self,
        req: Request<CommitToBranchRequest>,
    ) -> Result<Response<CommitToBranchResponse>, Status> {
        let CommitToBranchRequest { repo, branch, message, expected_tip, author_name, author_email, content } =
            req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        // Предусловие записи (ADR-0015): спрашиваем приложение ДО любой работы.
        crate::gate::ensure_writable(&repo.owner, &repo.slug).await?;
        if !valid_branch(&branch) || branch == "main" {
            return Err(reason::status(Code::InvalidArgument, Reason::BadName, "bad branch name"));
        }
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Канон собирает ядро: содержимое — из запроса, kind и надстройки блоков
        // (картинка/пометка, по идентичности) — из БД (Ф2a).
        let kind = db::load_list_kind(&self.pool, id).await.map_err(db_status)?;
        let carry = db::current_marks(&self.pool, id).await.map_err(db_status)?;
        let list_json = canon_list_json(content, kind, &carry)?;
        // Тот же лок, что у merge/push: tip читаем ПОСЛЕ захвата, иначе
        // конкурентный пуш в ветку потерялся бы.
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let (tip, changed) = with_repo(bare, move |repo| {
            let author = opt(&author_name).zip(opt(&author_email));
            match write::commit_list_json(repo, &branch, &list_json, &message, &expected_tip, author) {
                Ok(write::WriteOutcome::Committed(sha)) => Ok((sha, true)),
                Ok(write::WriteOutcome::Unchanged(sha)) => Ok((sha, false)),
                Err(write::WriteError::NotFound) => {
                    Err(reason::status(Code::NotFound, Reason::NotFound, "branch not found"))
                }
                Err(write::WriteError::Stale) => Err(reason::status(
                    Code::FailedPrecondition,
                    Reason::Stale,
                    "branch tip moved since it was read",
                )),
                Err(write::WriteError::Git(e)) => Err(Status::internal(e)),
            }
        })
        .await?;
        Ok(Response::new(CommitToBranchResponse { tip_sha: tip, changed }))
    }

    /// Ф3: пуш зеркала по запросу (кнопка «Синхронизировать», после сохранения
    /// настроек). Исход в теле ответа — текст ошибки показывается владельцу.
    async fn mirror_push(&self, req: Request<RepoRef>) -> Result<Response<MirrorPushResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        let (bare, id) = self.ensure(&owner, &slug).await?;
        match push_mirror_now(&self.pool, id, &bare).await {
            Ok(()) => Ok(Response::new(MirrorPushResponse { ok: true, error: String::new() })),
            Err(e) => Ok(Response::new(MirrorPushResponse { ok: false, error: e })),
        }
    }

    /// Коммиты рефа (свежие первыми). `not_in` скрывает достижимое из базы —
    /// так вкладка «Коммиты» показывает ровно вклад ветки, а не всю историю.
    async fn list_commits(
        &self,
        req: Request<ListCommitsRequest>,
    ) -> Result<Response<CommitsResponse>, Status> {
        let ListCommitsRequest { repo, rev, not_in, limit } = req.into_inner();
        let RepoRef { owner, slug } = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        if rev.is_empty() {
            return Err(Status::invalid_argument("rev required"));
        }
        let (bare, _id) = self.ensure(&owner, &slug).await?;
        let take = if limit <= 0 { 100 } else { limit.min(500) } as usize;
        let found =
            with_repo(bare, move |repo| history::commits(repo, &rev, &not_in, take).map_err(internal))
                .await?;
        // Несуществующий реф — не ошибка: ветку могли удалить, UI покажет пусто.
        let commits = found
            .iter()
            .flatten()
            .map(|c| Commit {
                sha: c.sha.clone(),
                message: c.message.clone(),
                author_name: c.author_name.clone(),
                author_email: c.author_email.clone(),
                at_unix: c.at_unix,
                parents: c.parents,
            })
            .collect();
        Ok(Response::new(CommitsResponse { found: found.is_some(), commits }))
    }
}

// Формат принадлежит ядру: канон, собранный из структуры провода, обязан быть
// БАЙТ-В-БАЙТ тем же, что материализация версии кладёт в дерево коммита. Если
// эти два пути разойдутся, «правка в ветке» начнёт писать другой формат, чем push.
#[cfg(test)]
mod canon_tests {
    use super::{ListContent, canon_list_json, from_list_content};
    use crate::git::bundle::version_files;
    use crate::pb::{SnapshotRef, SnapshotStep};

    fn step(n: i32) -> SnapshotStep {
        SnapshotStep {
            n,
            title: "Install Redis".into(),
            desc: "Grab it".into(),
            command: "brew install redis".into(),
            level: "required".into(),
            why: "нужно для кэша".into(),
            section: "Setup".into(),
            subtasks: vec!["проверить версию".into()],
            refs: vec![SnapshotRef { label: "docs".into(), url: "https://redis.io".into() }],
            r#type: String::new(),
            content_json: String::new(),
            block_id: "11111111-2222-3333-4444-555555555555".into(),
        }
    }

    fn content(steps: Vec<SnapshotStep>) -> ListContent {
        ListContent {
            title: "Redis Caching".into(),
            desc: "Описание".into(),
            tags: vec!["redis".into(), "кэш".into()],
            ordered: true,
            version: 7,
            steps,
        }
    }

    /// Канон из структуры == list.json из материализации той же версии.
    #[test]
    fn structured_content_matches_materialized_list_json() {
        let c = content(vec![step(1), SnapshotStep { n: 2, title: "Configure".into(), ..step(2) }]);
        let from_wire =
            canon_list_json(Some(c.clone()), None, &Default::default()).expect("канон из структуры");
        let materialized = version_files(&from_list_content(c))
            .into_iter()
            .find(|(p, _)| p == "list.json")
            .expect("list.json")
            .1;
        assert_eq!(String::from_utf8(from_wire).unwrap(), materialized);
    }

    /// Не-step блоки: type/content едут проводом и попадают в канон.
    #[test]
    fn non_step_block_carries_type_and_content() {
        let block = SnapshotStep {
            n: 1,
            r#type: "text".into(),
            content_json: r#"{"md":"Вступление"}"#.into(),
            block_id: String::new(),
            ..step(1)
        };
        let out = canon_list_json(Some(content(vec![block])), None, &Default::default()).expect("канон");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\"type\": \"text\""), "тип блока в каноне: {s}");
        assert!(s.contains("\"md\": \"Вступление\""), "payload блока в каноне: {s}");
        assert!(!s.contains("\"blockId\""), "пустая идентичность не пишется вовсе");
    }

    /// Пустой url ссылки — это ОТСУТСТВИЕ url (провод не различает '' и None).
    #[test]
    fn empty_ref_url_is_omitted() {
        let s = SnapshotStep {
            refs: vec![SnapshotRef { label: "без ссылки".into(), url: String::new() }],
            ..step(1)
        };
        let out = canon_list_json(Some(content(vec![s])), None, &Default::default()).expect("канон");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"label\": \"без ссылки\""));
        assert!(!text.contains("\"url\""), "пустой url не должен попадать в канон: {text}");
    }

    /// Прислать готовый файл больше нельзя: без структуры запрос бессмыслен.
    #[test]
    fn без_содержимого_запрос_отклоняется() {
        let err = canon_list_json(None, None, &Default::default()).expect_err("канон не из чего собрать");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert_eq!(err.message(), "content required");
    }
}

#[cfg(test)]
mod branch_name_tests {
    use super::{is_version_tag, valid_branch, valid_tag};

    /// Список — дословное зеркало проверок `badBranch` на фронте
    /// (features/git/core.inproc.ts). Расходиться этим двум копиям нельзя: имя,
    /// заведённое через ядро, потом уходит в `execFile('git', […])` на inproc-пути.
    #[test]
    fn mirrors_frontend_rule() {
        for ok in ["main", "feature-1", "fix_a.b", "PR-42", "v1.2.3"] {
            assert!(valid_branch(ok), "должно быть валидно: {ok}");
        }
        for bad in [
            "",        // пусто
            "-D",      // ведущий '-' = флаг для git на inproc-пути
            "-main",   //
            "a..b",    // путь наверх
            "feat/x",  // слэш — не простое имя
            "ветка",   // не-ASCII: фронт такое отвергает, ядро пропускало
            "ветка-1", //
            "brànch",  // юникод-буква внутри ASCII-имени
            "a b",     // пробел
            "a\nb",    // перевод строки
        ] {
            assert!(!valid_branch(bad), "должно быть отвергнуто: {bad:?}");
        }
    }

    /// ТЕГИ живут по своим правилам: имя тега не уходит в аргументы `git`, поэтому
    /// человекочитаемые релизы («релиз-1», «версия 2» через дефис) остаются валидными —
    /// иначе парити с ветками сломала бы уже созданные релизы (авто-ревью core #56).
    #[test]
    fn tags_allow_human_names_but_not_broken_refs() {
        for ok in ["v1.2.3", "релиз-1", "release_2026-07", "prod.1"] {
            assert!(valid_tag(ok), "тег должен быть валиден: {ok}");
        }
        for bad in [
            "",       // пусто
            "-rc1",   // ведущий '-'
            "a..b",   // путь наверх
            "rel~1",  // служебные символы refspec
            "rel^2",  //
            "rel:1",  //
            "rel?1",  //
            "rel*",   //
            "rel[1]", //
            "a b",    // пробел
            "a
b",     // перевод строки
            "rel.",   // завершающая точка
            "/rel",   // ведущий слэш
            "rel/",   // завершающий слэш
            "a//b",   // двойной слэш
            "rel@{1}", // reflog-синтаксис
        ] {
            assert!(!valid_tag(bad), "тег должен быть отвергнут: {bad:?}");
        }
    }

    /// `v<число>` принадлежит версиям: по этим тегам считается «докуда версии уже
    /// записаны в git». Релиз с таким именем либо останавливал досыпку версий
    /// молча (имя выше текущей версии), либо перевешивал тег существующей версии.
    #[test]
    fn имена_версий_зарезервированы_за_системой() {
        for reserved in ["v1", "v8", "v20", "v0", "v000", "v999999"] {
            assert!(is_version_tag(reserved), "имя версии должно быть зарезервировано: {reserved}");
        }
    }

    /// Запрет ровно на то, что парсит max_tag_version — не шире. Человеку остаются
    /// и «v1.0», и «v2-beta», и всё, где после v не только цифры.
    #[test]
    fn человеческие_имена_с_v_остаются_доступны() {
        for free in ["v1.0", "v2-beta", "v", "version1", "v1a", "1", "v1_2", "V1"] {
            assert!(!is_version_tag(free), "имя должно остаться человеку: {free}");
        }
    }

    /// Зарезервированное имя проходит проверку ФОРМЫ — значит одной valid_tag мало,
    /// и отдельная проверка в create_tag обязана существовать.
    #[test]
    fn зарезервированное_имя_формально_валидно() {
        assert!(valid_tag("v20"), "по форме ref это корректное имя");
        assert!(is_version_tag("v20"), "и именно поэтому нужен отдельный запрет");
    }
}

#[cfg(test)]
mod squash_tests {
    use super::{commit_resolved, with_coauthors};

    /// Каталог-однодневка: удаляется на Drop (в т.ч. при panic внутри теста).
    struct Tmp(std::path::PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn bare() -> (Tmp, git2::Repository) {
        let p = std::env::temp_dir().join(format!("setfork-squash-{}", uuid::Uuid::new_v4()));
        let repo = git2::Repository::init_bare(&p).expect("init bare");
        (Tmp(p), repo)
    }

    /// Пустой коммит от заданного автора на ref.
    fn commit(
        repo: &git2::Repository,
        refname: &str,
        msg: &str,
        who: (&str, &str),
        parents: &[git2::Oid],
    ) -> git2::Oid {
        let tree = repo.treebuilder(None).expect("tb").write().expect("tree");
        let tree = repo.find_tree(tree).expect("find tree");
        let sig = git2::Signature::new(who.0, who.1, &git2::Time::new(1_700_000_000, 0)).expect("sig");
        let ps: Vec<git2::Commit> = parents.iter().map(|o| repo.find_commit(*o).expect("parent")).collect();
        let refs: Vec<&git2::Commit> = ps.iter().collect();
        repo.commit(Some(refname), &sig, &sig, msg, &tree, &refs).expect("commit")
    }

    /**
     * При squash история ветки в main не попадает, поэтому Co-authored-by —
     * ЕДИНСТВЕННАЯ запись о том, кто делал работу. Ошибка тут молча стирает
     * авторство.
     */
    #[test]
    fn трейлеры_собираются_из_вклада_ветки_без_дублей() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("Мика", "m@example.com"), &[]);
        let a = commit(&repo, "refs/heads/pr", "первый", ("Аня", "a@example.com"), &[base]);
        let b = commit(&repo, "refs/heads/pr", "второй", ("Аня", "a@example.com"), &[a]);
        let c = commit(&repo, "refs/heads/pr", "третий", ("Боря", "b@example.com"), &[b]);

        let msg = with_coauthors(&repo, c, base, "Заголовок");

        assert!(msg.starts_with("Заголовок\n\n"), "трейлеры отделены пустой строкой: {msg:?}");
        assert_eq!(msg.matches("Co-authored-by: Аня <a@example.com>").count(), 1, "дубли схлопнуты");
        assert!(msg.contains("Co-authored-by: Боря <b@example.com>"));
        // Автор коммита ИЗ MAIN не соавтор этой правки.
        assert!(!msg.contains("m@example.com"), "автор базы не должен попасть в соавторы");
    }

    #[test]
    fn служебная_подпись_сервиса_соавторством_не_считается() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("Мика", "m@example.com"), &[]);
        let a = commit(
            &repo,
            "refs/heads/pr",
            "авто",
            (crate::git::bundle::AUTHOR_NAME, crate::git::bundle::AUTHOR_EMAIL),
            &[base],
        );

        let msg = with_coauthors(&repo, a, base, "Заголовок");

        assert_eq!(msg, "Заголовок", "нечего приписывать — заголовок остаётся как есть");
    }

    #[test]
    fn ветка_без_своих_коммитов_не_добавляет_ничего() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("Мика", "m@example.com"), &[]);
        assert_eq!(with_coauthors(&repo, base, base, "Заголовок"), "Заголовок");
    }

    // ── Перенос покрытия из TS (coauthors.test.ts) перед Ф0b ────────────────
    // Сверка реализаций показала, что TS фильтровал больше: пустые подписи и
    // регистр служебной почты. Здесь это чинится в ядре — оно остаётся одно.

    /// Подпись с пробелами по краям не должна давать кривой трейлер.
    ///
    /// Пустое имя/почту здесь не проверить: git2 отказывается создавать такую
    /// подпись вовсе («Signature cannot have an empty name or email»), и обычным
    /// путём такой коммит не появится. Отбраковка пустых в `with_coauthors`
    /// оставлена как защита от коммитов, приехавших пушем из импортированных
    /// репозиториев (формат коммита сам по себе `author  <>` допускает).
    #[test]
    fn подпись_с_пробелами_по_краям_обрезается() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("SetFork", "git@setfork.com"), &[]);
        let a = commit(&repo, "refs/heads/pr", "a", ("  Гость  ", " g@example.com "), &[base]);

        let msg = with_coauthors(&repo, a, base, "Заголовок");
        assert!(msg.contains("Co-authored-by: Гость <g@example.com>"), "подпись обрезана: {msg}");
        assert!(!msg.contains("  Гость"), "лишние пробелы не доехали: {msg}");
    }

    /// Почта регистронезависима: «GIT@SetFork.com» — та же служебная подпись.
    #[test]
    fn служебная_почта_узнаётся_в_любом_регистре() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("SetFork", "git@setfork.com"), &[]);
        let a = commit(&repo, "refs/heads/pr", "a", ("SetFork", "GIT@SetFork.COM"), &[base]);
        assert_eq!(with_coauthors(&repo, a, base, "Заголовок"), "Заголовок");
    }

    /// Перед трейлерами обязана быть пустая строка — иначе git не считает их
    /// трейлерами и `git interpret-trailers` их не видит.
    #[test]
    fn перед_трейлерами_пустая_строка() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("SetFork", "git@setfork.com"), &[]);
        let a = commit(&repo, "refs/heads/pr", "a", ("Гость", "g@example.com"), &[base]);
        let msg = with_coauthors(&repo, a, base, "Заголовок");
        assert_eq!(msg, "Заголовок\n\nCo-authored-by: Гость <g@example.com>");
    }

    // ── Ручной резолв конфликта: режим слияния ───────────────────────────────
    // Раньше режима не было, и фронт на remote-пути (а это ПРОД) отказывал в
    // резолве squash-списков, выдавая отказ за 'conflict'.

    /// main + ветка с одним своим коммитом от стороннего автора.
    fn main_and_branch(repo: &git2::Repository) -> (git2::Oid, git2::Oid) {
        let base = commit(repo, "refs/heads/main", "base", ("SetFork", "git@setfork.com"), &[]);
        let theirs = commit(repo, "refs/heads/pr-1", "их правка", ("Гость", "guest@example.com"), &[base]);
        // main уходит вперёд — иначе это не расхождение, а перемотка.
        commit(repo, "refs/heads/main", "наша правка", ("SetFork", "git@setfork.com"), &[base]);
        (repo.refname_to_id("refs/heads/main").expect("main"), theirs)
    }

    fn commit_at<'a>(repo: &'a git2::Repository, sha: &str) -> git2::Commit<'a> {
        repo.find_commit(git2::Oid::from_str(sha).expect("oid")).expect("commit")
    }

    #[test]
    fn резолв_squash_даёт_одного_родителя_и_трейлеры() {
        let (_t, repo) = bare();
        main_and_branch(&repo);
        let sha = commit_resolved(&repo, "pr-1", br#"{"steps":[]}"#, true, "Свели руками").expect("резолв");
        let c = commit_at(&repo, &sha);
        assert_eq!(c.parent_count(), 1, "squash не тянет историю ветки в main");
        let msg = c.message().expect("сообщение");
        assert!(msg.starts_with("Свели руками"), "заголовок из запроса: {msg}");
        assert!(msg.contains("Co-authored-by: Гость <guest@example.com>"), "авторство ветки: {msg}");
        // Разрешённый канон на месте, steps/ сброшены.
        let tree = c.tree().expect("дерево");
        assert!(tree.get_path(std::path::Path::new("list.json")).is_ok());
        assert!(tree.get_path(std::path::Path::new("steps")).is_err());
    }

    #[test]
    fn резолв_squash_без_сообщения_берёт_имя_ветки() {
        let (_t, repo) = bare();
        main_and_branch(&repo);
        let sha = commit_resolved(&repo, "pr-1", br#"{"steps":[]}"#, true, "   ").expect("резолв");
        assert!(commit_at(&repo, &sha).message().expect("msg").starts_with("Squashed branch 'pr-1'"));
    }

    #[test]
    fn резолв_обычным_merge_даёт_двух_родителей() {
        let (_t, repo) = bare();
        main_and_branch(&repo);
        let sha = commit_resolved(&repo, "pr-1", br#"{"steps":[]}"#, false, "").expect("резолв");
        let c = commit_at(&repo, &sha);
        assert_eq!(c.parent_count(), 2, "обычный резолв сохраняет обе линии");
        assert_eq!(c.message().expect("msg"), "Merge branch 'pr-1' (resolved)");
    }

    #[test]
    fn резолв_двигает_main_и_знает_про_отсутствие_ветки() {
        let (_t, repo) = bare();
        main_and_branch(&repo);
        let sha = commit_resolved(&repo, "pr-1", br#"{"steps":[]}"#, true, "x").expect("резолв");
        assert_eq!(repo.refname_to_id(super::MAIN_REF).expect("main").to_string(), sha, "main переехал");

        let err = commit_resolved(&repo, "нет-такой", br#"{"steps":[]}"#, true, "x").expect_err("нет ветки");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    /// Ф2b: ручной резолв оставляет витрину свежей — README из нового канона.
    #[test]
    fn резолв_перегенерирует_readme_из_канона() {
        let (_t, repo) = bare();
        main_and_branch(&repo);
        let canon = crate::git::serialize::list_json(&crate::git::bundle::VersionData {
            version: 3,
            note: String::new(),
            ts: 0,
            title: "Resolved title".into(),
            desc: String::new(),
            tags: vec![],
            ordered: true,
            kind: None,
            steps: vec![],
        });
        let sha = commit_resolved(&repo, "pr-1", canon.as_bytes(), true, "x").expect("резолв");
        let tree = commit_at(&repo, &sha).tree().expect("tree");
        let readme = tree.get_path(std::path::Path::new("README.md")).expect("README есть");
        let readme =
            String::from_utf8(repo.find_blob(readme.id()).expect("blob").content().to_vec()).expect("utf8");
        assert!(readme.contains("# Resolved title"), "витрина из нового канона: {readme}");
        assert!(tree.get_path(std::path::Path::new(".gitattributes")).is_ok());
    }

    #[test]
    fn резолв_ветки_на_том_же_коммите_нечего_сливать() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("SetFork", "git@setfork.com"), &[]);
        repo.reference("refs/heads/pr-1", base, true, "ветка на main").expect("ref");
        let err = commit_resolved(&repo, "pr-1", br#"{"steps":[]}"#, false, "").expect_err("nothing");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        // Сверяем ПРИЧИНУ, а не текст (И1): текст — для логов и человека, его
        // можно менять свободно; контракт с клиентом держит трейлер.
        assert_eq!(
            err.metadata().get(crate::reason::REASON_KEY).and_then(|v| v.to_str().ok()),
            Some("NOTHING_TO_MERGE")
        );
    }
}
