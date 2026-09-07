#![cfg(not(target_arch = "wasm32"))]

#[path = "../../manifest_test.rs"]
mod manifest_test;

#[tokio::test]
async fn foreground_component_is_deterministic_and_admitted() {
    let manifest = manifest_test::build_and_verify(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))).await;
    assert_eq!(manifest["name"], "acme.echo");
    assert_eq!(manifest["actions"].as_object().unwrap().len(), 1);
    assert_eq!(
        manifest["actions"]["echo"]["parameters"]["properties"]["name"]["maxLength"],
        100
    );
    assert_eq!(
        manifest["actions"]["echo"]["parameters"]["required"],
        serde_json::json!(["name"])
    );
    assert!(manifest["resources"].as_object().is_none_or(serde_json::Map::is_empty));
    assert!(
        manifest["connections"]
            .as_object()
            .is_none_or(serde_json::Map::is_empty)
    );
    let output = tempfile::tempdir().expect("local execution directory exists");
    let result = sloper_extension_cli::run(sloper_extension_cli::RunOptions {
        directory: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        action: "echo".into(),
        out: output.path().join("result"),
        parameters: serde_json::json!({"name":"Ada"}),
        configuration: serde_json::json!({}),
        sources: std::collections::BTreeMap::new(),
        resources: std::collections::BTreeMap::new(),
    })
    .await
    .expect("public local runner executes the built greeting");
    assert_eq!(result.operation["state"], "SUCCEEDED");
}
