//! Inputs which define the checked-in public SDK execution fixture.

use std::{
    error::Error,
    fs,
    path::{
        Path,
        PathBuf,
    },
};

use sha2::{
    Digest,
    Sha256,
};

pub(crate) fn digest(host: &Path) -> Result<String, Box<dyn Error>> {
    let guest = host.join("tests/sdk-guest");
    let sdk = host.join("../..");
    let mut inputs = vec![
        guest.join("src/lib.rs"),
        guest.join("src/bin/build-sdk-fixture.rs"),
        guest.join("inputs.rs"),
        guest.join("Cargo.toml"),
        guest.join("Cargo.lock"),
        sdk.join("Cargo.toml"),
        sdk.join("Cargo.lock"),
        sdk.join("rust-toolchain.toml"),
    ];
    for name in ["sloper-extension", "sloper-extension-macros", "sloper-extension-spec"] {
        let package = sdk.join("crates").join(name);
        inputs.push(package.join("Cargo.toml"));
        collect(&package.join("src"), &mut inputs)?;
    }
    inputs.push(sdk.join("crates/sloper-extension/build.rs"));
    collect(&sdk.join("crates/sloper-extension-spec/wit"), &mut inputs)?;
    inputs.sort();
    let mut digest = Sha256::new();
    for input in inputs {
        digest.update(fs::read(input)?);
    }
    let mut text = String::new();
    for byte in digest.finalize() {
        use std::fmt::Write as _;
        write!(&mut text, "{byte:02x}")?;
    }
    text.push('\n');
    Ok(text)
}

fn collect(directory: &Path, output: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, output)?;
        } else {
            output.push(path);
        }
    }
    Ok(())
}
