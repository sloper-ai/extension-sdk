use std::{
    fs,
    path::Path,
    process::Command,
};

use serde_json::Value;
use wasmparser::{
    Parser,
    Payload,
};

const BINARY: &str = env!("CARGO_BIN_EXE_sloper-extension");

#[test]
fn build_overrides_debug_profiles_and_preserves_a_runnable_component() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let sdk = workspace.join("crates/sloper-extension");
    fs::create_dir(directory.path().join("src")).unwrap();
    fs::write(
        directory.path().join("Cargo.toml"),
        format!(
            r#"[package]
name = "optimized-component"
version = "0.0.1"
description = "Verify optimized distributable components."
edition = "2024"

[lib]
crate-type = ["cdylib"]

[workspace]

[dependencies]
sloper-extension = {{ path = {} }}

[profile.release]
opt-level = 0
debug = 2
strip = "none"
lto = false
codegen-units = 16
"#,
            serde_json::to_string(&sdk).unwrap()
        ),
    )
    .unwrap();
    fs::copy(workspace.join("Cargo.lock"), directory.path().join("Cargo.lock")).unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        r#"use sloper_extension::{action, extension, Result};

#[action]
async fn execute() -> Result<()> { Ok(()) }

extension! { name: "test.optimized", actions: [execute] }
"#,
    )
    .unwrap();

    // Sharing dependency artifacts keeps this real Cargo build inexpensive on
    // reruns.
    let target = workspace.join("target");
    for command in ["build", "check"] {
        let output = Command::new(BINARY)
            .args([command, "--json"])
            .arg(directory.path())
            .env("CARGO_TARGET_DIR", &target)
            .env("CARGO_PROFILE_RELEASE_OPT_LEVEL", "0")
            .env("CARGO_PROFILE_RELEASE_DEBUG", "2")
            .env("CARGO_PROFILE_RELEASE_STRIP", "none")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "command={command}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        let valid = if command == "build" {
            &response["check"]["valid"]
        } else {
            &response["valid"]
        };
        assert_eq!(valid, true, "command={command}, response={response}");
    }
    let component = fs::read(directory.path().join("dist/extension.wasm")).unwrap();
    for payload in Parser::new(0).parse_all(&component) {
        if let Payload::CustomSection(section) = payload.unwrap() {
            assert!(
                section.name() != "name"
                    && !section.name().starts_with(".debug_")
                    && !section.name().starts_with(".zdebug_"),
                "section={}",
                section.name()
            );
        }
    }
    let output = Command::new(BINARY)
        .args(["run", "execute", "--json", "--directory"])
        .arg(directory.path())
        .arg("--out")
        .arg(directory.path().join("result"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["operation"]["state"], "SUCCEEDED");
}
