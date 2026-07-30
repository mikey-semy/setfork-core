//! GitCore — gRPC-сервис поверх git-подсистемы: smart-HTTP (clone/push),
//! ветки/теги/merge, bundle. Обслуживает proto/git.proto (setfork.git.v1).
use sqlx::postgres::PgPool;
use std::path::PathBuf;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::util::{db_status, internal};
use crate::db;
use crate::git::bundle::{SerStep, StepRef, VersionData};
use crate::git::{MAIN_REF, bundle, history, project, repo, serialize, smart_http, write};
use crate::pb::git_core_server::GitCore;
use crate::pb::{
    Branch, BranchOpResponse, BranchSnapshotRequest, BranchSnapshotResponse, BranchesResponse, BytesResponse,
    Commit, CommitToBranchRequest, CommitToBranchResponse, CommitsResponse, CreateBranchRequest,
    CreateTagRequest, DeleteBranchRequest, InfoRefsRequest, ListCommitsRequest, ListContent,
    MergeBranchRequest, MergeBranchResponse, MergeResolvedRequest, MergeStateRequest, MergeStateResponse,
    PostRequest, ReceivePackResponse, RepoRef, SnapshotRef, SnapshotStep, Tag, TagsResponse,
    UpdateBranchRequest, UpdateBranchResponse,
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
            .ok_or_else(|| Status::not_found("list not found"))?;
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
            .ok_or_else(|| Status::not_found("list not found"))
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
            let (n, e) = (n.to_string(), e.to_string());
            // Служебная подпись самого сервиса соавторством не является.
            if e == bundle::AUTHOR_EMAIL {
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

/// Текущий oid main (или None, если ветки ещё нет) — для проверки «push сдвинул main».
fn main_oid(bare: &std::path::Path) -> Option<String> {
    git2::Repository::open_bare(bare).ok()?.refname_to_id(MAIN_REF).ok().map(|o| o.to_string())
}

// Пустая proto-строка → None (proto3 не отличает '' от отсутствия поля).
fn opt(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
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
                level: s.level,
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
        .map_err(|_| Status::not_found("branch not found"))?;
    let main_tip = repo.refname_to_id(MAIN_REF).map_err(internal)?;
    if main_tip == branch_tip {
        return Err(Status::failed_precondition("nothing-to-merge"));
    }
    let ours = repo.find_commit(main_tip).map_err(internal)?;
    let theirs = repo.find_commit(branch_tip).map_err(internal)?;
    let blob = repo.blob(list_json).map_err(internal)?;
    let mut tb = repo.treebuilder(Some(&ours.tree().map_err(internal)?)).map_err(internal)?;
    tb.insert("list.json", blob, 0o100644).map_err(internal)?;
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
    let merged = repo.commit(Some(MAIN_REF), &sig, &sig, &msg, &tree, &parents).map_err(internal)?;
    Ok(merged.to_string())
}

/// Канонические байты list.json для записи в ветку — ВСЕГДА собираются здесь,
/// из присланной структуры. Прислать готовый файл больше нельзя: поле `list_json`
/// снято из контракта (`reserved`), потому что оно требовало от клиента знать
/// правила формата, а значит держать вторую его реализацию.
fn canon_list_json(content: Option<ListContent>) -> Result<Vec<u8>, Status> {
    let c = content.ok_or_else(|| Status::invalid_argument("content required"))?;
    Ok(serialize::list_json(&from_list_content(c)).into_bytes())
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
        let PostRequest { repo, body, git_protocol } = req.into_inner();
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
        let PostRequest { repo, body, git_protocol } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Критическая секция: receive-pack + проекция под одним локом репо
        // (ленивый append не вклинивается между приёмом и проекцией).
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let bare_recv = bare.clone();
        // Внутри одного spawn_blocking: oid main до и после приёма пака — чтобы
        // проецировать версию ТОЛЬКО когда push реально сдвинул main. Пуш в
        // ветку-черновик main не двигает → иначе плодились бы дубли версий.
        let (data, moved) = tokio::task::spawn_blocking(move || -> std::io::Result<(Vec<u8>, bool)> {
            let before = main_oid(&bare_recv);
            let data = smart_http::receive_pack_rpc(&bare_recv, &body, opt(&git_protocol))?;
            let after = main_oid(&bare_recv);
            Ok((data, after.is_some() && after != before))
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;
        // Проекция list.json нового main tip → новая версия (0 = не спроецировано).
        // Сбой проекции НЕ отменяет push (git-объекты целы), но обязан быть громким:
        // тихая потеря версии — худший исход (аудит 2026-07-20, P0-1).
        let new_version = if moved {
            match project::project_pushed_commit(&self.pool, id, &bare).await {
                Ok(Some(v)) => v,
                // «Спроецировать нечего» — тот же исход, что и сбой: git принят, а версии
                // нет (битый или пустой list.json). Счётчик рос только в Err-ветке, поэтому
                // этот случай проходил мимо алерта — молча (линза 02, F3, побочная находка).
                Ok(None) => {
                    metrics::counter!("projection_failures_total", "op" => "push_empty").increment(1);
                    tracing::warn!(
                        owner = %repo.owner, slug = %repo.slug, %id,
                        "проекция push ничего не создала (list.json не разобран или без steps) — версия НЕ создана"
                    );
                    0
                }
                Err(e) => {
                    metrics::counter!("projection_failures_total", "op" => "push").increment(1);
                    tracing::error!(
                        owner = %repo.owner, slug = %repo.slug, %id, error = %e,
                        "ОШИБКА проекции push — git принят, версия НЕ создана; восстановление: reproject"
                    );
                    0
                }
            }
        } else {
            0
        };
        Ok(Response::new(ReceivePackResponse { data, new_version }))
    }
    async fn create_bundle(&self, req: Request<RepoRef>) -> Result<Response<BytesResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        // Персистентный репо (как TS bundleRepo) — bundle включает запушенные коммиты.
        let data = repo::bundle_repo(&self.pool, &owner, &slug)
            .await
            .map_err(db_status)?
            .ok_or_else(|| Status::not_found("list not found"))?;
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
            return Err(Status::invalid_argument("bad branch name"));
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
        let from = if from.is_empty() { "main".to_string() } else { from };
        if !valid_branch(&name) || !valid_branch(&from) {
            return Err(Status::invalid_argument("bad branch name"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let tip = with_repo(bare, move |repo| {
            let base = repo
                .refname_to_id(&format!("refs/heads/{from}"))
                .map_err(|_| Status::not_found("base branch not found"))?;
            let commit = repo.find_commit(base).map_err(internal)?;
            // force=false: существующая ветка → ошибка (already_exists наружу).
            // .map(|_| ()) сразу дропает Branch<'_> (заимствует repo).
            match repo.branch(&name, &commit, false).map(|_| ()) {
                Ok(()) => Ok(base.to_string()),
                Err(e) if e.code() == git2::ErrorCode::Exists => Err(Status::already_exists("branch exists")),
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
        if !valid_branch(&name) {
            return Err(Status::invalid_argument("bad branch name"));
        }
        if name == "main" {
            return Err(Status::failed_precondition("main is protected"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        with_repo(bare, move |repo| {
            let mut branch = repo
                .find_branch(&name, git2::BranchType::Local)
                .map_err(|_| Status::not_found("branch not found"))?;
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
        if !valid_branch(&name) || name == "main" {
            return Err(Status::invalid_argument("bad branch name"));
        }
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Merge двигает main → критическая секция с проекцией (как receive_pack).
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let (tip, ff) = with_repo(bare.clone(), move |repo| {
            let branch_tip = repo
                .refname_to_id(&format!("refs/heads/{name}"))
                .map_err(|_| Status::not_found("branch not found"))?;
            let main_tip = repo.refname_to_id(MAIN_REF).map_err(internal)?;
            let (ahead, _behind) = repo.graph_ahead_behind(branch_tip, main_tip).map_err(internal)?;
            if ahead == 0 {
                return Err(Status::failed_precondition("nothing-to-merge"));
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
                    return Err(Status::failed_precondition("conflict"));
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
                let squashed =
                    repo.commit(Some(MAIN_REF), &sig, &sig, &msg, &tree, &[&ours]).map_err(internal)?;
                return Ok((squashed.to_string(), false));
            }
            // main — предок ветки → fast-forward: просто двигаем ref.
            if repo.graph_descendant_of(branch_tip, main_tip).unwrap_or(false) {
                repo.reference(MAIN_REF, branch_tip, true, &format!("merge {name}: fast-forward"))
                    .map_err(internal)?;
                return Ok((branch_tip.to_string(), true));
            }
            // Расхождение → merge-commit; конфликт индекса = failed_precondition.
            let ours = repo.find_commit(main_tip).map_err(internal)?;
            let theirs = repo.find_commit(branch_tip).map_err(internal)?;
            let mut idx = repo.merge_commits(&ours, &theirs, None).map_err(internal)?;
            if idx.has_conflicts() {
                return Err(Status::failed_precondition("conflict"));
            }
            let tree_id = idx.write_tree_to(repo).map_err(internal)?;
            let tree = repo.find_tree(tree_id).map_err(internal)?;
            let sig = merge_sig()?;
            let msg = format!("Merge branch '{name}'");
            let merged =
                repo.commit(Some(MAIN_REF), &sig, &sig, &msg, &tree, &[&ours, &theirs]).map_err(internal)?;
            Ok((merged.to_string(), false))
        })
        .await?;
        // main сдвинулся → проекция новой версии (0 = list.json не изменился).
        // Сбой проекции не отменяет merge, но громко логируется (см. receive_pack).
        let new_version = match project::project_pushed_commit(&self.pool, id, &bare).await {
            Ok(Some(v)) => v,
            // «Спроецировать нечего» — тот же исход, что и сбой: git принят, а версии
            // нет (битый или пустой list.json). Счётчик рос только в Err-ветке, поэтому
            // этот случай проходил мимо алерта — молча (линза 02, F3, побочная находка).
            Ok(None) => {
                metrics::counter!("projection_failures_total", "op" => "merge_empty").increment(1);
                tracing::warn!(
                    owner = %repo.owner, slug = %repo.slug, %id,
                    "проекция merge ничего не создала (list.json не разобран или без steps) — версия НЕ создана"
                );
                0
            }
            Err(e) => {
                metrics::counter!("projection_failures_total", "op" => "merge").increment(1);
                tracing::error!(
                    owner = %repo.owner, slug = %repo.slug, %id, error = %e,
                    "ОШИБКА проекции merge — merge выполнен, версия НЕ создана; восстановление: reproject"
                );
                0
            }
        };
        Ok(Response::new(MergeBranchResponse { tip_sha: tip, new_version, fast_forward: ff }))
    }

    async fn get_merge_state(
        &self,
        req: Request<MergeStateRequest>,
    ) -> Result<Response<MergeStateResponse>, Status> {
        let MergeStateRequest { repo, branch } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        if !valid_branch(&branch) || branch == "main" {
            return Err(Status::invalid_argument("bad branch name"));
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
        if !valid_branch(&branch) || branch == "main" {
            return Err(Status::invalid_argument("bad branch name"));
        }
        // Канон собирает ядро (content) либо принимает готовым от старого клиента.
        let list_json = canon_list_json(content)?;
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let tip = with_repo(bare.clone(), move |repo| {
            commit_resolved(repo, &branch, &list_json, mode == "squash", &message)
        })
        .await?;
        let new_version = match project::project_pushed_commit(&self.pool, id, &bare).await {
            Ok(Some(v)) => v,
            // «Спроецировать нечего» — тот же исход, что и сбой: git принят, а версии
            // нет (битый или пустой list.json). Счётчик рос только в Err-ветке, поэтому
            // этот случай проходил мимо алерта — молча (линза 02, F3, побочная находка).
            Ok(None) => {
                metrics::counter!("projection_failures_total", "op" => "merge_resolved_empty").increment(1);
                tracing::warn!(
                    owner = %repo.owner, slug = %repo.slug, %id,
                    "проекция merge-resolved ничего не создала (list.json не разобран или без steps) — версия НЕ создана"
                );
                0
            }
            Err(e) => {
                metrics::counter!("projection_failures_total", "op" => "merge_resolved").increment(1);
                tracing::error!(
                    owner = %repo.owner, slug = %repo.slug, %id, error = %e,
                    "ОШИБКА проекции merge-resolved — merge выполнен, версия НЕ создана; восстановление: reproject"
                );
                0
            }
        };
        Ok(Response::new(MergeBranchResponse { tip_sha: tip, new_version, fast_forward: false }))
    }

    async fn create_tag(&self, req: Request<CreateTagRequest>) -> Result<Response<BranchOpResponse>, Status> {
        let CreateTagRequest { repo, name, version } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        if !valid_tag(&name) {
            return Err(Status::invalid_argument("bad tag name"));
        }
        // Отдельным кодом от «плохого имени»: имя корректно, но принадлежит версиям.
        // Клиенту нужно показать РАЗНЫЕ подсказки, поэтому и сообщения разные.
        if is_version_tag(&name) {
            return Err(Status::invalid_argument("reserved tag name"));
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
        if !valid_branch(&name) || name == "main" {
            return Err(Status::invalid_argument("bad branch name"));
        }
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Под тем же локом, что merge/push: tip ветки нельзя читать до захвата —
        // конкурентный пуш иначе потерялся бы (та же причина, что в merge_branch).
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let (tip, ff) = with_repo(bare, move |repo| {
            let branch_ref = format!("refs/heads/{name}");
            let branch_tip =
                repo.refname_to_id(&branch_ref).map_err(|_| Status::not_found("branch not found"))?;
            let main_tip = repo.refname_to_id(MAIN_REF).map_err(internal)?;
            // Ветка уже содержит main → обновлять нечего.
            if repo.graph_descendant_of(branch_tip, main_tip).unwrap_or(false) || branch_tip == main_tip {
                return Err(Status::failed_precondition("nothing-to-merge"));
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
                return Err(Status::failed_precondition("conflict"));
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
        if !valid_branch(&branch) || branch == "main" {
            return Err(Status::invalid_argument("bad branch name"));
        }
        // Канон собирает ядро (content) либо принимает готовым от старого клиента.
        let list_json = canon_list_json(content)?;
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Тот же лок, что у merge/push: tip читаем ПОСЛЕ захвата, иначе
        // конкурентный пуш в ветку потерялся бы.
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let (tip, changed) = with_repo(bare, move |repo| {
            let author = opt(&author_name).zip(opt(&author_email));
            match write::commit_list_json(repo, &branch, &list_json, &message, &expected_tip, author) {
                Ok(write::WriteOutcome::Committed(sha)) => Ok((sha, true)),
                Ok(write::WriteOutcome::Unchanged(sha)) => Ok((sha, false)),
                Err(write::WriteError::NotFound) => Err(Status::not_found("branch not found")),
                Err(write::WriteError::Stale) => Err(Status::failed_precondition("stale")),
                Err(write::WriteError::Git(e)) => Err(Status::internal(e)),
            }
        })
        .await?;
        Ok(Response::new(CommitToBranchResponse { tip_sha: tip, changed }))
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
        let from_wire = canon_list_json(Some(c.clone())).expect("канон из структуры");
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
        let out = canon_list_json(Some(content(vec![block]))).expect("канон");
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
        let out = canon_list_json(Some(content(vec![s]))).expect("канон");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"label\": \"без ссылки\""));
        assert!(!text.contains("\"url\""), "пустой url не должен попадать в канон: {text}");
    }

    /// Прислать готовый файл больше нельзя: без структуры запрос бессмыслен.
    #[test]
    fn без_содержимого_запрос_отклоняется() {
        let err = canon_list_json(None).expect_err("канон не из чего собрать");
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

    #[test]
    fn резолв_ветки_на_том_же_коммите_нечего_сливать() {
        let (_t, repo) = bare();
        let base = commit(&repo, "refs/heads/main", "base", ("SetFork", "git@setfork.com"), &[]);
        repo.reference("refs/heads/pr-1", base, true, "ветка на main").expect("ref");
        let err = commit_resolved(&repo, "pr-1", br#"{"steps":[]}"#, false, "").expect_err("nothing");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert_eq!(err.message(), "nothing-to-merge");
    }
}
