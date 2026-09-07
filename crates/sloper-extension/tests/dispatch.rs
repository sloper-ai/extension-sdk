#![warn(rust_2018_idioms)]
#![cfg(not(target_arch = "wasm32"))] // A native host executes the actual Wasm guest.

use std::{
    error::Error as StdError,
    fs,
    io,
    path::Path,
    pin::Pin,
    process::Command,
    sync::{
        Arc,
        Mutex,
        OnceLock,
        atomic::{
            AtomicUsize,
            Ordering,
        },
    },
    time::{
        Duration,
        Instant,
    },
};

use async_trait::async_trait;
use serde_json::Value;
use sloper_extension_host::{
    AccessToken,
    Engine,
    Failure,
    Host,
    HostError,
    Item,
    LogKind,
    LogLevel,
    Page,
    Request,
    assemble_parts,
    check_component,
    extract_parts,
    stamp_manifest,
    validate_component,
};
use tokio::{
    io::{
        AsyncRead,
        AsyncReadExt,
        AsyncWriteExt,
    },
    net::TcpListener,
    sync::{
        Notify,
        mpsc,
        watch,
    },
};

type TestResult<T = ()> = Result<T, Box<dyn StdError>>;

/// Build once per test process from current sources.
fn fixture() -> TestResult<&'static [u8]> {
    static COMPONENT: OnceLock<Result<Vec<u8>, String>> = OnceLock::new();
    COMPONENT
        .get_or_init(|| build_fixture().map_err(|error| error.to_string()))
        .as_deref()
        .map_err(|message| io::Error::other(message.clone()).into())
}

fn build_fixture() -> TestResult<Vec<u8>> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = directory.join("../../target/component-fixture");
    let output = Command::new("cargo")
        .args([
            "build",
            "--locked",
            "--release",
            "--target",
            "wasm32-wasip2",
            "--package",
            "sloper-extension-runtime-component",
            "--lib",
            "--target-dir",
        ])
        .arg(&target)
        .current_dir(directory)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "SDK component build failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    let bytes = fs::read(target.join("wasm32-wasip2/release/sloper_extension_runtime_component.wasm"))?;
    let parts = extract_parts(&bytes)?;
    let manifest = assemble_parts(&parts)?;
    let component = stamp_manifest(&bytes, &manifest)?;
    check_component(&component)?;
    Ok(component)
}

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Write(Vec<String>),
    Checkpoint(String),
}

#[derive(Default)]
struct RecordingHost {
    reads: AtomicUsize,
    token_calls: AtomicUsize,
    source_calls: AtomicUsize,
    events: Mutex<Vec<Event>>,
    logs: Mutex<Vec<String>>,
    uploaded: Mutex<Vec<Vec<u8>>>,
    checkpointed: Notify,
}

#[async_trait]
impl Host for RecordingHost {
    async fn access_token(&self, connection: &str) -> Result<AccessToken, HostError> {
        assert_eq!(connection, "ledger");
        let call = self.token_calls.fetch_add(1, Ordering::AcqRel);
        Ok(AccessToken {
            value: format!("sdk-secret-token-{call}"),
            expires_at: "2099-01-01T00:00:00Z".into(),
            scopes: vec!["items.read".into()],
        })
    }

    async fn open_source(&self, _id: &str) -> Result<Pin<Box<dyn AsyncRead + Send>>, HostError> {
        Err(HostError::unauthorized())
    }

