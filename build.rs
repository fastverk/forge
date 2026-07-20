fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[
                "proto/forge/v1/forge.proto",
                "proto/forge/v1/provision.proto",
            ],
            &["proto"],
        )?;
    Ok(())
}
