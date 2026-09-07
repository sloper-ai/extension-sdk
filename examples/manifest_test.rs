use std::{
    fs,
    path::Path,
};

use serde_json::Value;
use sloper_extension_cli::{
    build,
    check,
    validate_file,
};
use sloper_extension_host::extract_manifest;

pub(crate) async fn build_and_verify(directory: &Path) -> Value {
    let first = build(directory).await.expect("public tooling builds the example");
    let first_bytes = fs::read(&first.component).expect("first distributable exists");
    let first_manifest = extract_manifest(&first_bytes)
        .expect("first distributable has a valid embedded manifest")
        .to_vec();
    let second = build(directory).await.expect("public tooling rebuilds the example");
    assert_eq!(first.component, second.component);
    let second_bytes = fs::read(&second.component).expect("second distributable exists");
    assert_eq!(
        first_manifest,
        extract_manifest(&second_bytes).expect("second distributable has a valid embedded manifest"),
        "two builds must embed identical manifest bytes"
    );
    let checked = check(directory)
        .await
        .expect("persisted distributable matches fresh assembly");
    let validated = validate_file(&second.component).expect("static publication validation accepts the example");
    let expected = serde_json::to_value(&first.check.manifest).expect("validated manifest serializes");
    for result in [&first.check, &second.check, &checked] {
        assert!(result.valid);
        assert_eq!(result.findings, []);
        assert_eq!(
            serde_json::to_value(&result.manifest).expect("validated manifest serializes"),
            expected
        );
    }
    assert!(validated.valid);
    assert_eq!(validated.findings, []);
    let manifest_text = validated
        .manifest
        .as_deref()
        .expect("accepted static validation retains the embedded manifest text");
    assert_eq!(
        manifest_text.as_bytes(),
        first_manifest,
        "static validation must retain exact manifest bytes, including formatting"
    );
    let manifest: Value = serde_json::from_str(manifest_text).expect("embedded manifest is valid JSON");
    assert_eq!(manifest, expected);
    assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
    manifest
}
