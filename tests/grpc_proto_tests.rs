//! The checked-in gRPC code must be generated from the current
//! `proto/drevo.proto` (issue #583). `scripts/gen-grpc.sh` writes the
//! .proto's CRC-32 into the generated file's header; editing the .proto
//! without regenerating fails here, in every build (no `grpc` feature or
//! protoc needed).

use std::path::Path;

#[test]
fn generated_code_matches_the_proto() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let proto = std::fs::read(root.join("proto/drevo.proto")).expect("proto");
    let generated =
        std::fs::read_to_string(root.join("src/grpc/drevo.v1.rs")).expect("generated code");
    let recorded = generated
        .lines()
        .find_map(|l| l.strip_prefix("// proto-crc32: "))
        .expect("generated file records the proto checksum");
    let actual = format!("{:08x}", drevo::wal_format::crc32(&proto));
    assert_eq!(
        recorded, actual,
        "src/grpc/drevo.v1.rs is stale: run scripts/gen-grpc.sh"
    );
}
