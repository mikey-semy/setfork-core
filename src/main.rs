use sqlx::postgres::PgPool;
use tonic::{transport::Server, Request, Response, Status};

mod bundle;
mod ratelimit;
mod db;
mod domain_read;
mod domain_write;
mod project;
mod repo;
mod smart_http;
#[cfg(test)]
mod roundtrip_tests;

use bundle::VersionData;
use std::path::PathBuf;
use uuid::Uuid;

pub mod pb {
    tonic::include_proto!("setfork.git.v1");
}
pub mod pb_domain {
    tonic::include_proto!("setfork.domain.v1");
}

use pb::git_core_server::{GitCore, GitCoreServer};
use pb::{Branch, BranchOpResponse, BranchSnapshotRequest, BranchSnapshotResponse, BranchesResponse, BytesResponse, CreateBranchRequest, DeleteBranchRequest, InfoRefsRequest, MergeBranchRequest, MergeBranchResponse, MergeResolvedRequest, MergeStateRequest, MergeStateResponse, PostRequest, ReceivePackResponse, RepoRef, SnapshotRef, SnapshotStep};

// Только простые имена веток — никаких путей/точек (защита от ref-инъекций).
fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.contains("..")
}

struct GitCoreSvc {
    pool: PgPool,
}

impl GitCoreSvc {
    // Резолв списка + загрузка всей истории версий (общее для всех RPC).
    async fn load(&self, owner: &str, slug: &str) -> Result<Vec<VersionData>, Status> {
        let (id, _v) = db::resolve_list(&self.pool, owner, slug)
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found("list not found"))?;
        db::load_bundle_data(&self.pool, id)
            .await
            .map_err(|e| Status::internal(e.to_string()))
    }

    // Общая реализация bundle: загрузка версий → материализация → git bundle.
    async fn build(&self, owner: &str, slug: &str) -> Result<Vec<u8>, Status> {
        let versions = self.load(owner, slug).await?;
        tokio::task::spawn_blocking(move || bundle::build_bundle(&versions))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(|e| Status::internal(e.to_string()))
    }

    // Персистентный bare-репо (bootstrap/append под локом) — общий вход read/write RPC.
    async fn ensure(&self, owner: &str, slug: &str) -> Result<(PathBuf, Uuid), Status> {
        repo::ensure_repo(&self.pool, owner, slug)
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found("list not found"))
    }
}

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

fn opt<'a>(s: &'a str) -> Option<&'a str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
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
                refs: st.refs.iter().map(|r| SnapshotRef { label: r.label.clone(), url: r.url.clone().unwrap_or_default() }).collect(),
            })
            .collect(),
    }
}

