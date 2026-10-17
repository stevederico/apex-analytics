fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Vendored protoc so builders/Docker need no system protobuf-compiler.
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    // SAFETY: build script only; protoc path is a static vendored binary path.
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/admin.proto"], &["proto"])?;
    Ok(())
}
