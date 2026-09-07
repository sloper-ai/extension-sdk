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
    task::{
        Context,
        Poll,
    },
    time::{
        Duration,
        Instant,
    },
};

use async_trait::async_trait;
use sloper_extension_host::{
    AccessToken,
    Engine,
    Error,
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
        ReadBuf,
    },
    sync::{
        Notify,
        mpsc,
        watch,
    },
    time::sleep,
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SourceWriteFailure {
    #[default]
    None,
    BeforeRead,
    AfterRead,
}

#[derive(Default)]
pub(crate) struct TestHost {
    pub(crate) started: Notify,
    pub(crate) finish_started: Notify,
    pub(crate) token_calls: AtomicUsize,
    pub(crate) source_calls: AtomicUsize,
    pub(crate) finishes: AtomicUsize,
    pub(crate) token_delay: Duration,
    pub(crate) finish_delay: Duration,
    pub(crate) finish_expiry: Option<watch::Sender<Option<Instant>>>,
    pub(crate) source_failure: bool,
    pub(crate) source_write_failure: SourceWriteFailure,
    pub(crate) unreadable: bool,
    pub(crate) token: Option<String>,
    pub(crate) read_sizes: Vec<usize>,
    pub(crate) written_items: Mutex<Vec<String>>,
    pub(crate) logs: Mutex<Vec<String>>,
    pub(crate) log_kinds: Mutex<Vec<LogKind>>,
    pub(crate) sources: Mutex<BTreeMap<String, Vec<u8>>>,
}

#[async_trait]
impl Host for TestHost {
    async fn access_token(&self, connection: &str) -> Result<AccessToken, HostError> {
        if connection != "ledger" {
            return Err(HostError::unauthorized());
        }
        self.token_calls.fetch_add(1, Ordering::AcqRel);
        sleep(self.token_delay).await;
        Ok(AccessToken {
            value: self.token.clone().unwrap_or_else(|| "sensitive-test-token".into()),
            expires_at: "2099-01-01T00:00:00Z".into(),
            scopes: vec!["test.read".into()],
        })
    }

    async fn open_source(&self, id: &str) -> Result<Pin<Box<dyn AsyncRead + Send>>, HostError> {
        self.source_calls.fetch_add(1, Ordering::AcqRel);
        if self.unreadable {
            return Ok(Box::pin(FailingReader));
        }
        let bytes = self
            .sources
            .lock()
            .expect("source mutex is not poisoned")
            .get(id)
            .cloned()
            .ok_or_else(HostError::unauthorized)?;
        Ok(Box::pin(io::Cursor::new(bytes)))
    }

    async fn read(&self, _resource: &str) -> Result<Option<Page>, HostError> {
        if !self.read_sizes.is_empty() {
            return Ok(Some(Page {
                items: self
                    .read_sizes
                    .iter()
                    .enumerate()
                    .map(|(index, size)| {
                        Item {
                            seed: index.to_string(),
                            value: sized_item(*size),
                        }
                    })
                    .collect(),
                remaining: None,
            }));
        }
        Ok(self
            .sources
            .lock()
            .expect("source mutex is not poisoned")
            .contains_key("projected")
            .then(|| {
                Page {
                    items: vec![Item {
                        seed: "opaque-seed".into(),
                        value: "{\"id\":\"one\",\"document\":\"projected\"}".into(),
                    }],
                    remaining: None,
                }
            }))
    }

    async fn write(
        &self,
        _resource: &str,
        _connection: Option<&str>,
        mut items: mpsc::Receiver<String>,
    ) -> Result<(), HostError> {
        while let Some(item) = items.recv().await {
            self.written_items.lock().expect("items mutex").push(item);
        }
        Ok(())
    }

    async fn checkpoint(&self, _resource: &str, _connection: Option<&str>, _cursor: &str) -> Result<(), HostError> {
        Ok(())
    }

    async fn source(
        &self,
        resource: &str,
        property: &str,
        _filename: &str,
        mut bytes: Pin<Box<dyn AsyncRead + Send>>,
    ) -> Result<String, HostError> {
        assert_eq!((resource, property), ("items", "document"));
        if self.source_write_failure == SourceWriteFailure::BeforeRead {
            return Err(HostError::source_write(io::Error::other(
                "injected produced-source storage failure",
            )));
        }
        let mut contents = Vec::new();
        bytes
            .read_to_end(&mut contents)
            .await
            .map_err(|_| HostError::source_unreadable())?;
        if self.source_failure {
            return Err(HostError::unavailable());
        }
        if self.source_write_failure == SourceWriteFailure::AfterRead {
            return Err(HostError::source_write(io::Error::other(
                "injected produced-source storage failure",
            )));
        }
        self.sources
            .lock()
            .expect("source mutex is not poisoned")
            .insert("produced".into(), contents);
        Ok("produced".into())
    }

    fn log(&self, _level: LogLevel, message: &str, kind: LogKind) {
        self.log_kinds.lock().expect("log kind mutex").push(kind);
        if message == "spinning" {
            self.started.notify_one();
        }
        self.logs
            .lock()
            .expect("log mutex is not poisoned")
            .push(message.into());
    }

    async fn finish(&self) -> Result<(), HostError> {
        self.finish_started.notify_one();
        sleep(self.finish_delay).await;
        if let Some(deadline) = &self.finish_expiry {
            deadline
                .send(Some(Instant::now()))
                .expect("settlement retains the native deadline receiver");
        }
        self.finishes.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

struct FailingReader;
impl AsyncRead for FailingReader {
    fn poll_read(self: Pin<&mut Self>, _context: &mut Context<'_>, _buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::other("injected source transport failure")))
    }
}

pub(crate) fn fixture() -> Vec<u8> {
    fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host-guest.wasm"))
        .expect("regenerate the real guest with tests/guest's build-fixture binary")
}

pub(crate) fn request(action: &str) -> Request {
    Request {
        operation: "host-test".into(),
        action: action.into(),
        parameters: "{}".into(),
        configuration: "{}".into(),
        sources: Vec::new(),
        cursors: Vec::new(),
    }
}

fn sized_item(size: usize) -> String {
    let empty = r#"{"id":"one","value":""}"#;
    format!(r#"{{"id":"one","value":"{}"}}"#, "z".repeat(size - empty.len()))
}

pub(crate) async fn run(
    engine: &Engine,
    request: Request,
    host: Arc<TestHost>,
    deadline: Option<Instant>,
) -> Result<(), Error> {
    let bytes = fixture();
    let manifest = validate_component(&bytes)?;
    let (_stopping, stopping) = watch::channel(None);
    let (_native, native) = watch::channel(deadline);
    let result = engine.run(&bytes, &manifest, request, host, stopping, native).await?;
    assert!(result.is_ok(), "guest assertion failed: {result:?}");
    Ok(())
}
