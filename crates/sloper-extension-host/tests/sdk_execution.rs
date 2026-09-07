use std::{
    collections::BTreeMap,
    fs,
    io,
    path::Path,
    pin::Pin,
    sync::{
        Arc,
        Mutex,
        atomic::{
            AtomicUsize,
            Ordering,
        },
    },
};

use async_trait::async_trait;
use sloper_extension_host::{
    AccessToken,
    Engine,
    Error,
    Failure,
    Host,
    HostError,
    Item,
    LogKind,
    LogLevel,
    Page,
    Request,
    validate_component,
};
use tokio::{
    io::{
        AsyncRead,
        AsyncReadExt,
    },
    sync::{
        mpsc,
        watch,
    },
};

#[derive(Default)]
struct SdkHost {
    reads: AtomicUsize,
    writes: Mutex<Vec<Vec<String>>>,
    checkpoints: Mutex<Vec<String>>,
    sources: Mutex<BTreeMap<String, Vec<u8>>>,
    produced: AtomicUsize,
    source_filenames: Mutex<Vec<String>>,
    logs: Mutex<Vec<String>>,
}
#[async_trait]
impl Host for SdkHost {
    async fn access_token(&self, connection: &str) -> Result<AccessToken, HostError> {
        if connection != "ledger" {
            return Err(HostError::unauthorized());
        }
        Ok(AccessToken {
            value: "sdk-secret-token".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            scopes: vec!["items.read".into()],
        })
    }

    async fn open_source(&self, id: &str) -> Result<Pin<Box<dyn AsyncRead + Send>>, HostError> {
        let bytes = self
            .sources
            .lock()
            .expect("source mutex")
            .get(id)
            .cloned()
            .ok_or_else(HostError::unauthorized)?;
        Ok(Box::pin(io::Cursor::new(bytes)))
    }

    async fn read(&self, resource: &str) -> Result<Option<Page>, HostError> {
        assert_eq!(resource, "items");
        let call = self.reads.fetch_add(1, Ordering::AcqRel);
        if call > 0 {
            return Ok(None);
        }
        Ok(Some(Page {
            items: vec![Item {
                seed: "seed-a".into(),
                value: r#"{"id":"a","value":1.0,"payload":"x","document":"input"}"#.into(),
            }],
            remaining: None,
        }))
    }

    async fn write(
        &self,
        resource: &str,
        _connection: Option<&str>,
        mut items: mpsc::Receiver<String>,
    ) -> Result<(), HostError> {
        assert_eq!(resource, "items");
        let mut chunk = Vec::new();
        while let Some(item) = items.recv().await {
            chunk.push(item);
        }
        self.writes.lock().expect("write mutex").push(chunk);
        Ok(())
    }

    async fn checkpoint(&self, resource: &str, _connection: Option<&str>, cursor: &str) -> Result<(), HostError> {
        assert_eq!(resource, "items");
        self.checkpoints.lock().expect("checkpoint mutex").push(cursor.into());
        Ok(())
    }

    async fn source(
        &self,
        resource: &str,
        property: &str,
        filename: &str,
        mut bytes: Pin<Box<dyn AsyncRead + Send>>,
    ) -> Result<String, HostError> {
        assert_eq!((resource, property), ("items", "document"));
        let mut contents = Vec::new();
        bytes
            .read_to_end(&mut contents)
            .await
            .map_err(|_| HostError::source_unreadable())?;
        let id = format!("produced-{}", self.produced.fetch_add(1, Ordering::AcqRel));
        self.sources.lock().expect("source mutex").insert(id.clone(), contents);
        self.source_filenames
            .lock()
            .expect("source filenames mutex")
            .push(filename.into());
        Ok(id)
    }

    async fn finish(&self) -> Result<(), HostError> {
        Ok(())
    }

    fn log(&self, _level: LogLevel, message: &str, _kind: LogKind) {
        self.logs.lock().expect("log mutex").push(message.into());
    }
}

fn fixture() -> Vec<u8> {
    fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sdk-guest.wasm"))
        .expect("regenerate SDK guest fixture")
}

fn request(action: &str) -> Request {
    Request {
        operation: "sdk-test".into(),
        action: action.replace('_', "-"),
        parameters: "{}".into(),
        configuration: "{}".into(),
        sources: Vec::new(),
        cursors: Vec::new(),
    }
}

async fn run(engine: &Engine, host: Arc<SdkHost>, action: &str) -> Result<Result<(), Failure>, Error> {
    run_request(engine, host, request(action)).await
}

async fn run_request(engine: &Engine, host: Arc<SdkHost>, request: Request) -> Result<Result<(), Failure>, Error> {
    let bytes = fixture();
    let manifest = validate_component(&bytes)?;
    let (_stop, stop_rx) = watch::channel(None);
    let (_deadline, deadline_rx) = watch::channel(None);
    engine.run(&bytes, &manifest, request, host, stop_rx, deadline_rx).await
}

