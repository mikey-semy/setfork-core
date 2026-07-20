// Кодоген из proto/*.proto: protox (чистый Rust) компилирует дескрипторы,
// tonic-prost-build генерит сервисы — protoc-бинарь не нужен вовсе.
use prost::Message;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fds = protox::compile(["proto/git.proto", "proto/domain_read.proto"], ["proto"])?;
    // Дескрипторы — в OUT_DIR для gRPC server reflection (tonic-reflection):
    // grpcurl/grpcui на проде работают без локальных proto-файлов.
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("descriptor.bin"), fds.encode_to_vec())?;
    tonic_prost_build::configure().compile_fds(fds)?;
    Ok(())
}
