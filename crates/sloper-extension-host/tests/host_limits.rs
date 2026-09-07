mod support {
    pub(crate) mod host_fixture;
}

use std::sync::Arc;

use sloper_extension_host::{
    Engine,
    Error,
};
use support::host_fixture::{
    TestHost,
    request,
    run,
};

const MIB: usize = 1024 * 1024;

#[tokio::test]
async fn item_and_batch_byte_limits_are_exact_before_and_after_redaction() {
    let engine = Engine::new().expect("engine config is valid");
    let mut over_batch = vec![MIB; 7];
    over_batch.extend([MIB - 23, 24]);
    let mut redacted_over_batch = vec![MIB - 9; 7];
    redacted_over_batch.extend([MIB - 64, 47]);
    for (sizes, redact, accepted) in [
        (vec![MIB], false, true),
        (vec![MIB + 1], false, false),
        (vec![MIB; 8], false, true),
        (over_batch, false, false),
        (vec![MIB - 8], true, false),
        (vec![MIB - 9; 8], true, true),
        (redacted_over_batch, true, false),
    ] {
        let host = Arc::new(TestHost {
            token: Some("x".into()),
            ..TestHost::default()
        });
        let mut input = request("write-bytes");
        input.parameters =
            serde_json::json!({"sizes":sizes,"redact":redact,"tokenCount":usize::from(redact)}).to_string();
        let result = run(&engine, input, Arc::clone(&host), None).await;
        if accepted {
            result.expect("exact item and batch byte ceilings accept");
            let items = host.written_items.lock().expect("items mutex");
            assert_eq!(items.len(), sizes.len());
            assert!(items.iter().all(|item| item.len() <= MIB));
            assert!(items.iter().map(String::len).sum::<usize>() <= 8 * MIB);
        } else {
            assert!(matches!(result, Err(Error::LimitExceeded)), "result={result:?}");
            assert!(
                host.written_items.lock().expect("items mutex").is_empty(),
                "invalid complete call reached storage"
            );
        }
    }
}

#[tokio::test]
async fn projected_pages_enforce_exact_item_and_page_byte_limits() {
    let engine = Engine::new().expect("engine config is valid");
    let mut over_page = vec![MIB; 7];
    over_page.extend([MIB - 23, 24]);
    for (sizes, accepted) in [
        (vec![MIB], true),
        (vec![MIB + 1], false),
        (vec![MIB; 8], true),
        (over_page, false),
    ] {
        let host = Arc::new(TestHost {
            read_sizes: sizes,
            ..TestHost::default()
        });
        let result = run(&engine, request("read-bytes"), host, None).await;
        if accepted {
            result.expect("exact page and item limits accept");
        } else {
            assert!(matches!(result, Err(Error::LimitExceeded)), "result={result:?}");
        }
    }
}

#[tokio::test]
async fn guest_memory_allocation_cannot_exceed_host_ceiling() {
    let engine = Engine::new().expect("engine config is valid");
    let result = run(&engine, request("memory"), Arc::new(TestHost::default()), None).await;
    assert!(matches!(result, Err(Error::LimitExceeded)), "result={result:?}");
}
