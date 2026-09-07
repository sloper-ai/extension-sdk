#![cfg(not(target_arch = "wasm32"))]

#[path = "../../manifest_test.rs"]
mod manifest_test;

#[tokio::test]
async fn read_write_component_has_one_keyed_resource_and_no_parameters() {
    let manifest = manifest_test::build_and_verify(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).await;
    assert_eq!(manifest["name"], "acme.invoice-labels");
    assert_eq!(manifest["actions"].as_object().unwrap().len(), 1);
    let action = &manifest["actions"]["normalize"];
    assert_eq!(action["mode"], "background");
    assert_eq!(action["resources"]["invoices"], serde_json::json!(["read", "write"]));
    assert!(action.get("parameters").is_none_or(serde_json::Value::is_null));
    assert_eq!(manifest["resources"].as_object().unwrap().len(), 1);
    let resource = &manifest["resources"]["invoices"];
    assert_eq!(resource["key"], "id");
    assert_eq!(resource["capabilities"], serde_json::json!(["read", "write"]));
    assert_eq!(resource["schema"]["required"], serde_json::json!(["id", "label"]));
    assert_eq!(resource["schema"]["properties"]["id"]["maxLength"], 512);
    let output = tempfile::tempdir().expect("local execution directory exists");
    let input = output.path().join("invoices.jsonl");
    std::fs::write(&input, b"{\"id\":\"invoice-1\",\"label\":\"  Example invoice  \"}\n")
        .expect("resource fixture is written");
    let result = sloper_extension_cli::run(sloper_extension_cli::RunOptions {
        directory: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        action: "normalize".into(),
        out: output.path().join("result"),
        parameters: serde_json::json!({}),
        configuration: serde_json::json!({}),
        sources: std::collections::BTreeMap::new(),
        resources: std::collections::BTreeMap::from([("invoices".into(), input)]),
    })
    .await
    .expect("public local runner executes the built resource action");
    assert_eq!(result.operation["state"], "SUCCEEDED");
    assert_eq!(result.resources.len(), 1);
    assert_eq!(result.resources[0].items, "1");
    let bytes = std::fs::read(&result.resources[0].path).expect("normalized resource is exported");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("resource output is JSONL");
    assert_eq!(value, serde_json::json!({"id":"invoice-1","label":"Example invoice"}));
}
