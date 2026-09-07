use std::{
    error::Error,
    fs,
    path::PathBuf,
    process::Command,
};

use sloper_extension_host::{
    assemble_parts,
    check_component,
    extract_parts,
    stamp_manifest,
};
#[path = "../../inputs.rs"]
mod inputs;

fn main() -> Result<(), Box<dyn Error>> {
    let guest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = guest.join("target");
    let status = Command::new("cargo")
        .args([
            "build",
            "-Ztrim-paths",
            "--config",
            "profile.release.trim-paths='object'",
            "--release",
            "--locked",
            "--target",
            "wasm32-wasip2",
            "--lib",
            "--manifest-path",
        ])
        .arg(guest.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target)
        .status()?;
    if !status.success() {
        return Err("SDK guest build failed".into());
    }
    let bytes = fs::read(target.join("wasm32-wasip2/release/sloper_sdk_test_guest.wasm"))?;
    let parts = extract_parts(&bytes)?;
    let manifest = assemble_parts(&parts)?;
    let component = stamp_manifest(&bytes, &manifest)?;
    check_component(&component)?;
    let fixtures = guest.join("../fixtures");
    fs::create_dir_all(&fixtures)?;
    fs::write(fixtures.join("sdk-guest.wasm"), &component)?;
    fs::write(fixtures.join("sdk-guest.sha256"), inputs::digest(&guest.join("../.."))?)?;
    println!("Validated SDK guest: {} bytes", component.len());
    Ok(())
}
