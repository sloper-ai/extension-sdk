use std::{
    error::Error,
    fmt::Write as _,
    fs,
    path::{
        Path,
        PathBuf,
    },
    process::Command,
};

use sha2::{
    Digest,
    Sha256,
};
use sloper_extension_host::{
    assemble_parts,
    check_component,
    extract_parts,
    stamp_manifest,
};

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
        return Err("guest build failed".into());
    }
    let bytes = fs::read(target.join("wasm32-wasip2/release/sloper_host_test_guest.wasm"))?;
    let parts = extract_parts(&bytes)?;
    let manifest = assemble_parts(&parts)?;
    let component = stamp_manifest(&bytes, &manifest)?;
    check_component(&component)?;
    let fixtures = guest.join("../fixtures");
    fs::create_dir_all(&fixtures)?;
    fs::write(fixtures.join("host-guest.wasm"), &component)?;
    let cli_fixtures = guest.join("../../../sloper-extension-cli/tests/fixtures");
    fs::create_dir_all(&cli_fixtures)?;
    fs::write(cli_fixtures.join("host-guest.wasm"), &component)?;
    let host = guest.join("../..");
    let mut inputs = vec![
        guest.join("build.rs"),
        guest.join("src/lib.rs"),
        guest.join("src/bin/build-fixture.rs"),
        guest.join("src/parts.json"),
        guest.join("Cargo.toml"),
        guest.join("Cargo.lock"),
    ];
    let spec = host.join("../sloper-extension-spec");
    inputs.push(spec.join("Cargo.toml"));
    files(&spec.join("src"), &mut inputs)?;
    files(&spec.join("wit"), &mut inputs)?;
    inputs.sort();
    let mut hash = Sha256::new();
    for input in inputs {
        hash.update(fs::read(input)?);
    }
    let mut digest = String::with_capacity(65);
    for byte in hash.finalize() {
        write!(digest, "{byte:02x}")?;
    }
    digest.push('\n');
    fs::write(fixtures.join("host-guest.sha256"), digest)?;
    println!("Validated host guest: {} bytes", component.len());
    Ok(())
}

fn files(directory: &Path, output: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            files(&path, output)?;
        } else {
            output.push(path);
        }
    }
    Ok(())
}
