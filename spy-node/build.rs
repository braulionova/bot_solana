fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_client(true)
        .build_server(false)
        .compile(
            &[
                "proto/auth.proto",
                "proto/shredstream.proto",
                "proto/packet.proto",
                "proto/searcher.proto",
            ],
            &["proto"],
        )?;
    Ok(())
}
