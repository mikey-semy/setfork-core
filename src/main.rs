use sqlx::postgres::PgPool;
use tonic::{transport::Server, Request, Response, Status};

mod db;

pub mod pb {
    tonic::include_proto!("setfork.git.v1");
}

use pb::git_core_server::{GitCore, GitCoreServer};
use pb::{BytesResponse, InfoRefsRequest, PostRequest, ReceivePackResponse, RepoRef};

// Скелет git-ядра. Держит пул Postgres (для резолва/проекции). RPC пока unimplemented;
// реализация (материализация репо + gix/git2 + проекция) добавляется послойно.
struct GitCoreSvc {
    #[allow(dead_code)]
    pool: PgPool,
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
        let _ = req.into_inner();
        Err(Status::unimplemented("create_bundle — Фаза 2 WIP"))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    // Подключение к Postgres + self-check (доказательство pipeline Rust↔БД).
    let pool = db::connect().await?;
    let n = db::published_count(&pool).await?;
    println!("setfork-core: connected to Postgres — {n} published public lists");
    if let Some((id, ver)) = db::resolve_list(&pool, "demo", "redis-caching-setup-for-web-apps").await? {
        println!("setfork-core: resolved demo/redis-caching-setup-for-web-apps → {id} (v{ver})");
    }

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
