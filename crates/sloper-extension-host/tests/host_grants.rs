mod support {
    pub(crate) mod host_fixture;
}

use std::sync::{
    Arc,
    atomic::Ordering,
};

use sloper_extension_host::Engine;
use support::host_fixture::{
    TestHost,
    request,
    run,
};

#[tokio::test]
async fn action_grants_deny_other_actions_capabilities_before_callbacks() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost::default());
    run(&engine, request("denied"), Arc::clone(&host), None)
        .await
        .expect("guest observes capability denials");
    assert_eq!(host.token_calls.load(Ordering::Acquire), 0);
    assert_eq!(host.source_calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn duplicate_resource_writers_stay_denied_after_drop() {
    let engine = Engine::new().expect("engine config is valid");
    run(&engine, request("writer"), Arc::new(TestHost::default()), None)
        .await
        .expect("writer cannot be reopened in one attempt");
}
