fn main() {
    tonic_build::configure()
        .compile(
            &["src/protos/sandbox.proto", "src/protos/ssi.proto"],
            &["src/protos"],
        )
        .expect("tonic proto gen failed");
}
