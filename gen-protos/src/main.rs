use std::io::Result;
use std::path::Path;

fn main() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let protos_dir = root.join("src").join("protos");

    prost_build::Config::new()
        .out_dir(&protos_dir)
        .compile_protos(&[protos_dir.join("change_request.proto")], &[&protos_dir])
}