    async fn read(&self, resource: &str) -> Result<Option<Page>, HostError> {
        assert_eq!(resource, "items");
        if self.reads.fetch_add(1, Ordering::AcqRel) != 0 {
            return Ok(None);
        }
        Ok(Some(Page {
            items: vec![Item {
                seed: "seed-a".into(),
                value: r#"{"id":"a","value":1.0,"payload":"x","document":null}"#.into(),
            }],
            // This means a whole-model scan, not end of the reader.
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
        let mut batch = Vec::new();
        while let Some(item) = items.recv().await {
            batch.push(item);
        }
        self.events.lock().expect("event mutex").push(Event::Write(batch));
        Ok(())
    }

    async fn checkpoint(&self, resource: &str, _connection: Option<&str>, cursor: &str) -> Result<(), HostError> {
        assert_eq!(resource, "items");
        self.events
            .lock()
            .expect("event mutex")
            .push(Event::Checkpoint(cursor.into()));
        self.checkpointed.notify_one();
        Ok(())
    }

    async fn source(
        &self,
        resource: &str,
        property: &str,
        filename: &str,
        mut bytes: Pin<Box<dyn AsyncRead + Send>>,
    ) -> Result<String, HostError> {
        self.source_calls.fetch_add(1, Ordering::AcqRel);
        assert_eq!((resource, property, filename), ("items", "document", "entropy.bin"));
        let mut uploaded = Vec::new();
        bytes
            .read_to_end(&mut uploaded)
            .await
            .map_err(|_| HostError::unavailable())?;
        self.uploaded.lock().expect("upload mutex").push(uploaded);
        Ok("uploaded-entropy".into())
    }

    async fn finish(&self) -> Result<(), HostError> {
        Ok(())
    }

    fn log(&self, _level: LogLevel, message: &str, _kind: LogKind) {
        self.logs.lock().expect("log mutex").push(message.into());
    }
}

async fn dispatch(host: &Arc<RecordingHost>, action: &str) -> TestResult<Result<(), Failure>> {
    dispatch_parameters(host, action, "{}".into()).await
}

fn request(action: &str, parameters: String) -> Request {
    Request {
        operation: "sdk-conformance".into(),
        action: action.replace('_', "-"),
        parameters,
        configuration: "{}".into(),
        sources: Vec::new(),
        cursors: Vec::new(),
    }
}

async fn dispatch_parameters(
    host: &Arc<RecordingHost>,
    action: &str,
    parameters: String,
) -> TestResult<Result<(), Failure>> {
    let component = fixture()?;
    let manifest = validate_component(component)?;
    let engine = Engine::new()?;
    let (_stop, stop_rx) = watch::channel(None);
    let (_deadline, deadline_rx) = watch::channel(None);
    Ok(engine
        .run(
            component,
            &manifest,
            request(action, parameters),
            Arc::clone(host) as Arc<dyn Host>,
            stop_rx,
            deadline_rx,
        )
        .await?)
}

/// The HTTP response depends on a source upload and resource checkpoint from
/// the same action. Completion proves that Tokio and component callbacks both
/// progress while the other executor has pending work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_crates_progress_alongside_component_resource_streams() -> TestResult {
    fixture()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let host = Arc::new(RecordingHost::default());
    let response_host = Arc::clone(&host);
    let server = async move {
        for index in 0..2 {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0; 1024];
            let size = stream.read(&mut request).await?;
            assert!(size > 0, "reqwest sends an HTTP request before the resource checkpoint");
            if index == 0 {
                response_host.checkpointed.notified().await;
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\ncrate-compatible")
                .await?;
        }
        io::Result::Ok(())
    };
    let parameters = serde_json::json!({"address": address.to_string()}).to_string();
    let run = async {
        let result = dispatch_parameters(&host, "crate_compatibility", parameters).await?;
        assert!(result.is_ok(), "mixed crate dispatch failed: {result:?}");
        TestResult::Ok(())
    };
    tokio::time::timeout(Duration::from_secs(120), async {
        tokio::try_join!(run, async {
            server.await?;
            TestResult::Ok(())
        })
    })
    .await??;
    assert_eq!(host.source_calls.load(Ordering::Acquire), 1);
    assert_eq!(host.uploaded.lock().expect("upload mutex")[0].len(), 32);
    let events = host.events.lock().expect("event mutex");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1], Event::Checkpoint("mixed-complete".into()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wasi_http_client_runs_within_generated_dispatch() -> TestResult {
    fixture()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let host = Arc::new(RecordingHost::default());
    let server = async move {
        let (mut stream, _) = listener.accept().await?;
        let mut request = [0; 1024];
        assert!(stream.read(&mut request).await? > 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\ncrate-compatible")
            .await?;
        io::Result::Ok(())
    };
    let parameters = serde_json::json!({"address": address.to_string()}).to_string();
    let run = async {
        let result = dispatch_parameters(&host, "wasi_http", parameters).await?;
        assert!(
            result.is_ok(),
            "WASI HTTP dispatch failed: {result:?}; logs={:?}",
            host.logs.lock().expect("log mutex")
        );
        TestResult::Ok(())
    };
    tokio::time::timeout(Duration::from_secs(120), async {
        tokio::try_join!(run, async {
            server.await?;
            TestResult::Ok(())
        })
    })
    .await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_deadline_interrupts_a_pending_tokio_http_request() -> TestResult {
    let component = fixture()?;
    let manifest = validate_component(component)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let host = Arc::new(RecordingHost::default());
    let engine = Engine::new()?;
    let (_stop, stop_rx) = watch::channel(None);
    let (deadline, deadline_rx) = watch::channel(None);
    let parameters = serde_json::json!({"address": address.to_string()}).to_string();
    let run = engine.run(
        component,
        &manifest,
        request("pending_http", parameters),
        host,
        stop_rx,
        deadline_rx,
    );
    let interrupt = async move {
        let (mut stream, _) = listener.accept().await?;
        let mut request = [0; 1024];
        assert!(stream.read(&mut request).await? > 0);
        deadline.send(Some(Instant::now())).map_err(io::Error::other)?;
        // Keep the peer open until the host drops the cancelled guest socket.
        assert_eq!(stream.read(&mut request).await?, 0);
        io::Result::Ok(())
    };
    let (result, interrupted) =
        tokio::time::timeout(Duration::from_secs(120), async { tokio::join!(run, interrupt) }).await?;
    interrupted?;
    assert!(
        matches!(result, Err(sloper_extension_host::Error::Deadline)),
        "result={result:?}"
    );
    Ok(())
}

#[tokio::test]
async fn abandoned_source_is_cancelled_without_sealing_partial_bytes() -> TestResult {
    fixture()?;
    let host = Arc::new(RecordingHost::default());
    let result = tokio::time::timeout(Duration::from_secs(120), dispatch(&host, "abandoned_source")).await?;
    assert!(!matches!(result, Ok(Ok(()))), "abandoned upload must fail dispatch");
    assert_eq!(host.source_calls.load(Ordering::Acquire), 1, "result={result:?}");
    assert!(host.uploaded.lock().expect("upload mutex").is_empty());
    Ok(())
}

#[tokio::test]
async fn writer_chunks_items_and_advances_only_the_final_cursor() -> TestResult {
    let host = Arc::new(RecordingHost::default());
    assert!(dispatch(&host, "chunk_items").await?.is_ok());
    let events = host.events.lock().expect("event mutex");
    let mut item_index = 0;
    assert_eq!(events.len(), 6);
    for (index, expected_count) in [1000, 1000, 1].into_iter().enumerate() {
        let Event::Write(items) = &events[index * 2] else {
            panic!("write precedes its checkpoint")
        };
        assert_eq!(items.len(), expected_count);
        for item in items {
            let value: Value = serde_json::from_str(item)?;
            assert_eq!(value["id"], format!("i{item_index}"));
            item_index += 1;
        }
        assert_eq!(
            events[index * 2 + 1],
            Event::Checkpoint(
                if index == 2 {
                    "c2"
                } else {
                    ""
                }
                .into()
            )
        );
    }
    assert_eq!(item_index, 2001);
    assert_eq!(host.token_calls.load(Ordering::Acquire), 2);
    assert!(
        host.logs
            .lock()
            .expect("log mutex")
            .iter()
            .all(|message| !message.contains("sdk-secret-token"))
    );
    Ok(())
}

#[tokio::test]
async fn writer_splits_byte_batches_before_the_final_checkpoint() -> TestResult {
    let host = Arc::new(RecordingHost::default());
    assert!(dispatch(&host, "chunk_bytes").await?.is_ok());
    let events = host.events.lock().expect("event mutex");
    assert_eq!(events.len(), 4);
    for (index, expected_count) in [9, 1].into_iter().enumerate() {
        let Event::Write(items) = &events[index * 2] else {
            panic!("write precedes its checkpoint")
        };
        assert_eq!(items.len(), expected_count);
        assert!(items.iter().all(|item| item.len() <= 1024 * 1024));
        assert!(items.iter().map(String::len).sum::<usize>() <= 8 * 1024 * 1024);
        assert_eq!(
            events[index * 2 + 1],
            Event::Checkpoint(
                if index == 1 {
                    "c2"
                } else {
                    ""
                }
                .into()
            )
        );
    }
    Ok(())
}

#[tokio::test]
async fn empty_batch_advances_its_cursor_without_writing_items() -> TestResult {
    let host = Arc::new(RecordingHost::default());
    assert!(dispatch(&host, "empty_cursor").await?.is_ok());
    assert_eq!(
        *host.events.lock().expect("event mutex"),
        [Event::Checkpoint("c2".into())]
    );
    Ok(())
}

#[tokio::test]
async fn generated_dispatch_drains_start_send_after_the_writer_is_dropped() -> TestResult {
    let host = Arc::new(RecordingHost::default());
    assert!(dispatch(&host, "dropped_send").await?.is_ok());
    let events = host.events.lock().expect("event mutex");
    assert_eq!(events.len(), 2);
    let Event::Write(items) = &events[0] else {
        panic!("registered send writes its item")
    };
    assert_eq!(items.len(), 1);
    assert_eq!(serde_json::from_str::<Value>(&items[0])?["id"], "dropped");
    assert_eq!(events[1], Event::Checkpoint(String::new()));
    Ok(())
}

#[tokio::test]
async fn reader_fetches_only_the_pages_requested_by_the_action() -> TestResult {
    for (action, calls) in [("read_none", 0), ("read_one", 1), ("read_all", 2)] {
        let host = Arc::new(RecordingHost::default());
        assert!(dispatch(&host, action).await?.is_ok(), "{action}");
        assert_eq!(host.reads.load(Ordering::Acquire), calls, "{action}");
    }
    Ok(())
}

#[tokio::test]
async fn encoded_item_limit_includes_json_and_ignored_overflow_fails_dispatch() -> TestResult {
    let exact = Arc::new(RecordingHost::default());
    assert!(dispatch(&exact, "exact_item").await?.is_ok());
    {
        let events = exact.events.lock().expect("event mutex");
        assert_eq!(events.len(), 2);
        let Event::Write(items) = &events[0] else {
            panic!("exact item is written")
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].len(), 1024 * 1024);
    }
    let oversized = Arc::new(RecordingHost::default());
    assert!(matches!(
        dispatch(&oversized, "oversized_item").await?,
        Err(Failure::Rejected(_))
    ));
    assert!(oversized.events.lock().expect("event mutex").is_empty());
    assert!(
        oversized
            .logs
            .lock()
            .expect("log mutex")
            .iter()
            .any(|message| message == "ignored oversized item error; action returns success")
    );
    Ok(())
}

#[tokio::test]
async fn rejected_source_pointer_and_nonfinite_send_remain_dispatch_failures() -> TestResult {
    for (action, marker) in [
        (
            "pointer_source",
            "ignored source property error; action returns success",
        ),
        ("nonfinite", "ignored nonfinite item error; action returns success"),
    ] {
        let host = Arc::new(RecordingHost::default());
        assert!(
            matches!(dispatch(&host, action).await?, Err(Failure::Internal(_))),
            "{action}"
        );
        assert!(host.events.lock().expect("event mutex").is_empty(), "{action}");
        assert_eq!(host.source_calls.load(Ordering::Acquire), 0, "{action}");
        assert!(
            host.logs
                .lock()
                .expect("log mutex")
                .iter()
                .any(|message| message == marker),
            "the action reached its successful return after ignoring {action}"
        );
    }
    Ok(())
}
