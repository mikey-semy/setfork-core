use sqlx::postgres::PgPool;
use tonic::{transport::Server, Request, Response, Status};

mod bundle;
mod db;

pub mod pb {
    tonic::include_proto!("setfork.git.v1");
}

use pb::git_core_server::{GitCore, GitCoreServer};
use pb::{BytesResponse, InfoRefsRequest, PostRequest, ReceivePackResponse, RepoRef};

struct GitCoreSvc {
    pool: PgPool,
}

impl GitCoreSvc {
    // Общая реализация bundle: резолв → загрузка версий → материализация → git bundle.
    async fn build(&self, owner: &str, slug: &str) -> Result<Vec<u8>, Status> {
        let (id, _v) = db::resolve_list(&self.pool, owner, slug)
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found("list not found"))?;
        let versions = db::load_bundle_data(&self.pool, id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        tokio::task::spawn_blocking(move || bundle::build_bundle(&versions))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(|e| Status::internal(e.to_string()))
    }
}

#[tonic::async_trait]
impl GitCore for GitCoreSvc {
    async fn info_refs_upload_pack(&self, req: Request<InfoRefsRequest>) -> Result<Response<BytesResponse>, Status> {
        let _ = req.into_inner();
        Err(Status::unimplemented("info_refs_upload_pack — Фаза 2 WIP"))
    }
    async fn info_refs_receive_pack(&self, req: Request<InfoRefsRequest>) -> Result<Response<BytesResponse>, Status> {
        let _ = req.into_inner();
        Err(Status::unimplemented("info_refs_receive_pack — Фаза 2 WIP"))
    }
    async fn upload_pack(&self, req: Request<PostRequest>) -> Result<Response<BytesResponse>, Status> {
        let _ = req.into_inner();
        Err(Status::unimplemented("upload_pack — Фаза 2 WIP"))
    }
    async fn receive_pack(&self, req: Request<PostRequest>) -> Result<Response<ReceivePackResponse>, Status> {
        let _ = req.into_inner();
        Err(Status::unimplemented("receive_pack — Фаза 2 WIP"))
    }
    async fn create_bundle(&self, req: Request<RepoRef>) -> Result<Response<BytesResponse>, Status> {
        let RepoRef { owner, slug } = req.into_inner();
        let data = self.build(&owner, &slug).await?;
        Ok(Response::new(BytesResponse { data }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    let pool = db::connect().await?;

    // CLI-режим для проверки: `setfork-core bundle <owner> <slug> <out.bundle>`
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("bundle") {
        let owner = args.get(2).expect("usage: bundle <owner> <slug> <out>");
        let slug = args.get(3).expect("usage: bundle <owner> <slug> <out>");
        let out = args.get(4).expect("usage: bundle <owner> <slug> <out>");
        let svc = GitCoreSvc { pool };
        let data = svc.build(owner, slug).await.map_err(|e| e.to_string())?;
        std::fs::write(out, &data)?;
        println!("wrote {} bytes → {}", data.len(), out);
        return Ok(());
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
