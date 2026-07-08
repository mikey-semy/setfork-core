// Кодоген из proto/*.proto: protox (чистый Rust) компилирует дескрипторы,
// tonic-prost-build генерит сервисы — protoc-бинарь не нужен вовсе.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fds = protox::compile(["proto/git.proto", "proto/domain_read.proto"], ["proto"])?;
    tonic_prost_build::configure().compile_fds(fds)?;
    Ok(())
}
