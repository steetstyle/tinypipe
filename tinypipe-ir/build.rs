use std::path::Path;

fn main() {
    // Tell cargo to rerun this build script if the .fbs file changes
    let schema_path = Path::new("schemas/execution_plan.fbs");
    println!("cargo:rerun-if-changed={}", schema_path.display());

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");

    // Run flatc to generate Rust bindings
    let status = std::process::Command::new("flatc")
        .args(&["--rust", "-o", &out_dir, schema_path.to_str().unwrap()])
        .status()
        .expect("failed to run flatc — is flatbuffers-compiler installed?");

    assert!(status.success(), "flatc compilation failed");
}