#[tonic::async_trait]
impl GitCore for GitCoreSvc {
    async fn info_refs_upload_pack(&self, req: Request<InfoRefsRequest>) -> Result<Response<BytesResponse>, Status> {
        let InfoRefsRequest { repo, git_protocol } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let data = tokio::task::spawn_blocking(move || smart_http::upload_pack_advertise(&bare, opt(&git_protocol)))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(BytesResponse { data }))
    }
    async fn info_refs_receive_pack(&self, req: Request<InfoRefsRequest>) -> Result<Response<BytesResponse>, Status> {
        let InfoRefsRequest { repo, git_protocol } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let data = tokio::task::spawn_blocking(move || smart_http::receive_pack_advertise(&bare, opt(&git_protocol)))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(BytesResponse { data }))
    }
    async fn upload_pack(&self, req: Request<PostRequest>) -> Result<Response<BytesResponse>, Status> {
        let PostRequest { repo, body, git_protocol } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let data = tokio::task::spawn_blocking(move || smart_http::upload_pack_rpc(&bare, &body, opt(&git_protocol)))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(BytesResponse { data }))
    }
    async fn receive_pack(&self, req: Request<PostRequest>) -> Result<Response<ReceivePackResponse>, Status> {
        let PostRequest { repo, body, git_protocol } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Критическая секция: receive-pack + проекция под одним локом репо
        // (ленивый append не вклинивается между приёмом и проекцией).
        let _guard = repo::repo_guard(&self.pool, id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let bare_recv = bare.clone();
        let data = tokio::task::spawn_blocking(move || smart_http::receive_pack_rpc(&bare_recv, &body, opt(&git_protocol)))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(|e| Status::internal(e.to_string()))?;
        // Проекция list.json запушенного tip → новая версия (0 = не спроецировано).
        let new_version = project::project_pushed_commit(&self.pool, id, &bare).await.unwrap_or(0);
        Ok(Response::new(ReceivePackResponse { data, new_version }))
    }
    async fn create_bundle(&self, req: Request<RepoRef>) -> Result<Response<BytesResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        // Персистентный репо (как TS bundleRepo) — bundle включает запушенные коммиты.
        let data = repo::bundle_repo(&self.pool, &owner, &slug)
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found("list not found"))?;
        Ok(Response::new(BytesResponse { data }))
    }

    async fn list_branches(&self, req: Request<RepoRef>) -> Result<Response<BranchesResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        let (bare, _id) = self.ensure(&owner, &slug).await?;
        // git2-объекты не Send → всё в spawn_blocking, наружу только owned-данные.
        let branches = tokio::task::spawn_blocking(move || -> Result<Vec<Branch>, String> {
            let repo = git2::Repository::open_bare(&bare).map_err(|e| e.to_string())?;
            let main_tip = repo.refname_to_id("refs/heads/main").map_err(|e| e.to_string())?;
            let mut out: Vec<Branch> = Vec::new();
            for b in repo.branches(Some(git2::BranchType::Local)).map_err(|e| e.to_string())? {
                let (branch, _) = b.map_err(|e| e.to_string())?;
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
        .await
        .map_err(|e| Status::internal(e.to_string()))?
        .map_err(Status::internal)?;
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
            .map_err(|e| Status::internal(e.to_string()))?;
        let Some(sn) = snap else {
            return Ok(Response::new(BranchSnapshotResponse { found: false, ..Default::default() }));
        };
        Ok(Response::new(to_snapshot_pb(sn)))
    }

    async fn create_branch(&self, req: Request<CreateBranchRequest>) -> Result<Response<BranchOpResponse>, Status> {
        let CreateBranchRequest { repo, name, from } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        let from = if from.is_empty() { "main".to_string() } else { from };
        if !valid_branch(&name) || !valid_branch(&from) {
            return Err(Status::invalid_argument("bad branch name"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        let tip = tokio::task::spawn_blocking(move || -> Result<String, Status> {
            let repo = git2::Repository::open_bare(&bare).map_err(|e| Status::internal(e.to_string()))?;
            let base = repo
                .refname_to_id(&format!("refs/heads/{from}"))
                .map_err(|_| Status::not_found("base branch not found"))?;
            let commit = repo.find_commit(base).map_err(|e| Status::internal(e.to_string()))?;
            // force=false: существующая ветка → ошибка (already_exists наружу).
            // .map(|_| ()) сразу дропает Branch<'_> (заимствует repo).
            match repo.branch(&name, &commit, false).map(|_| ()) {
                Ok(()) => Ok(base.to_string()),
                Err(e) if e.code() == git2::ErrorCode::Exists => Err(Status::already_exists("branch exists")),
                Err(e) => Err(Status::internal(e.to_string())),
            }
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(BranchOpResponse { tip_sha: tip }))
    }

    async fn delete_branch(&self, req: Request<DeleteBranchRequest>) -> Result<Response<BranchOpResponse>, Status> {
        let DeleteBranchRequest { repo, name } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        if !valid_branch(&name) {
            return Err(Status::invalid_argument("bad branch name"));
        }
        if name == "main" {
            return Err(Status::failed_precondition("main is protected"));
        }
        let (bare, _id) = self.ensure(&repo.owner, &repo.slug).await?;
        tokio::task::spawn_blocking(move || -> Result<(), Status> {
            let repo = git2::Repository::open_bare(&bare).map_err(|e| Status::internal(e.to_string()))?;
            let mut branch = repo
                .find_branch(&name, git2::BranchType::Local)
                .map_err(|_| Status::not_found("branch not found"))?;
            branch.delete().map_err(|e| Status::internal(e.to_string()))
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(BranchOpResponse { tip_sha: String::new() }))
    }

    async fn merge_branch(&self, req: Request<MergeBranchRequest>) -> Result<Response<MergeBranchResponse>, Status> {
        let MergeBranchRequest { repo, name } = req.into_inner();
        let repo = repo.ok_or_else(|| Status::invalid_argument("repo required"))?;
        if !valid_branch(&name) || name == "main" {
            return Err(Status::invalid_argument("bad branch name"));
        }
        let (bare, id) = self.ensure(&repo.owner, &repo.slug).await?;
        // Merge двигает main → критическая секция с проекцией (как receive_pack).
        let _guard = repo::repo_guard(&self.pool, id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let bare_merge = bare.clone();
        let (tip, ff) = tokio::task::spawn_blocking(move || -> Result<(String, bool), Status> {
            let repo = git2::Repository::open_bare(&bare_merge).map_err(|e| Status::internal(e.to_string()))?;
            let branch_tip = repo
                .refname_to_id(&format!("refs/heads/{name}"))
                .map_err(|_| Status::not_found("branch not found"))?;
            let main_tip = repo
                .refname_to_id("refs/heads/main")
                .map_err(|e| Status::internal(e.to_string()))?;
            let (ahead, _behind) = repo
                .graph_ahead_behind(branch_tip, main_tip)
                .map_err(|e| Status::internal(e.to_string()))?;
            if ahead == 0 {
                return Err(Status::failed_precondition("nothing-to-merge"));
            }
            // main — предок ветки → fast-forward: просто двигаем ref.
            if repo.graph_descendant_of(branch_tip, main_tip).unwrap_or(false) {
                repo.reference("refs/heads/main", branch_tip, true, &format!("merge {name}: fast-forward"))
                    .map_err(|e| Status::internal(e.to_string()))?;
                return Ok((branch_tip.to_string(), true));
            }
            // Расхождение → merge-commit; конфликт индекса = failed_precondition.
            let ours = repo.find_commit(main_tip).map_err(|e| Status::internal(e.to_string()))?;
            let theirs = repo.find_commit(branch_tip).map_err(|e| Status::internal(e.to_string()))?;
            let mut idx = repo
                .merge_commits(&ours, &theirs, None)
                .map_err(|e| Status::internal(e.to_string()))?;
            if idx.has_conflicts() {
                return Err(Status::failed_precondition("conflict"));
            }
            let tree_id = idx.write_tree_to(&repo).map_err(|e| Status::internal(e.to_string()))?;
            let tree = repo.find_tree(tree_id).map_err(|e| Status::internal(e.to_string()))?;
            let sig = git2::Signature::now("SetFork", "git@setfork.com").map_err(|e| Status::internal(e.to_string()))?;
            let msg = format!("Merge branch '{name}'");
            let merged = repo
                .commit(Some("refs/heads/main"), &sig, &sig, &msg, &tree, &[&ours, &theirs])
                .map_err(|e| Status::internal(e.to_string()))?;
            Ok((merged.to_string(), false))
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        // main сдвинулся → проекция новой версии (0 = list.json не изменился).
        let new_version = project::project_pushed_commit(&self.pool, id, &bare).await.unwrap_or(0);
        Ok(Response::new(MergeBranchResponse { tip_sha: tip, new_version, fast_forward: ff }))
    }

    async fn get_merge_state(&self, req: Request<MergeStateRequest>) -> Result<Response<MergeStateResponse>, Status> {
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
            let main_tip = repo.refname_to_id("refs/heads/main").map_err(|e| e.to_string())?;
            let Ok(base_oid) = repo.merge_base(main_tip, branch_tip) else {
                return Ok(None);
            };
            drop(repo); // commit_snapshot открывает репо сам
            let base = project::commit_snapshot(&bare, &base_oid.to_string());
            let ours = project::branch_snapshot(&bare, "refs/heads/main");
            let theirs = project::branch_snapshot(&bare, &format!("refs/heads/{branch}"));
            match (base, ours, theirs) {
                (Some(b), Some(o), Some(t)) => Ok(Some((base_oid.to_string(), b, o, t))),
                _ => Ok(None),
            }
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?
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

    async fn merge_resolved(&self, req: Request<MergeResolvedRequest>) -> Result<Response<MergeBranchResponse>, Status> {
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
        let _guard = repo::repo_guard(&self.pool, id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let bare_merge = bare.clone();
        let tip = tokio::task::spawn_blocking(move || -> Result<String, Status> {
            let repo = git2::Repository::open_bare(&bare_merge).map_err(|e| Status::internal(e.to_string()))?;
            let branch_tip = repo
                .refname_to_id(&format!("refs/heads/{branch}"))
                .map_err(|_| Status::not_found("branch not found"))?;
            let main_tip = repo
                .refname_to_id("refs/heads/main")
                .map_err(|e| Status::internal(e.to_string()))?;
            if main_tip == branch_tip {
                return Err(Status::failed_precondition("nothing-to-merge"));
            }
            // Дерево = дерево main c заменённым list.json и БЕЗ steps/ (см. proto).
            let ours = repo.find_commit(main_tip).map_err(|e| Status::internal(e.to_string()))?;
            let theirs = repo.find_commit(branch_tip).map_err(|e| Status::internal(e.to_string()))?;
            let blob = repo.blob(&list_json).map_err(|e| Status::internal(e.to_string()))?;
            let mut tb = repo
                .treebuilder(Some(&ours.tree().map_err(|e| Status::internal(e.to_string()))?))
                .map_err(|e| Status::internal(e.to_string()))?;
            tb.insert("list.json", blob, 0o100644).map_err(|e| Status::internal(e.to_string()))?;
            if tb.get("steps").map_err(|e| Status::internal(e.to_string()))?.is_some() {
                tb.remove("steps").map_err(|e| Status::internal(e.to_string()))?;
            }
            let tree_id = tb.write().map_err(|e| Status::internal(e.to_string()))?;
            let tree = repo.find_tree(tree_id).map_err(|e| Status::internal(e.to_string()))?;
            let sig = git2::Signature::now("SetFork", "git@setfork.com").map_err(|e| Status::internal(e.to_string()))?;
            let msg = format!("Merge branch '{branch}' (resolved)");
            let merged = repo
                .commit(Some("refs/heads/main"), &sig, &sig, &msg, &tree, &[&ours, &theirs])
                .map_err(|e| Status::internal(e.to_string()))?;
            Ok(merged.to_string())
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        let new_version = project::project_pushed_commit(&self.pool, id, &bare).await.unwrap_or(0);
        Ok(Response::new(MergeBranchResponse { tip_sha: tip, new_version, fast_forward: false }))
    }
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let pool = db::connect().await?;

    // CLI-режимы для golden-проверки (тот же код-пас, что и RPC, без gRPC-транспорта):
    //   bundle           <owner> <slug> <out>
    //   advertise-upload <owner> <slug> <out>   (== GET /info/refs?service=git-upload-pack)
    //   advertise-receive<owner> <slug> <out>   (== GET /info/refs?service=git-receive-pack)
    //   upload-pack      <owner> <slug> <body> <out>  (== POST /git-upload-pack)
    let args: Vec<String> = std::env::args().collect();
    if let Some(cmd) = args.get(1).map(|s| s.as_str()) {
        let cli = |i: usize| args.get(i).cloned().unwrap_or_default();
        // pool клонируется (Arc внутри) — оригинал остаётся серверу ниже.
        let svc = GitCoreSvc { pool: pool.clone() };
        match cmd {
            "bundle" => {
                let data = svc.build(&cli(2), &cli(3)).await.map_err(|e| e.to_string())?;
                std::fs::write(cli(4), &data)?;
                println!("wrote {} bytes → {}", data.len(), cli(4));
                return Ok(());
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
                return Ok(());
            }
            // Golden-сверка READ-портов: канонический JSON (см. domain_read::golden_json)
            //   domain-read <owner> <slug> <out.json>
            "domain-read" => {
                let v = domain_read::golden_json(&pool, &cli(2), &cli(3)).await.map_err(|e| e.to_string())?;
                std::fs::write(cli(4), serde_json::to_string_pretty(&v)?)?;
                println!("wrote domain-read json → {}", cli(4));
                return Ok(());
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
                return Ok(());
            }
            // не CLI-команда → падём в режим сервера ниже
            _ => {}
        }
    }

    let n = db::published_count(&pool).await?;
    println!("setfork-core: connected to Postgres — {n} published public lists");

    let addr = std::env::var("SETFORK_CORE_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:50051".to_string())
        .parse()?;

    // gRPC health-check (grpc.health.v1) — для проб оркестратора/LB.
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter.set_serving::<GitCoreServer<GitCoreSvc>>().await;

    // Graceful shutdown: по SIGTERM/SIGINT сперва снимаем SERVING (оркестратор
    // уводит трафик), затем serve_with_shutdown перестаёт принимать и до-обслуживает
    // текущие RPC (напр. идущий receive-pack не обрывается на середине).
    let shutdown = async move {
        wait_for_signal().await;
        let mut health_reporter = health_reporter;
        health_reporter.set_not_serving::<GitCoreServer<GitCoreSvc>>().await;
        println!("setfork-core: получен сигнал остановки — дренаж активных RPC…");
    };

    println!("setfork-core git-core listening on {addr}");
    // Auth канала Next↔ядро: общий Bearer-токен (SETFORK_CORE_TOKEN).
    // Токен не задан → канал открыт (локальный dev). Health остаётся без
    // авторизации — docker/k8s-пробам токен не раздаём.
    let token: Option<&'static str> = std::env::var("SETFORK_CORE_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(|t| &*Box::leak(format!("Bearer {t}").into_boxed_str()));
    if token.is_some() {
        println!("setfork-core: канал защищён Bearer-токеном");
    } else {
        println!("setfork-core: SETFORK_CORE_TOKEN не задан — канал без авторизации (dev)");
    }
    let check_auth = move |req: Request<()>| -> Result<Request<()>, Status> {
        let Some(expected) = token else { return Ok(req) };
        match req.metadata().get("authorization").and_then(|v| v.to_str().ok()) {
            // constant-time не нужен: токен длинный и случайный, тайминг не течёт полезно,
            // но сравнение всё равно полное (eq по всей строке).
            Some(got) if got == expected => Ok(req),
            _ => Err(Status::unauthenticated("invalid core token")),
        }
    };

    Server::builder()
        .layer(ratelimit::RateLimitLayer::from_env())
        .add_service(health_service)
        .add_service(GitCoreServer::with_interceptor(GitCoreSvc { pool: pool.clone() }, check_auth))
        .add_service(pb_domain::list_read_server::ListReadServer::with_interceptor(
            domain_read::ListReadSvc { pool: pool.clone() },
            check_auth,
        ))
        .add_service(pb_domain::curation_read_server::CurationReadServer::with_interceptor(
            domain_read::CurationReadSvc { pool: pool.clone() },
            check_auth,
        ))
        .add_service(pb_domain::curation_write_server::CurationWriteServer::with_interceptor(
            domain_read::CurationWriteSvc { pool: pool.clone() },
            check_auth,
        ))
        .add_service(pb_domain::collab_write_server::CollabWriteServer::with_interceptor(
            domain_write::CollabWriteSvc { pool: pool.clone() },
            check_auth,
        ))
        .add_service(pb_domain::list_write_server::ListWriteServer::with_interceptor(
            domain_write::ListWriteSvc { pool },
            check_auth,
        ))
        .serve_with_shutdown(addr, shutdown)
        .await?;
    println!("setfork-core: остановлен чисто");
    Ok(())
}

/// Ждёт первый из сигналов остановки: Ctrl-C (SIGINT) или SIGTERM (docker stop / k8s).
async fn wait_for_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("не удалось повесить обработчик Ctrl-C");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("не удалось повесить обработчик SIGTERM")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
