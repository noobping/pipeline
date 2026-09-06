use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let manifest_path = manifest_dir.join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", manifest_path.display());

    let source = fs::read_to_string(&manifest_path).expect("read Cargo.toml");
    let manifest: toml::Value = toml::from_str(&source).expect("parse Cargo.toml");
    let metadata = manifest
        .get("package")
        .and_then(|value| value.get("metadata"))
        .and_then(|value| value.get("pipeline"))
        .expect("[package.metadata.pipeline]");

    emit(metadata, "just-image", "PIPELINE_JUST_IMAGE");
    emit(metadata, "just-min-version", "PIPELINE_JUST_MIN_VERSION");
}

fn emit(metadata: &toml::Value, key: &str, environment: &str) {
    let value = metadata
        .get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("package.metadata.pipeline.{key} must be a string"));
    println!("cargo:rustc-env={environment}={value}");
}
