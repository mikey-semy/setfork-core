// Кодоген из proto/*.proto через tonic-build.
// protoc берём из крейта protoc-bin-vendored — системный protoc не нужен.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    std::env::set_var("PROTOC", protoc);
    tonic_build::compile_protos("proto/git.proto")?;
    tonic_build::compile_protos("proto/domain_read.proto")?;
    Ok(())
}
