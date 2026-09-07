mod support {
    pub(crate) mod host_fixture;
}

use std::{
    error::Error as StdError,
    sync::{
        Arc,
        atomic::Ordering,
    },
};

use sloper_extension_host::{
    Engine,
    Error,
    HostError,
    Source,
};
use support::host_fixture::{
    SourceWriteFailure,
    TestHost,
    request,
    run,
};

#[tokio::test]
async fn successful_production_and_projected_read_items_lend_source_bytes() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost::default());
    run(&engine, request("source-produced"), Arc::clone(&host), None)
        .await
        .expect("produced bytes can be reopened in their attempt");
    host.sources
        .lock()
        .expect("source mutex is not poisoned")
        .insert("projected".into(), b"projected content".to_vec());
    run(&engine, request("reader"), host, None)
        .await
        .expect("read projection lends its source bytes");
}

#[tokio::test]
async fn source_production_rejects_schema_paths_and_undeclared_properties_before_sealing() {
    let engine = Engine::new().expect("engine config is valid");
    for property in [
        "/properties/document",
        "/properties/attachments/items/properties/document",
        "missing",
        "id",
    ] {
        let host = Arc::new(TestHost::default());
        let mut request = request("source-property");
        request.parameters = serde_json::json!({"property": property}).to_string();
        let result = run(&engine, request, Arc::clone(&host), None).await;
        assert!(
            matches!(result, Err(Error::SourceFailed)),
            "property={property}, result={result:?}"
        );
        assert!(host.sources.lock().expect("source mutex is not poisoned").is_empty());
        assert_eq!(host.finishes.load(Ordering::Acquire), 0);
    }
}

#[tokio::test]
async fn ignored_failed_source_production_prevents_success() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost {
        source_failure: true,
        ..TestHost::default()
    });
    let result = run(&engine, request("source-failed"), Arc::clone(&host), None).await;
    assert!(matches!(result, Err(Error::SourceFailed)), "result={result:?}");
    assert_eq!(host.finishes.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn source_read_transport_errors_cannot_become_successful_eof() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost {
        unreadable: true,
        ..TestHost::default()
    });
    let mut input = request("source-read");
    input.parameters = "{\"document\":\"lent\"}".into();
    input.sources.push(Source {
        id: "lent".into(),
        filename: "lent.txt".into(),
        media_type: "text/plain".into(),
        size: 5,
    });
    let result = run(&engine, input, Arc::clone(&host), None).await;
    let error = result.expect_err("source transport errors prevent success");
    assert!(matches!(error, Error::SourceRead(_)), "error={error:?}");
    let mut source = error.source();
    let mut retained = false;
    while let Some(cause) = source {
        retained |= cause.to_string().contains("injected source transport failure");
        source = cause.source();
    }
    assert!(retained, "source read error lost its native transport cause: {error:?}");
    assert_eq!(host.finishes.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn ignored_source_size_failure_never_seals_truncated_bytes() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost::default());
    let result = run(&engine, request("source-oversize"), Arc::clone(&host), None).await;
    assert!(matches!(result, Err(Error::LimitExceeded)), "result={result:?}");
    assert!(host.sources.lock().expect("source mutex is not poisoned").is_empty());
    assert_eq!(host.finishes.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn produced_source_storage_failure_retains_its_native_cause() {
    let engine = Engine::new().expect("engine config is valid");
    for failure in [SourceWriteFailure::BeforeRead, SourceWriteFailure::AfterRead] {
        let host = Arc::new(TestHost {
            source_write_failure: failure,
            ..TestHost::default()
        });
        let result = run(&engine, request("source-produced"), Arc::clone(&host), None).await;
        let Err(Error::Host(HostError::SourceWrite(cause))) = result else {
            panic!("result={result:?}")
        };
        assert_eq!(cause.to_string(), "injected produced-source storage failure");
        assert!(host.sources.lock().expect("source mutex is not poisoned").is_empty());
        assert_eq!(host.finishes.load(Ordering::Acquire), 0);
    }
}
