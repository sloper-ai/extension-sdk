mod support {
    pub(crate) mod host_fixture;
}

use std::{
    fs,
    path::PathBuf,
    sync::Arc,
};

use sloper_extension_host::Engine;
use support::host_fixture::{
    TestHost,
    request,
    run,
};

#[test]
fn compiled_fixture_omits_machine_source_paths() {
    let host = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let component = fs::read(host.join("tests/fixtures/host-guest.wasm")).expect("fixture exists");
    for prefix in ["/Users/", "/Volumes/", "/home/", "\\Users\\", "/cargo/build-dir/"] {
        assert!(
            !component.windows(prefix.len()).any(|bytes| bytes == prefix.as_bytes()),
            "fixture contains a machine-specific source path; regenerate with remapped compiler paths"
        );
    }
    assert!(component.windows(7).any(|bytes| bytes == b"/rustc/"));
    assert!(component.windows(7).any(|bytes| bytes == b"/cargo/"));
}

#[tokio::test]
async fn every_attempt_has_a_fresh_guest_heap() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost::default());
    run(&engine, request("counter"), Arc::clone(&host), None)
        .await
        .expect("first instance counter starts at zero");
    run(&engine, request("counter"), host, None)
        .await
        .expect("second instance counter starts at zero");
}
