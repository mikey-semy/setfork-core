use sqlx::postgres::PgPool;
use tonic::{transport::Server, Request, Response, Status};

mod bundle;
mod db;
mod project;
mod repo;
mod smart_http;

use bundle::VersionData;
use std::path::PathBuf;
use uuid::Uuid;

pub mod pb {
    tonic::include_proto!("setfork.git.v1");
}

use pb::git_core_server::{GitCore, GitCoreServer};
use pb::{BytesResponse, InfoRefsRequest, PostRequest, ReceivePackResponse, RepoRef};

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
        let _guard = repo::repo_lock(id).await;
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
    println!("setfork-core git-core listening on {addr}");
    Server::builder()
        .add_service(GitCoreServer::new(GitCoreSvc { pool }))
        .serve(addr)
        .await?;
    Ok(())
}
