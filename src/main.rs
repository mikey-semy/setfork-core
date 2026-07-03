use tonic::{transport::Server, Request, Response, Status};

pub mod pb {
    tonic::include_proto!("setfork.git.v1");
}

use pb::git_core_server::{GitCore, GitCoreServer};
use pb::{BytesResponse, InfoRefsRequest, PostRequest, ReceivePackResponse, RepoRef};

// Скелет git-ядра. Пока все RPC — unimplemented; реализация (gix read / git2 write
// + sqlx-проекция в Postgres) добавляется послойно. Контракт = proto/git.proto.
#[derive(Default)]
struct GitCoreSvc;

#[tonic::async_trait]
impl GitCore for GitCoreSvc {
    async fn info_refs_upload_pack(
        &self,
        req: Request<InfoRefsRequest>,
    ) -> Result<Response<BytesResponse>, Status> {
        let _ = req.into_inner();
        Err(Status::unimplemented("info_refs_upload_pack — Фаза 2 WIP"))
    }

    async fn info_refs_receive_pack(
        &self,
        req: Request<InfoRefsRequest>,
    ) -> Result<Response<BytesResponse>, Status> {
        let _ = req.into_inner();
        Err(Status::unimplemented("info_refs_receive_pack — Фаза 2 WIP"))
    }

    async fn upload_pack(
        &self,
        req: Request<PostRequest>,
    ) -> Result<Response<BytesResponse>, Status> {
        let _ = req.into_inner();
        Err(Status::unimplemented("upload_pack — Фаза 2 WIP"))
    }

    async fn receive_pack(
        &self,
        req: Request<PostRequest>,
    ) -> Result<Response<ReceivePackResponse>, Status> {
        let _ = req.into_inner();
        Err(Status::unimplemented("receive_pack — Фаза 2 WIP"))
    }

    async fn create_bundle(
        &self,
        req: Request<RepoRef>,
    ) -> Result<Response<BytesResponse>, Status> {
        let _ = req.into_inner();
        Err(Status::unimplemented("create_bundle — Фаза 2 WIP"))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("SETFORK_CORE_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:50051".to_string())
        .parse()?;
    println!("setfork-core git-core (skeleton) listening on {addr}");
    Server::builder()
        .add_service(GitCoreServer::new(GitCoreSvc::default()))
        .serve(addr)
        .await?;
    Ok(())
}
