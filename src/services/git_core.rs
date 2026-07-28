//! GitCore — gRPC-сервис поверх git-подсистемы: smart-HTTP (clone/push),
//! ветки/теги/merge, bundle. Обслуживает proto/git.proto (setfork.git.v1).
use sqlx::postgres::PgPool;
use std::path::PathBuf;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::util::{db_status, internal};
use crate::db;
use crate::git::bundle::VersionData;
use crate::git::{MAIN_REF, bundle, history, project, repo, smart_http, write};
use crate::pb::git_core_server::GitCore;
use crate::pb::{
    Branch, BranchOpResponse, BranchSnapshotRequest, BranchSnapshotResponse, BranchesResponse, BytesResponse,
    Commit, CommitToBranchRequest, CommitToBranchResponse, CommitsResponse, CreateBranchRequest,
    CreateTagRequest, DeleteBranchRequest, InfoRefsRequest, ListCommitsRequest, MergeBranchRequest,
    MergeBranchResponse, MergeResolvedRequest, MergeStateRequest, MergeStateResponse, PostRequest,
    ReceivePackResponse, RepoRef, SnapshotRef, SnapshotStep, Tag, TagsResponse, UpdateBranchRequest,
    UpdateBranchResponse,
};

// Только простые имена веток — никаких путей/точек (защита от ref-инъекций).
fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.contains("..")
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
                Ok(v) => v.unwrap_or(0),
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
            Ok(v) => v.unwrap_or(0),
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
        let MergeResolvedRequest { repo, branch, list_json } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        if !valid_branch(&branch) || branch == "main" {
            return Err(Status::invalid_argument("bad branch name"));
        }
        // Контент обязан быть валидным JSON-объектом (list.json — канон).
        if serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&list_json).is_err() {
            return Err(Status::invalid_argument("list_json is not a JSON object"));
        }
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        let _guard = repo::repo_guard(&self.pool, id).await.map_err(db_status)?;
        let tip = with_repo(bare.clone(), move |repo| {
            let branch_tip = repo
                .refname_to_id(&format!("refs/heads/{branch}"))
                .map_err(|_| Status::not_found("branch not found"))?;
            let main_tip = repo.refname_to_id(MAIN_REF).map_err(internal)?;
            if main_tip == branch_tip {
                return Err(Status::failed_precondition("nothing-to-merge"));
            }
            // Дерево = дерево main c заменённым list.json и БЕЗ steps/ (см. proto).
            let ours = repo.find_commit(main_tip).map_err(internal)?;
            let theirs = repo.find_commit(branch_tip).map_err(internal)?;
            let blob = repo.blob(&list_json).map_err(internal)?;
            let mut tb = repo.treebuilder(Some(&ours.tree().map_err(internal)?)).map_err(internal)?;
            tb.insert("list.json", blob, 0o100644).map_err(internal)?;
            if tb.get("steps").map_err(internal)?.is_some() {
                tb.remove("steps").map_err(internal)?;
            }
            let tree_id = tb.write().map_err(internal)?;
            let tree = repo.find_tree(tree_id).map_err(internal)?;
            let sig = merge_sig()?;
            let msg = format!("Merge branch '{branch}' (resolved)");
            let merged =
                repo.commit(Some(MAIN_REF), &sig, &sig, &msg, &tree, &[&ours, &theirs]).map_err(internal)?;
            Ok(merged.to_string())
        })
        .await?;
        let new_version = match project::project_pushed_commit(&self.pool, id, &bare).await {
            Ok(v) => v.unwrap_or(0),
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
        if !valid_branch(&name) {
            return Err(Status::invalid_argument("bad tag name"));
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
        let CommitToBranchRequest {
            repo,
            branch,
            list_json,
            message,
            expected_tip,
            author_name,
            author_email,
        } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        if !valid_branch(&branch) || branch == "main" {
            return Err(Status::invalid_argument("bad branch name"));
        }
        // Контент обязан быть валидным JSON-объектом — как в merge_resolved:
        // list.json канон, и мусор в ветке сломал бы её снапшот и дифф.
        if serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&list_json).is_err() {
            return Err(Status::invalid_argument("list_json is not a JSON object"));
        }
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

#[cfg(test)]
mod squash_tests {
    use super::with_coauthors;

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
}