#[test]
fn sdk_fixture_omits_machine_source_paths() {
    let component = fixture();
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
async fn sdk_unavailable_failure_preserves_message_and_retry_instant() {
    let engine = Engine::new().expect("engine config");
    let failure = run(&engine, Arc::new(SdkHost::default()), "retry_failure")
        .await
        .expect("SDK guest returns its declared failure")
        .expect_err("provider failure cannot become success");
    let Failure::Unavailable(retry) = failure else {
        panic!("expected unavailable failure, got {failure:?}");
    };
    assert_eq!(retry.message, "provider requested retry");
    // The fixture requests 2099-01-02T03:04:05.123456789Z.
    assert_eq!(
        retry.not_before.map(|when| (when.seconds, when.nanoseconds)),
        Some((4_071_006_245, 123_456_789))
    );
}

#[tokio::test]
async fn sdk_source_copy_reopens_lent_bytes_and_seals_the_copy() {
    let engine = Engine::new().expect("engine config");
    let host = Arc::new(SdkHost::default());
    host.sources
        .lock()
        .expect("source mutex")
        .insert("input".into(), vec![7; 8192]);
    let request = request("source_copy");
    run_request(&engine, Arc::clone(&host), request)
        .await
        .expect("host executes source copy")
        .expect("source copy succeeds");
    assert_eq!(
        host.sources
            .lock()
            .expect("source mutex")
            .get("produced-0")
            .map(Vec::as_slice),
        Some(vec![7; 8192].as_slice())
    );
    assert_eq!(
        *host.source_filenames.lock().expect("source filenames mutex"),
        ["copied.bin"]
    );
    let writes = host.writes.lock().expect("write mutex");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].len(), 1);
    let item: serde_json::Value = serde_json::from_str(&writes[0][0]).expect("copied item is JSON");
    assert_eq!(
        item,
        serde_json::json!({"id":"a", "value":1.0, "payload":"x", "document":"produced-0"})
    );
}

#[tokio::test]
async fn cancelled_sdk_source_uploads_cannot_seal_partial_bytes() {
    let engine = Engine::new().expect("engine config");
    for action in ["oversized", "abandoned", "oversized_after_prefix", "forgotten_upload"] {
        let host = Arc::new(SdkHost::default());
        let result = run(&engine, Arc::clone(&host), action).await;
        match (action, &result) {
            // No write is acknowledged, so these failures remain SDK outcomes.
            ("oversized", Ok(Err(Failure::Rejected(message)))) => {
                assert_eq!(message, "value exceeds its declared limit");
            },
            ("abandoned" | "forgotten_upload", Ok(Err(Failure::Internal(message)))) => {
                assert_eq!(message, "source upload was not sealed");
            },
            // The acknowledged prefix starts host work; cancellation must fail
            // that pending source even though the guest ignores its size error.
            ("oversized_after_prefix", Err(Error::SourceFailed)) => {},
            _ => panic!("{action} returned an unexpected upload outcome: {result:?}"),
        }
        assert!(
            host.sources.lock().expect("source mutex").is_empty(),
            "{action} sealed truncated source bytes"
        );
    }
}

#[tokio::test]
async fn sdk_source_seals_exact_bytes_and_writes_the_source_reference() {
    let engine = Engine::new().expect("engine config");
    let host = Arc::new(SdkHost::default());
    run(&engine, Arc::clone(&host), "produce")
        .await
        .expect("host executes source production")
        .expect("source production succeeds");
    let sources = host.sources.lock().expect("source mutex");
    assert_eq!(
        sources.get("produced-0").map(Vec::as_slice),
        Some(vec![7; 100_000].as_slice())
    );
    assert_eq!(
        *host.source_filenames.lock().expect("source filenames mutex"),
        ["document.bin"]
    );
    let writes = host.writes.lock().expect("write mutex");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].len(), 1);
    let item: serde_json::Value = serde_json::from_str(&writes[0][0]).expect("produced item is JSON");
    assert_eq!(
        item,
        serde_json::json!({"id":"produced", "value":1.0, "payload":"x", "document":"produced-0"})
    );
}

#[tokio::test]
async fn source_sealed_before_a_later_action_failure_remains_durable() {
    let engine = Engine::new().expect("engine config");
    let host = Arc::new(SdkHost::default());
    let result = run(&engine, Arc::clone(&host), "seal_then_failure").await;
    assert!(
        matches!(&result, Ok(Err(Failure::Rejected(message))) if message == "later action failure"),
        "result={result:?}"
    );
    assert_eq!(
        host.sources
            .lock()
            .expect("source mutex")
            .get("produced-0")
            .map(Vec::as_slice),
        Some(b"durable contents".as_slice())
    );
}
