use std::{
    fs,
    process::Command,
};

use serde_json::Value;
use sloper_extension_host::{
    extract_manifest,
    stamp_manifest,
};

const BINARY: &str = env!("CARGO_BIN_EXE_sloper-extension");
const COMPONENT: &[u8] = include_bytes!("fixtures/host-guest.wasm");

fn whitespace_manifest_component() -> (Vec<u8>, Vec<u8>) {
    let manifest = [
        b" \n".as_slice(),
        extract_manifest(COMPONENT).unwrap(),
        b"\n\t".as_slice(),
    ]
    .concat();
    let mut unstamped = COMPONENT[..8].to_vec();
    let mut reader = wasmparser::BinaryReader::new(&COMPONENT[8..], 8);
    while !reader.eof() {
        let start = usize::try_from(reader.original_position()).unwrap();
        let id = reader.read_u8().unwrap();
        let length = reader.read_var_u32().unwrap() as usize;
        let payload = reader.read_bytes(length).unwrap();
        if id != 0 || wasmparser::BinaryReader::new(payload, 0).read_string().unwrap() != "sloper:manifest" {
            unstamped.extend_from_slice(&COMPONENT[start..usize::try_from(reader.original_position()).unwrap()]);
        }
    }
    (stamp_manifest(&unstamped, &manifest).unwrap(), manifest)
}

fn append_section(component: &mut Vec<u8>, id: u8, payload: &[u8]) {
    component.push(id);
    leb(payload.len(), component);
    component.extend_from_slice(payload);
}

fn leb(mut value: usize, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f).to_le_bytes()[0];
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            return;
        }
    }
}

fn trapping_component() -> Vec<u8> {
    let mut modules = 0;
    let mut depth = 0;
    for payload in wasmparser::Parser::new(0).parse_all(COMPONENT) {
        match payload.unwrap() {
            wasmparser::Payload::ModuleSection {
                ..
            } => {
                if depth == 0 {
                    modules += 1;
                }
                depth += 1;
            },
            wasmparser::Payload::ComponentSection {
                ..
            } => depth += 1,
            wasmparser::Payload::End(_) if depth > 0 => depth -= 1,
            _ => {},
        }
    }
    let mut component = COMPONENT.to_vec();
    let trap = wat::parse_str("(module (func $start unreachable) (start $start))").unwrap();
    append_section(&mut component, 1, &trap);
    let mut instance = vec![1, 0];
    leb(modules, &mut instance);
    instance.push(0);
    append_section(&mut component, 2, &instance);
    component
}

#[test]
fn process_preserves_exact_manifest_text_and_rejects_invalid_input() {
    let (component, manifest) = whitespace_manifest_component();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("component.wasm");
    fs::write(&path, component).unwrap();
    let output = Command::new(BINARY)
        .arg("validate")
        .arg(&path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr={:?}", output.stderr);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["valid"], true);
    assert_eq!(result["findings"], serde_json::json!([]));
    assert_eq!(result["manifest"].as_str().unwrap().as_bytes(), manifest);

    fs::write(&path, b"invalid").unwrap();
    let output = Command::new(BINARY)
        .arg("validate")
        .arg(&path)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["valid"], false);
    assert!(result["manifest"].is_null());
    assert_ne!(result["findings"].as_array().unwrap().as_slice(), [] as [Value; 0]);
    assert!(output.stdout.len() < 4096);
}

#[test]
fn process_distinguishes_usage_and_operational_failures() {
    for arguments in [vec![], vec!["validate"], vec!["validate", "one.wasm", "two.wasm"]] {
        let output = Command::new(BINARY).args(arguments).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stdout, [] as [u8; 0]);
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("missing.wasm");
    let output = Command::new(BINARY)
        .arg("validate")
        .arg(&path)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(10));
    assert_eq!(output.stdout, [] as [u8; 0]);
    assert!(
        !String::from_utf8(output.stderr)
            .unwrap()
            .contains(path.to_str().unwrap())
    );
}

#[test]
fn process_bounds_input_before_parsing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("oversized.wasm");
    fs::File::create(&path).unwrap().set_len(64 * 1024 * 1024 + 1).unwrap();
    let output = Command::new(BINARY)
        .arg("validate")
        .arg(path)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["valid"], false);
    assert!(result["manifest"].is_null());
    assert_eq!(result["findings"][0]["code"], "MANIFEST_TOO_LARGE");
}

#[tokio::test]
async fn validate_command_does_not_instantiate_a_trapping_start() {
    let component = trapping_component();
    let file = tempfile::NamedTempFile::new().unwrap();
    fs::write(file.path(), &component).unwrap();
    let output = Command::new(BINARY)
        .arg("validate")
        .arg(file.path())
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr={:?}", output.stderr);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["valid"], true);
    assert_eq!(
        result["manifest"].as_str().unwrap().as_bytes(),
        extract_manifest(&component).unwrap()
    );
    let engine = sloper_extension_host::Engine::new().unwrap();
    assert!(engine.admit_component(&component).await.is_err());
}
