//! One shared WASI context and fresh component instance for each attempt.

use std::{
    cmp::Reverse,
    collections::{
        BTreeMap,
        BTreeSet,
    },
    error::Error as StdError,
    fmt,
    future::{
        Future,
        ready,
    },
    io::{
        self,
        Error as IoError,
    },
    mem,
    pin::Pin,
    process,
    sync::{
        Arc,
        Mutex,
        PoisonError,
        atomic::{
            AtomicBool,
            AtomicUsize,
            Ordering,
        },
    },
    task::{
        Context,
        Poll,
    },
    thread::{
        self,
        Builder as ThreadBuilder,
        JoinHandle,
    },
    time::{
        Duration,
        Instant,
    },
};

use async_trait::async_trait;
use sloper_extension_spec::{
    Capability,
    Manifest,
    ManifestError,
    Mode,
    Schema,
    parse_object,
    validate_source_filename,
    validate_source_property,
};
use tokio::{
    io::{
        AsyncRead,
        AsyncWrite,
        DuplexStream,
        ReadBuf,
        duplex,
    },
    sync::{
        mpsc,
        watch,
    },
    time::sleep_until,
};
use tokio_util::sync::PollSender;
use wasmtime::{
    Config,
    ResourceLimiter,
    Store,
    StoreContextMut,
    UpdateDeadline,
    component::{
        Accessor,
        Component,
        Destination,
        HasSelf,
        Linker,
        Resource,
        ResourceTable,
        Source as StreamSource,
        StreamConsumer,
        StreamProducer,
        StreamReader,
        StreamResult,
        VecBuffer,
    },
};
use wasmtime_wasi::{
    FsPerms,
    WasiCtx,
    WasiCtxBuilder,
    WasiCtxView,
    WasiView,
    p2::add_to_linker_async as add_wasi_to_linker,
    sockets::SocketAddrUse,
};
use wasmtime_wasi_http::{
    WasiHttpCtx,
    WasiHttpCtxView,
    WasiHttpView,
    p2::add_only_http_to_linker_async as add_http_to_linker,
};

#[allow(
    missing_docs,
    unreachable_pub,
    clippy::all,
    clippy::module_name_repetitions,
    clippy::pedantic,
    reason = "Generated canonical ABI bindings are owned by wasmtime bindgen."
)]
mod bindings {
    pub use super::Writer;
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}
use bindings::Extension;
pub use log::Level as LogLevel;

/// Internal log admission control, separate from the guest's message bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogKind {
    /// An ordinary diagnostic emitted by the guest.
    Message,
    /// The host's single terminal notice after exhausting its log budget.
    DroppedNotice,
}
pub use resources::{
    Item,
    Page,
};
pub use sources::Source;
use zeroize::Zeroizing;

pub use self::bindings::exports::sloper::extension::action::{
    Cursor,
    Failure,
    Request,
};
use self::bindings::sloper::api::{
    credentials,
    errors,
    log,
    resources,
    sources,
};
use crate::{
    Error,
    HostError,
    extract_manifest,
    validate_component,
};

impl From<HostError> for errors::Error {
    fn from(value: HostError) -> Self {
        match value {
            HostError::Unauthorized => Self::Unauthorized,
            HostError::Invalid(message) => Self::Invalid(message.into()),
            HostError::TooLarge => Self::TooLarge,
            HostError::Stopped => Self::Cancelled,
            HostError::SourceUnreadable | HostError::SourceRead(_) => {
                Self::Unavailable("source could not be read".into())
            },
            HostError::SourceWrite(_) => Self::Unavailable("source could not be written".into()),
            HostError::Unavailable => Self::Unavailable("extension host is unavailable".into()),
        }
    }
}

/// A freshly issued provider token. Debug output excludes the secret value.
pub struct AccessToken {
    /// Short-lived token bytes, copied into the guest only for its declared
    /// connection.
    pub value: String,
    /// RFC 3339 expiration supplied by the token issuer.
    pub expires_at: String,
    /// Granted OAuth scopes.
    pub scopes: Vec<String>,
}
impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessToken").finish_non_exhaustive()
    }
}

/// Caller-supplied capabilities lent to exactly one attempt.
///
/// Every method must reject cancelled or superseded attempts. The host
/// validates incoming guest data; the adapter controls persistence, source
/// media validation, and provider authorization.
#[async_trait]
pub trait Host: Send + Sync + 'static {
    /// Obtain a new short-lived token for the pinned connection binding.
    async fn access_token(&self, connection: &str) -> Result<AccessToken, HostError>;
    /// Open already-authorized source bytes. I/O errors must not be converted
    /// to EOF.
    async fn open_source(&self, id: &str) -> Result<Pin<Box<dyn AsyncRead + Send>>, HostError>;
    /// Advance only this reader's committed forward scan.
    async fn read(&self, resource: &str) -> Result<Option<Page>, HostError>;
    /// Atomically stage a complete, schema-validated bounded call.
    async fn write(
        &self,
        resource: &str,
        connection: Option<&str>,
        items: mpsc::Receiver<String>,
    ) -> Result<(), HostError>;
    /// Persist staged items and the cursor together under the current fence.
    async fn checkpoint(&self, resource: &str, connection: Option<&str>, cursor: &str) -> Result<(), HostError>;
    /// Seal exact bytes only after successful EOF; retain sealed data on later
    /// action failure.
    async fn source(
        &self,
        resource: &str,
        property: &str,
        filename: &str,
        bytes: Pin<Box<dyn AsyncRead + Send>>,
    ) -> Result<String, HostError>;
    /// Persist a bounded line after exact issued-token redaction.
    fn log(&self, level: LogLevel, message: &str, kind: LogKind);
    /// Finish a successful guest, discarding uncheckpointed staging and
    /// completing required settlement. Failed attempts are swept by the
    /// caller during operation finalization.
    async fn finish(&self) -> Result<(), HostError>;
}

#[derive(Debug)]
#[allow(
    unreachable_pub,
    reason = "Wasmtime bindgen publicly aliases mapped resources inside its private module."
)]
pub struct Writer {
    name: String,
    connection: Option<String>,
}
struct State {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    limits: Limits,
    host: Arc<dyn Host>,
    connections: Vec<String>,
    resources: BTreeMap<String, Vec<Capability>>,
    schemas: BTreeMap<String, Schema>,
    opened_writers: BTreeSet<String>,
    lent_sources: BTreeSet<String>,
    has_reader: bool,
    source_failure: Arc<AtomicBool>,
    source_read_failure: Arc<AtomicBool>,
    source_read_cause: ReadCause,
    source_write_cause: ReadCause,
    pending_sources: Arc<AtomicUsize>,
    tokens: Vec<Zeroizing<String>>,
    log_bytes: usize,
    logs_dropped: bool,
    write_failures: WriteFailures,
    deadline: Deadline,
    // Drop after WASI descriptors so cleanup also works on Windows.
    _scratch: tempfile::TempDir,
}
impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}
impl WasiHttpView for State {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}
impl errors::Host for State {}
impl credentials::Host for State {}
impl sources::Host for State {}
impl resources::Host for State {}
impl log::Host for State {
    fn write(&mut self, level: log::Level, message: String) -> wasmtime::Result<()> {
        if self.logs_dropped {
            return Ok(());
        }
        let mut message = redact(&message, &self.tokens);
        truncate(&mut message, MAX_LOG_LINE_BYTES);
        let charge = message.len() + LOG_ENTRY_BYTES;
        // A token issued after earlier entries can expand the final notice to
        // the full line limit. Its control meaning never depends on its text.
        let reserve = MAX_LOG_LINE_BYTES + LOG_ENTRY_BYTES;
        if self.log_bytes + charge <= MAX_LOG_BYTES - reserve {
            self.log_bytes += charge;
            self.host.log(level, &message, LogKind::Message);
        } else {
            self.logs_dropped = true;
            let mut notice = redact(LOG_DROPPED_MESSAGE, &self.tokens);
            truncate(&mut notice, MAX_LOG_LINE_BYTES);
            self.log_bytes += notice.len() + LOG_ENTRY_BYTES;
            self.host.log(LogLevel::Warn, &notice, LogKind::DroppedNotice);
        }
        Ok(())
    }
}
impl credentials::HostWithStore<State> for HasSelf<State> {
    async fn access_token(
        store: &Accessor<State, Self>,
        connection: String,
    ) -> wasmtime::Result<Result<credentials::Token, errors::Error>> {
        let host = store.with(|mut access| {
            access
                .data_mut()
                .connections
                .contains(&connection)
                .then(|| Arc::clone(&access.data_mut().host))
        });
        let Some(host) = host else {
            return Ok(Err(errors::Error::Invalid(
                "connection is not declared by the action".into(),
            )));
        };
        let token = match host.access_token(&connection).await {
            Ok(token) => token,
            Err(error) => return Ok(Err(error.into())),
        };
        if token.value.is_empty() || token.value.len() > 16 * 1024 {
            return Ok(Err(errors::Error::Unavailable("provider token is invalid".into())));
        }
        store.with(|mut access| access.data_mut().tokens.push(Zeroizing::new(token.value.clone())));
        Ok(Ok(credentials::Token {
            value: token.value,
            expires_at: token.expires_at,
            scopes: token.scopes,
        }))
    }
}
impl sources::HostWithStore<State> for HasSelf<State> {
    async fn open(
        store: &Accessor<State, Self>,
        id: String,
    ) -> wasmtime::Result<Result<StreamReader<u8>, errors::Error>> {
        let granted = store.with(|mut access| {
            let state = access.data_mut();
            state.lent_sources.contains(&id).then(|| {
                (
                    Arc::clone(&state.host),
                    Arc::clone(&state.source_read_failure),
                    Arc::clone(&state.source_read_cause),
                )
            })
        });
        let Some((host, failed, cause)) = granted else {
            return Ok(Err(errors::Error::Invalid(
                "source was not lent to this attempt".into(),
            )));
        };
        let reader = match host.open_source(&id).await {
            Ok(reader) => reader,
            Err(error) => {
                failed.store(true, Ordering::Release);
                if let HostError::SourceRead(source) = &error {
                    remember_read_cause(&cause, Arc::clone(source));
                }
                return Ok(Err(error.into()));
            },
        };
        store
            .with(|mut access| {
                StreamReader::new(
                    &mut access,
                    BytesProducer {
                        reader,
                        failed,
                        cause,
                    },
                )
            })
            .map(Ok)
    }
}
impl resources::HostWithStore<State> for HasSelf<State> {
    fn open(
        store: &Accessor<State, Self>,
        name: String,
        connection: Option<String>,
    ) -> impl Future<Output = wasmtime::Result<Result<Resource<Writer>, errors::Error>>> + Send {
        ready(store.with(|mut access| {
            let state = access.data_mut();
            if !state
                .resources
                .get(&name)
                .is_some_and(|caps| caps.contains(&Capability::Write))
                || connection
                    .as_ref()
                    .is_some_and(|name| !state.connections.contains(name))
                || (state.connections.len() == 1 && connection.as_ref() != state.connections.first())
            {
                return Ok(Err(errors::Error::Invalid("writer capability is not declared".into())));
            }
            if state.opened_writers.contains(&name) {
                return Ok(Err(errors::Error::Invalid("resource writer is already open".into())));
            }
            state.opened_writers.insert(name.clone());
            state
                .table
                .push(Writer {
                    name,
                    connection,
                })
                .map(Ok)
                .map_err(Into::into)
        }))
    }

    async fn read(
        store: &Accessor<State, Self>,
        name: String,
    ) -> wasmtime::Result<Result<Option<resources::Page>, errors::Error>> {
        let host = store.with(|mut access| {
            access
                .data_mut()
                .resources
                .get(&name)
                .is_some_and(|caps| caps.contains(&Capability::Read))
                .then(|| Arc::clone(&access.data_mut().host))
        });
        let Some(host) = host else {
            return Ok(Err(errors::Error::Invalid("reader capability is not declared".into())));
        };
        let page = match host.read(&name).await {
            Ok(page) => page,
            Err(error) => {
                store.with(|mut access| record_failure::<()>(access.data_mut(), &Err(error.clone())));
                return Ok(Err(error.into()));
            },
        };
        if let Some(page) = &page {
            let validation = store.with(|mut access| {
                let state = access.data_mut();
                let Some(schema) = state.schemas.get(&name) else {
                    return Err(HostError::invalid("reader schema is missing"));
                };
                let mut bytes = 0_usize;
                let mut sources = BTreeSet::new();
                if page.items.len() > MAX_ITEMS {
                    return Err(HostError::too_large());
                }
                for item in &page.items {
                    bytes = bytes.saturating_add(item.value.len());
                    if item.value.len() > MAX_ITEM_BYTES || bytes > MAX_BATCH_BYTES {
                        return Err(HostError::too_large());
                    }
                    let value = parse_object(item.value.as_bytes())
                        .map_err(|_| HostError::invalid("read item is not a JSON object"))?;
                    schema
                        .validate_instance(&value)
                        .map_err(|_| HostError::invalid("read item does not satisfy its schema"))?;
                    collect_sources(schema.as_value(), &value, &mut sources);
                }
                state.lent_sources.extend(sources);
                Ok(())
            });
            if let Err(error) = validation {
                store.with(|mut access| record_failure::<()>(access.data_mut(), &Err(error.clone())));
                return Ok(Err(error.into()));
            }
        }
        Ok(Ok(page))
    }
}
impl resources::HostWriter for State {
    fn drop(&mut self, writer: Resource<Writer>) -> wasmtime::Result<()> {
        self.table.delete(writer)?;
        Ok(())
    }
}
impl resources::HostWriterWithStore<State> for HasSelf<State> {
    async fn write(
        store: &Accessor<State, Self>,
        writer: Resource<Writer>,
        items: StreamReader<String>,
    ) -> wasmtime::Result<Result<(), errors::Error>> {
        let (host, name, connection) = store.with(|mut access| -> wasmtime::Result<_> {
            let state = access.data_mut();
            let writer = state.table.get(&writer)?;
            Ok((Arc::clone(&state.host), writer.name.clone(), writer.connection.clone()))
        })?;
        let (sender, mut receiver) = mpsc::channel(16);
        store.with(|mut access| {
            items.pipe(
                &mut access,
                ItemsConsumer {
                    sender: PollSender::new(sender),
                },
            )
        })?;
        let mut staged = Vec::new();
        let mut bytes = 0_usize;
        let mut redacted_bytes = 0_usize;
        while let Some(item) = receiver.recv().await {
            bytes = bytes.saturating_add(item.len());
            if staged.len() >= MAX_ITEMS || item.len() > MAX_ITEM_BYTES || bytes > MAX_BATCH_BYTES {
                store.with(|mut access| access.data_mut().write_failures.ceiling = true);
                return Ok(Err(errors::Error::TooLarge));
            }
            let valid = parse_object(item.as_bytes()).and_then(|mut value| {
                store.with(|mut access| {
                    let state = access.data_mut();
                    if !redact_value(&mut value, &state.tokens) {
                        return Err(ManifestError::invalid(
                            "MANIFEST_INVALID_VALUE",
                            "",
                            "Redaction would duplicate an item property.",
                        ));
                    }
                    let schema = &state.schemas[&name];
                    schema.validate_instance(&value)?;
                    let mut sources = BTreeSet::new();
                    collect_sources(schema.as_value(), &value, &mut sources);
                    Ok((value, sources))
                })
            });
            let Ok((value, sources)) = valid else {
                store.with(|mut access| access.data_mut().write_failures.invalid = true);
                return Ok(Err(errors::Error::Invalid("item does not satisfy its schema".into())));
            };
            let authorized = store.with(|mut access| sources.is_subset(&access.data_mut().lent_sources));
            if !authorized {
                store.with(|mut access| access.data_mut().write_failures.invalid = true);
                return Ok(Err(errors::Error::Invalid(
                    "item source was not lent to this attempt".into(),
                )));
            }
            let encoded = serde_json::to_string(&value).expect("validated JSON value is serializable");
            redacted_bytes = redacted_bytes.saturating_add(encoded.len());
            if encoded.len() > MAX_ITEM_BYTES || redacted_bytes > MAX_BATCH_BYTES {
                store.with(|mut access| access.data_mut().write_failures.ceiling = true);
                return Ok(Err(errors::Error::TooLarge));
            }
            staged.push(encoded);
        }
        let (sender, receiver) = mpsc::channel(staged.len().max(1));
        for item in staged {
            sender
                .try_send(item)
                .expect("channel capacity equals the complete validated call");
        }
        drop(sender);
        let result = host.write(&name, connection.as_deref(), receiver).await;
        store.with(|mut access| record_failure(access.data_mut(), &result));
        Ok(result.map_err(Into::into))
    }

    async fn checkpoint(
        store: &Accessor<State, Self>,
        writer: Resource<Writer>,
        cursor: String,
    ) -> wasmtime::Result<Result<(), errors::Error>> {
        let (host, name, connection, has_reader) = store.with(|mut access| -> wasmtime::Result<_> {
            let state = access.data_mut();
            wasmtime::Result::Ok((
                Arc::clone(&state.host),
                state.table.get(&writer)?.name.clone(),
                state.table.get(&writer)?.connection.clone(),
                state.has_reader,
            ))
        })?;
        if cursor.len() > MAX_CURSOR_BYTES {
            store.with(|mut access| access.data_mut().write_failures.ceiling = true);
            return Ok(Err(errors::Error::TooLarge));
        }
        Ok(host
            .checkpoint(
                &name,
                connection.as_deref(),
                if has_reader {
                    ""
                } else {
                    &cursor
                },
            )
            .await
            .map_err(Into::into))
    }

    async fn source(
        store: &Accessor<State, Self>,
        writer: Resource<Writer>,
        property: String,
        filename: String,
        contents: StreamReader<u8>,
    ) -> wasmtime::Result<Result<String, errors::Error>> {
        let (host, name, failure, pending, bound) = store.with(|mut access| -> wasmtime::Result<_> {
            let state = access.data_mut();
            let name = state.table.get(&writer)?.name.clone();
            let bound = state
                .schemas
                .get(&name)
                .filter(|_| validate_source_property(&property).is_ok())
                .and_then(|schema| schema.as_value().get("properties"))
                .and_then(|properties| properties.get(&property))
                .filter(|schema| schema.get("format").and_then(serde_json::Value::as_str) == Some("source"))
                .map(|schema| {
                    schema
                        .get("maxBytes")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(MAX_SOURCE_BYTES)
                });
            Ok((
                Arc::clone(&state.host),
                name,
                Arc::clone(&state.source_failure),
                Arc::clone(&state.pending_sources),
                bound,
            ))
        })?;
        let Some(bound) = bound else {
            failure.store(true, Ordering::Release);
            return Ok(Err(errors::Error::Invalid("source property is not declared".into())));
        };
        if !valid_filename(&filename) {
            failure.store(true, Ordering::Release);
            return Ok(Err(errors::Error::Invalid("source filename is invalid".into())));
        }
        pending.fetch_add(1, Ordering::AcqRel);
        let mut pending = PendingSource {
            pending,
            failure: Arc::clone(&failure),
            complete: false,
        };
        let status = Arc::new(AtomicBool::new(false));
        let exceeded = Arc::new(AtomicBool::new(false));
        let (reader, writer) = duplex(STREAM_CHUNK_BYTES);
        store.with(|mut access| {
            contents.pipe(
                &mut access,
                BytesConsumer {
                    writer,
                    failure: Arc::clone(&status),
                    exceeded: Arc::clone(&exceeded),
                    written: 0,
                    bound,
                },
            )
        })?;
        let bytes = Box::pin(CheckedSourceReader {
            reader,
            failed: Arc::clone(&status),
        });
        let result = host.source(&name, &property, &filename, bytes).await;
        let result = if exceeded.load(Ordering::Acquire) {
            Err(HostError::too_large())
        } else if status.load(Ordering::Acquire) && !matches!(result, Err(HostError::SourceWrite(_))) {
            Err(HostError::unavailable())
        } else {
            result
        };
        store.with(|mut access| record_failure(access.data_mut(), &result));
        if result.is_err() {
            failure.store(true, Ordering::Release);
        }
        if let Ok(id) = &result {
            store.with(|mut access| access.data_mut().lent_sources.insert(id.clone()));
        }
        pending.complete = true;
        Ok(result.map_err(Into::into))
    }
}

struct BytesProducer {
    reader: Pin<Box<dyn AsyncRead + Send>>,
    failed: Arc<AtomicBool>,
    cause: ReadCause,
}
impl StreamProducer<State> for BytesProducer {
    type Buffer = VecBuffer<u8>;
    type Item = u8;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'a, State>,
        destination: Destination<'a, u8, VecBuffer<u8>>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut destination = destination.as_direct(store, STREAM_CHUNK_BYTES);
        let remaining = destination.remaining();
        let capacity = remaining.len().min(STREAM_CHUNK_BYTES);
        if capacity == 0 {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let mut buffer = ReadBuf::new(&mut remaining[..capacity]);
        match self.reader.as_mut().poll_read(cx, &mut buffer) {
            Poll::Ready(Ok(())) => {
                let n = buffer.filled().len();
                destination.mark_written(n);
                Poll::Ready(Ok(if n == 0 {
                    StreamResult::Dropped
                } else {
                    StreamResult::Completed
                }))
            },
            Poll::Ready(Err(error)) => {
                self.failed.store(true, Ordering::Release);
                remember_read_cause(&self.cause, Arc::new(error));
                Poll::Ready(Err(wasmtime::Error::msg("source could not be read")))
            },
            Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => Poll::Pending,
        }
    }
}
struct BytesConsumer {
    writer: DuplexStream,
    failure: Arc<AtomicBool>,
    exceeded: Arc<AtomicBool>,
    written: u64,
    bound: u64,
}
impl StreamConsumer<State> for BytesConsumer {
    type Item = u8;

    fn poll_consume<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'a, State>,
        source: StreamSource<'a, u8>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut source = source.as_direct(store);
        if source.remaining().len() as u64 > self.bound.saturating_sub(self.written) {
            self.exceeded.store(true, Ordering::Release);
            self.failure.store(true, Ordering::Release);
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        match Pin::new(&mut self.writer).poll_write(cx, source.remaining()) {
            Poll::Ready(Ok(n)) => {
                source.mark_read(n);
                self.written += n as u64;
                Poll::Ready(Ok(StreamResult::Completed))
            },
            Poll::Ready(Err(error)) => {
                self.failure.store(true, Ordering::Release);
                Poll::Ready(Err(error.into()))
            },
            Poll::Pending if finish => {
                self.failure.store(true, Ordering::Release);
                Poll::Ready(Ok(StreamResult::Cancelled))
            },
            Poll::Pending => Poll::Pending,
        }
    }
}
struct ItemsConsumer {
    sender: PollSender<String>,
}
impl StreamConsumer<State> for ItemsConsumer {
    type Item = String;

    fn poll_consume<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, State>,
        mut source: StreamSource<'a, String>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        match self.sender.poll_reserve(cx) {
            Poll::Ready(Ok(())) => {},
            Poll::Ready(Err(_)) => return Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending if finish => {
                self.sender.abort_send();
                return Poll::Ready(Ok(StreamResult::Cancelled));
            },
            Poll::Pending => return Poll::Pending,
        }
        let mut item = None;
        if let Err(error) = source.read(&mut store, &mut item) {
            self.sender.abort_send();
            return Poll::Ready(Err(error));
        }
        if let Some(item) = item {
            self.sender
                .send_item(item)
                .expect("a successful reservation retains its channel slot");
        } else {
            self.sender.abort_send();
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

/// Exact bytes and manifest admitted by the restricted host linker.
#[derive(Debug)]
pub struct AdmittedComponent {
    component: Vec<u8>,
    manifest: Manifest,
    manifest_bytes: Vec<u8>,
}
impl AdmittedComponent {
    /// Borrows the exact admitted component bytes.
    #[must_use]
    pub fn component(&self) -> &[u8] {
        &self.component
    }

    /// Borrows the validated embedded manifest.
    #[must_use]
    pub const fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Borrows the original manifest bytes, without canonicalization.
    #[must_use]
    pub fn manifest_bytes(&self) -> &[u8] {
        &self.manifest_bytes
    }
}

/// One engine and restricted linker policy for component execution.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}
struct EngineInner {
    engine: wasmtime::Engine,
    complete: Arc<AtomicBool>,
    ticker: Option<JoinHandle<()>>,
}
impl Drop for EngineInner {
    fn drop(&mut self) {
        self.complete.store(true, Ordering::Release);
        if let Some(ticker) = self.ticker.take() {
            // The ticker has no fallible work. A panic is not a guest diagnostic.
            if ticker.join().is_err() {
                process::abort();
            }
        }
    }
}
impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}
impl Engine {
    /// Creates the shared async component engine, without a compiled disk
    /// cache.
    ///
    /// # Errors
    /// Returns an engine configuration or independent ticker startup failure.
    pub fn new() -> Result<Self, Error> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .wasm_component_model_async(true)
            .epoch_interruption(true);
        let engine = wasmtime::Engine::new(&config)?;
        let complete = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&complete);
        let clock = engine.clone();
        let ticker = ThreadBuilder::new()
            .name("sloper-extension-epochs".into())
            .spawn(move || {
                while !done.load(Ordering::Acquire) {
                    thread::sleep(EPOCH_INTERVAL);
                    clock.increment_epoch();
                }
            })
            .map_err(Error::watchdog)?;
        Ok(Self {
            inner: Arc::new(EngineInner {
                engine,
                complete,
                ticker: Some(ticker),
            }),
        })
    }

    fn linker(&self) -> wasmtime::Result<Linker<State>> {
        let mut linker = Linker::new(&self.inner.engine);
        add_wasi_to_linker(&mut linker)?;
        add_http_to_linker(&mut linker)?;
        errors::add_to_linker::<State, HasSelf<State>>(&mut linker, |state| state)?;
        credentials::add_to_linker::<State, HasSelf<State>>(&mut linker, |state| state)?;
        sources::add_to_linker::<State, HasSelf<State>>(&mut linker, |state| state)?;
        resources::add_to_linker::<State, HasSelf<State>>(&mut linker, |state| state)?;
        log::add_to_linker::<State, HasSelf<State>>(&mut linker, |state| state)?;
        Ok(linker)
    }

    fn store(
        &self,
        host: Arc<dyn Host>,
        manifest: &Manifest,
        request: Option<&Request>,
        deadline: Deadline,
    ) -> Result<Store<State>, Error> {
        let scratch = tempfile::Builder::new()
            .prefix("sloper-extension-")
            .tempdir()
            .map_err(Error::scratch)?;
        let mut wasi = WasiCtxBuilder::new();
        wasi.preopened_dir(scratch.path(), "/tmp", FsPerms::ReadWrite)?;
        wasi.initial_cwd("/tmp");
        wasi.allow_tcp(true).allow_udp(false).allow_ip_name_lookup(true);
        // TCP clients need an implicit local bind. Listening and accepting
        // remain denied, including on sockets that were explicitly bound.
        wasi.socket_addr_check(|_, usage| {
            Box::pin(async move { matches!(usage, SocketAddrUse::TcpBind | SocketAddrUse::TcpConnect) })
        });
        let mut table = ResourceTable::new();
        table.set_max_capacity(MAX_HANDLES);
        let action = request.and_then(|request| manifest.actions.get(&request.action));
        let state = State {
            wasi: wasi.build(),
            http: WasiHttpCtx::new(),
            table,
            limits: Limits::default(),
            host,
            connections: action.map_or_else(Vec::new, |action| action.connections.clone()),
            resources: action.map_or_else(BTreeMap::new, |action| action.resources.clone()),
            schemas: manifest
                .resources
                .iter()
                .map(|(name, resource)| (name.clone(), resource.schema.clone()))
                .collect(),
            opened_writers: BTreeSet::new(),
            lent_sources: request.map_or_else(BTreeSet::new, |request| {
                request.sources.iter().map(|source| source.id.clone()).collect()
            }),
            has_reader: action
                .is_some_and(|action| action.resources.values().any(|caps| caps.contains(&Capability::Read))),
            source_failure: Arc::new(AtomicBool::new(false)),
            source_read_failure: Arc::new(AtomicBool::new(false)),
            source_read_cause: Arc::new(Mutex::new(None)),
            source_write_cause: Arc::new(Mutex::new(None)),
            pending_sources: Arc::new(AtomicUsize::new(0)),
            tokens: Vec::new(),
            log_bytes: 0,
            logs_dropped: false,
            write_failures: WriteFailures::default(),
            deadline,
            _scratch: scratch,
        };
        let mut store = Store::new(&self.inner.engine, state);
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(1);
        // Epochs are shared. Each store examines its own absolute deadline rather
        // than treating another store's tick as permission to interrupt it.
        store.epoch_deadline_callback(|store| {
            Ok(if store.data().deadline.expired().is_some() {
                UpdateDeadline::Interrupt
            } else {
                UpdateDeadline::Continue(1)
            })
        });
        Ok(store)
    }

    /// Validates bytes and instantiates the actual host world without any
    /// action grants.
    ///
    /// # Errors
    /// Returns malformed manifest, incompatible world, instantiation, or
    /// deadline errors.
    ///
    /// # Cancel safety
    /// Dropping the future drops the admission store and its guest instance.
    pub async fn admit_component(&self, bytes: &[u8]) -> Result<AdmittedComponent, Error> {
        let manifest = validate_component(bytes)?;
        let manifest_bytes = extract_manifest(bytes)?;
        let component = Component::new(&self.inner.engine, bytes)?;
        let deadline = Deadline::admission();
        let mut store = self.store(Arc::new(AdmissionHost), &manifest, None, deadline.clone())?;
        let linker = self.linker()?;
        tokio::select! {
            result = Extension::instantiate_async(&mut store, &component, &linker) => { result?; }
            error = deadline.clone().wait() => return Err(error),
        }
        Ok(AdmittedComponent {
            component: bytes.to_vec(),
            manifest,
            manifest_bytes: manifest_bytes.to_vec(),
        })
    }

    /// Executes one fresh attempt with pinned inputs and absolute interruption
    /// times.
    ///
    /// Cancellation carries the persisted stopping time plus the 60-second
    /// grace; native deadlines additionally bound the operation window and
    /// host settlement.
    ///
    /// # Errors
    /// Returns spec violations, traps, I/O failures, limits, or elapsed
    /// deadlines.
    ///
    /// # Cancel safety
    /// Dropping this future abandons the guest; the caller must fence its
    /// attempt. Only callbacks that have completed may have durable side
    /// effects.
    pub async fn run(
        &self,
        bytes: &[u8],
        manifest: &Manifest,
        request: Request,
        host: Arc<dyn Host>,
        stopping: watch::Receiver<Option<Instant>>,
        native_deadline: watch::Receiver<Option<Instant>>,
    ) -> Result<Result<(), Failure>, Error> {
        let embedded = validate_component(bytes)?;
        if serde_json::to_value(&embedded).map_err(|_| Error::invalid("manifest cannot serialize"))?
            != serde_json::to_value(manifest).map_err(|_| Error::invalid("manifest cannot serialize"))?
        {
            return Err(Error::invalid("pinned manifest does not match component"));
        }
        validate_request(manifest, &request)?;
        let action = &manifest.actions[&request.action];
        let duration = match action.mode {
            Mode::Foreground => FOREGROUND_DEADLINE,
            Mode::Background => BACKGROUND_DEADLINE,
        };
        let deadline = Deadline {
            attempt: Instant::now() + duration,
            stopping,
            native: native_deadline,
        };
        if let Some(error) = deadline.expired() {
            return Err(error);
        }
        let component = Component::new(&self.inner.engine, bytes)?;
        let mut store = self.store(Arc::clone(&host), manifest, Some(&request), deadline.clone())?;
        let linker = self.linker()?;
        let result = tokio::select! {
            result = async {
                let instance = Extension::instantiate_async(&mut store, &component, &linker).await?;
                store.run_concurrent(async |access| instance.sloper_extension_action().call_run(access, request).await).await?
            } => result,
            error = deadline.clone().wait() => return Err(error),
        };
        if let Some(error) = deadline.expired() {
            return Err(error);
        }
        let state = store.data_mut();
        if state.source_read_failure.load(Ordering::Acquire) {
            if let Some(source) = state
                .source_read_cause
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                return Err(Error::SourceRead(source));
            }
            return Err(Error::source_unreadable());
        }
        if state.source_failure.load(Ordering::Acquire) || state.pending_sources.load(Ordering::Acquire) != 0 {
            if state.write_failures.ceiling {
                return Err(Error::limit_exceeded());
            }
            if let Some(source) = state
                .source_write_cause
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                return Err(Error::Host(HostError::SourceWrite(source)));
            }
            return Err(Error::source_failed());
        }
        let mut result = result.map_err(|error| {
            if state.limits.exceeded {
                Error::limit_exceeded()
            } else if error.downcast_ref::<wasmtime::Trap>().is_some() {
                Error::Trapped(error)
            } else {
                Error::Runtime(error)
            }
        })?;
        if matches!(result, Err(Failure::Internal(_))) {
            if state.write_failures.invalid {
                return Err(Error::item_invalid());
            }
            if state.write_failures.ceiling {
                return Err(Error::limit_exceeded());
            }
        }
        if let Err(failure) = &mut result {
            redact_failure(failure, &state.tokens);
        }
        // Settlement and staging cleanup use the same deadline, not a new allowance.
        if result.is_ok() {
            tokio::select! {
                result = host.finish() => result?,
                error = deadline.clone().wait() => return Err(error),
            }
            if let Some(error) = deadline.expired() {
                return Err(error);
            }
        }
        Ok(result)
    }
}

#[derive(Clone)]
struct Deadline {
    attempt: Instant,
    stopping: watch::Receiver<Option<Instant>>,
    native: watch::Receiver<Option<Instant>>,
}
impl Deadline {
    fn admission() -> Self {
        let (_, stopping) = watch::channel(None);
        let (_, native) = watch::channel(None);
        Self {
            attempt: Instant::now() + FOREGROUND_DEADLINE,
            stopping,
            native,
        }
    }

    fn expired(&self) -> Option<Error> {
        let now = Instant::now();
        if self.stopping.borrow().is_some_and(|deadline| now >= deadline) {
            Some(Error::stopped())
        } else if now >= self.attempt || self.native.borrow().is_some_and(|deadline| now >= deadline) {
            Some(Error::deadline())
        } else {
            None
        }
    }

    async fn wait(mut self) -> Error {
        let mut stopping_open = true;
        let mut native_open = true;
        loop {
            if let Some(error) = self.expired() {
                return error;
            }
            let mut next = self.attempt;
            if let Some(deadline) = *self.stopping.borrow() {
                next = next.min(deadline);
            }
            if let Some(deadline) = *self.native.borrow() {
                next = next.min(deadline);
            }
            tokio::select! {
                () = sleep_until(next.into()) => {},
                changed = self.stopping.changed(), if stopping_open => stopping_open = changed.is_ok(),
                changed = self.native.changed(), if native_open => native_open = changed.is_ok(),
            }
        }
    }
}

struct AdmissionHost;
#[async_trait]
impl Host for AdmissionHost {
    async fn access_token(&self, _: &str) -> Result<AccessToken, HostError> {
        Err(HostError::invalid("admission has no connections"))
    }

    async fn open_source(&self, _: &str) -> Result<Pin<Box<dyn AsyncRead + Send>>, HostError> {
        Err(HostError::invalid("admission has no sources"))
    }

    async fn read(&self, _: &str) -> Result<Option<Page>, HostError> {
        Err(HostError::invalid("admission has no readers"))
    }

    async fn write(&self, _: &str, _: Option<&str>, _: mpsc::Receiver<String>) -> Result<(), HostError> {
        Err(HostError::invalid("admission has no writers"))
    }

    async fn checkpoint(&self, _: &str, _: Option<&str>, _: &str) -> Result<(), HostError> {
        Err(HostError::invalid("admission has no writers"))
    }

    async fn source(&self, _: &str, _: &str, _: &str, _: Pin<Box<dyn AsyncRead + Send>>) -> Result<String, HostError> {
        Err(HostError::invalid("admission has no writers"))
    }

    fn log(&self, _: LogLevel, _: &str, _: LogKind) { /* Admission has no durable operation log. */
    }

    async fn finish(&self) -> Result<(), HostError> {
        Err(HostError::invalid("admission executes no action"))
    }
}

struct PendingSource {
    pending: Arc<AtomicUsize>,
    failure: Arc<AtomicBool>,
    complete: bool,
}
impl Drop for PendingSource {
    fn drop(&mut self) {
        if !self.complete {
            self.failure.store(true, Ordering::Release);
        }
        self.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

// The memory ceiling applies to the sum of all guest linear memories.
#[derive(Default)]
struct Limits {
    memory: usize,
    tables: usize,
    exceeded: bool,
}
impl ResourceLimiter for Limits {
    fn memory_growing(&mut self, current: usize, desired: usize, maximum: Option<usize>) -> wasmtime::Result<bool> {
        let Some(growth) = desired.checked_sub(current) else {
            return Ok(false);
        };
        let Some(total) = self.memory.checked_add(growth) else {
            return Ok(false);
        };
        if total > MAX_MEMORY_BYTES || maximum.is_some_and(|max| desired > max) {
            self.exceeded = true;
            return Ok(false);
        }
        self.memory = total;
        Ok(true)
    }

    fn table_growing(&mut self, current: usize, desired: usize, maximum: Option<usize>) -> wasmtime::Result<bool> {
        let Some(growth) = desired.checked_sub(current) else {
            return Ok(false);
        };
        let Some(total) = self.tables.checked_add(growth) else {
            return Ok(false);
        };
        if total > MAX_TABLE_ELEMENTS || maximum.is_some_and(|max| desired > max) {
            self.exceeded = true;
            return Ok(false);
        }
        self.tables = total;
        Ok(true)
    }
}

// Bounds applied before lending work to host adapters.
const MAX_ITEMS: usize = 1_000;
const MAX_ITEM_BYTES: usize = 1024 * 1024;
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
const MAX_CURSOR_BYTES: usize = 1024;
const MAX_SOURCE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_MEMORY_BYTES: usize = 512 * 1024 * 1024;
const MAX_LOG_LINE_BYTES: usize = 4 * 1024;
const MAX_LOG_BYTES: usize = 1024 * 1024;
// Reserve metadata overhead per log entry so empty messages still consume
// the bounded attempt log budget.
const LOG_ENTRY_BYTES: usize = 192;
const LOG_DROPPED_MESSAGE: &str = "Further attempt logs were dropped after the 1 MiB limit.";
const FOREGROUND_DEADLINE: Duration = Duration::from_mins(5);
const BACKGROUND_DEADLINE: Duration = Duration::from_hours(6);
// Bounded host buffering and descriptor allocations independent of guest
// memory.
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const MAX_TABLE_ELEMENTS: usize = 100_000;
const MAX_HANDLES: usize = 4_096;
const EPOCH_INTERVAL: Duration = Duration::from_millis(10);

fn validate_request(manifest: &Manifest, request: &Request) -> Result<(), Error> {
    if request.parameters.len() > MAX_BATCH_BYTES || request.configuration.len() > 256 * 1024 {
        return Err(Error::invalid("parameters or configuration exceed their byte ceiling"));
    }
    if request.operation.is_empty()
        || request.operation.len() > 64
        || !request
            .operation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(Error::invalid("operation identity is invalid"));
    }
    let parameters = parse_object(request.parameters.as_bytes())
        .map_err(|_| Error::invalid("parameters must be a strict JSON object"))?;
    let configuration = parse_object(request.configuration.as_bytes())
        .map_err(|_| Error::invalid("configuration must be a strict JSON object"))?;
    let action = manifest
        .actions
        .get(&request.action)
        .ok_or_else(|| Error::invalid("action is not declared"))?;
    if let Some(schema) = &action.parameters {
        schema
            .validate_instance(&parameters)
            .map_err(|_| Error::invalid("parameters do not satisfy their schema"))?;
    } else if parameters != serde_json::json!({}) {
        return Err(Error::invalid("action does not accept parameters"));
    }
    if let Some(schema) = &manifest.configuration {
        schema
            .validate_instance(&configuration)
            .map_err(|_| Error::invalid("configuration does not satisfy its schema"))?;
    } else if configuration != serde_json::json!({}) {
        return Err(Error::invalid("extension does not accept configuration"));
    }
    let has_reader = action.resources.values().any(|caps| caps.contains(&Capability::Read));
    if has_reader && (!request.cursors.is_empty() || parameters != serde_json::json!({})) {
        return Err(Error::invalid("reader action carries provider state"));
    }
    let mut cursors = BTreeSet::new();
    for cursor in &request.cursors {
        if cursor.value.len() > MAX_CURSOR_BYTES
            || !cursors.insert(&cursor.name)
            || !action
                .resources
                .get(&cursor.name)
                .is_some_and(|caps| caps.contains(&Capability::Write))
        {
            return Err(Error::invalid("writer cursor is invalid or duplicated"));
        }
    }
    let mut expected = BTreeSet::new();
    if let Some(schema) = &action.parameters {
        collect_sources(schema.as_value(), &parameters, &mut expected);
    }
    let mut actual = BTreeSet::new();
    for source in &request.sources {
        if !valid_filename(&source.filename)
            || source.size > MAX_SOURCE_BYTES
            || source.media_type.is_empty()
            || !actual.insert(source.id.clone())
        {
            return Err(Error::invalid("parameter source metadata is invalid or duplicated"));
        }
    }
    if expected != actual {
        return Err(Error::invalid(
            "parameter source metadata differs from the validated parameters",
        ));
    }
    if let Some(schema) = &action.parameters {
        let metadata = request
            .sources
            .iter()
            .map(|source| (source.id.as_str(), source))
            .collect();
        validate_source_metadata(schema.as_value(), &parameters, &metadata)?;
    }
    Ok(())
}

fn collect_sources(schema: &serde_json::Value, value: &serde_json::Value, ids: &mut BTreeSet<String>) {
    if schema.get("format").and_then(serde_json::Value::as_str) == Some("source") {
        if let Some(id) = value.as_str() {
            ids.insert(id.to_owned());
        }
        return;
    }
    if let Some(object) = value.as_object() {
        for (name, value) in object {
            let child = schema
                .get("properties")
                .and_then(|properties| properties.get(name))
                .or_else(|| schema.get("additionalProperties").filter(|schema| schema.is_object()));
            if let Some(child) = child {
                collect_sources(child, value, ids);
            }
        }
    } else if let Some(items) = value.as_array()
        && let Some(child) = schema.get("items")
    {
        for value in items {
            collect_sources(child, value, ids);
        }
    }
}

fn record_failure<T>(state: &mut State, result: &Result<T, HostError>) {
    match result {
        Err(HostError::Invalid(_)) => state.write_failures.invalid = true,
        Err(HostError::TooLarge) => state.write_failures.ceiling = true,
        Err(HostError::SourceUnreadable) => state.source_read_failure.store(true, Ordering::Release),
        Err(HostError::SourceRead(source)) => {
            state.source_read_failure.store(true, Ordering::Release);
            remember_read_cause(&state.source_read_cause, Arc::clone(source));
        },
        Err(HostError::SourceWrite(source)) => {
            state.source_failure.store(true, Ordering::Release);
            remember_read_cause(&state.source_write_cause, Arc::clone(source));
        },
        _ => {},
    }
}

fn valid_filename(filename: &str) -> bool {
    validate_source_filename(filename).is_ok()
}

fn redact(text: &str, tokens: &[Zeroizing<String>]) -> String {
    // Replacing a marker-bearing token repeatedly could grow exponentially
    // before the final safety check. Omit before allocating replacements.
    if tokens.iter().any(|token| "[redacted]".contains(token.as_str()))
        && tokens.iter().any(|token| text.contains(token.as_str()))
    {
        return String::new();
    }
    let mut text = text.to_owned();
    // Longer exact secrets win when tokens overlap.
    let mut ordered = tokens.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|token| Reverse(token.len()));
    for token in ordered {
        text = text.replace(token.as_str(), "[redacted]");
    }
    // An issued token can itself occur in the replacement marker or be
    // reconstructed by replacement boundaries. Omit such diagnostics rather
    // than persisting a secret or repeatedly expanding attacker-chosen text.
    if tokens
        .iter()
        .any(|token| !token.is_empty() && text.contains(token.as_str()))
    {
        text.clear();
    }
    text
}

fn truncate(text: &mut String, maximum: usize) {
    if text.len() > maximum {
        let mut end = maximum;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}

fn redact_failure(failure: &mut Failure, tokens: &[Zeroizing<String>]) {
    let message = match failure {
        Failure::InvalidParameters(message)
        | Failure::NotConnected(message)
        | Failure::Rejected(message)
        | Failure::Internal(message) => message,
        Failure::Unavailable(retry) => &mut retry.message,
    };
    *message = redact(message, tokens);
    truncate(message, MAX_LOG_LINE_BYTES);
}

struct CheckedSourceReader {
    reader: DuplexStream,
    failed: Arc<AtomicBool>,
}
impl AsyncRead for CheckedSourceReader {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if self.failed.load(Ordering::Acquire) {
            return Poll::Ready(Err(IoError::other("source stream failed")));
        }
        let result = Pin::new(&mut self.reader).poll_read(cx, buffer);
        if self.failed.load(Ordering::Acquire) {
            Poll::Ready(Err(IoError::other("source stream failed")))
        } else {
            result
        }
    }
}

fn redact_value(value: &mut serde_json::Value, tokens: &[Zeroizing<String>]) -> bool {
    match value {
        serde_json::Value::String(text) => *text = redact(text, tokens),
        serde_json::Value::Array(values) => {
            for value in values {
                if !redact_value(value, tokens) {
                    return false;
                }
            }
        },
        serde_json::Value::Object(properties) => {
            let previous = mem::take(properties);
            for (name, mut value) in previous {
                if !redact_value(&mut value, tokens) || properties.insert(redact(&name, tokens), value).is_some() {
                    return false;
                }
            }
        },
        _ => {},
    }
    true
}

fn validate_source_metadata(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    sources: &BTreeMap<&str, &Source>,
) -> Result<(), Error> {
    if schema.get("format").and_then(serde_json::Value::as_str) == Some("source") {
        if let Some(id) = value.as_str() {
            let source = sources
                .get(id)
                .ok_or_else(|| Error::invalid("source metadata is missing"))?;
            let maximum = schema
                .get("maxBytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(MAX_SOURCE_BYTES);
            if source.size > maximum {
                return Err(Error::invalid("source exceeds its declared byte bound"));
            }
            if let Some(media_types) = schema.get("mediaTypes").and_then(serde_json::Value::as_array)
                && !media_types
                    .iter()
                    .any(|media_type| media_type.as_str() == Some(source.media_type.as_str()))
            {
                return Err(Error::invalid("source media type does not satisfy its declaration"));
            }
        }
    } else if let Some(object) = value.as_object() {
        for (name, value) in object {
            let child = schema
                .get("properties")
                .and_then(|properties| properties.get(name))
                .or_else(|| schema.get("additionalProperties").filter(|schema| schema.is_object()));
            if let Some(child) = child {
                validate_source_metadata(child, value, sources)?;
            }
        }
    } else if let Some(items) = value.as_array()
        && let Some(child) = schema.get("items")
    {
        for value in items {
            validate_source_metadata(child, value, sources)?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct WriteFailures {
    invalid: bool,
    ceiling: bool,
}

type ReadCause = Arc<Mutex<Option<Arc<dyn StdError + Send + Sync>>>>;

fn remember_read_cause(cause: &ReadCause, source: Arc<dyn StdError + Send + Sync>) {
    let mut cause = cause.lock().unwrap_or_else(PoisonError::into_inner);
    if cause.is_none() {
        *cause = Some(source);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io,
        sync::Arc,
    };

    use tokio::{
        task::yield_now,
        time::timeout,
    };
    use zeroize::Zeroizing;

    use super::{
        AdmissionHost,
        Deadline,
        Engine,
        State,
        redact,
        validate_component,
    };

    #[tokio::test]
    async fn scratch_directories_are_isolated_and_removed_with_their_stores() {
        let engine = Engine::new().unwrap();
        let manifest = validate_component(include_bytes!("../tests/fixtures/host-guest.wasm")).unwrap();
        let first = engine
            .store(Arc::new(AdmissionHost), &manifest, None, Deadline::admission())
            .unwrap();
        let second = engine
            .store(Arc::new(AdmissionHost), &manifest, None, Deadline::admission())
            .unwrap();
        let State {
            _scratch: first_scratch,
            ..
        } = first.data();
        let State {
            _scratch: second_scratch,
            ..
        } = second.data();
        let first_path = first_scratch.path().to_owned();
        let second_path = second_scratch.path().to_owned();
        assert_ne!(first_path, second_path);
        fs::write(first_path.join("private"), b"first attempt").unwrap();
        assert!(!second_path.join("private").exists());
        drop(first);
        assert!(!first_path.exists());
        assert!(second_path.is_dir());
        drop(second);
        assert!(!second_path.exists());
    }

    #[test]
    fn repeated_marker_tokens_cannot_expand_before_safe_omission() {
        let tokens = vec![Zeroizing::new("e".to_owned()); 64];
        assert_eq!(redact("secret", &tokens), "");
        assert_eq!(redact("xyz", &tokens), "xyz");
    }

    #[tokio::test]
    async fn source_producer_streams_short_reads_pending_and_eof_without_staging() {
        use std::{
            pin::Pin,
            sync::{
                Mutex,
                atomic::{
                    AtomicBool,
                    AtomicUsize,
                    Ordering,
                },
            },
            task::{
                Context,
                Poll,
            },
            time::Duration,
        };

        use tokio::io::{
            AsyncRead,
            AsyncReadExt as _,
            ReadBuf,
            duplex,
        };
        use wasmtime::component::StreamReader;

        use super::{
            BytesConsumer,
            BytesProducer,
            STREAM_CHUNK_BYTES,
        };

        struct AlternatingReader {
            remaining: usize,
            polls: Arc<AtomicUsize>,
        }
        impl AsyncRead for AlternatingReader {
            fn poll_read(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buffer: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                assert!(buffer.remaining() <= STREAM_CHUNK_BYTES);
                if self.polls.fetch_add(1, Ordering::Relaxed).is_multiple_of(2) {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                let length = buffer.remaining().min(self.remaining).min(4093);
                buffer.put_slice(&vec![b'x'; length]);
                self.remaining -= length;
                Poll::Ready(Ok(()))
            }
        }
        let engine = Engine::new().unwrap();
        let manifest = validate_component(include_bytes!("../tests/fixtures/host-guest.wasm")).unwrap();
        for size in [0, 7, STREAM_CHUNK_BYTES * 2 + 7] {
            let mut store = engine
                .store(Arc::new(AdmissionHost), &manifest, None, Deadline::admission())
                .unwrap();
            let failed = Arc::new(AtomicBool::new(false));
            let polls = Arc::new(AtomicUsize::new(0));
            let producer = BytesProducer {
                reader: Box::pin(AlternatingReader {
                    remaining: size,
                    polls: Arc::clone(&polls),
                }),
                failed: Arc::clone(&failed),
                cause: Arc::new(Mutex::new(None)),
            };
            let (mut reader, writer) = duplex(STREAM_CHUNK_BYTES);
            let stream = StreamReader::new(&mut store, producer).unwrap();
            stream
                .pipe(
                    &mut store,
                    BytesConsumer {
                        writer,
                        failure: Arc::new(AtomicBool::new(false)),
                        exceeded: Arc::new(AtomicBool::new(false)),
                        written: 0,
                        bound: size as u64,
                    },
                )
                .unwrap();
            let mut output = Vec::new();
            timeout(
                Duration::from_secs(5),
                store.run_concurrent(async |_| {
                    reader.read_to_end(&mut output).await.unwrap();
                }),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(output, vec![b'x'; size]);
            assert!(!failed.load(Ordering::Acquire));
            assert!(polls.load(Ordering::Relaxed) >= 2);
        }
    }

    #[tokio::test]
    async fn item_consumer_preserves_order_past_channel_capacity_and_empty_streams() {
        use std::{
            collections::VecDeque,
            pin::Pin,
            task::{
                Context,
                Poll,
            },
            time::Duration,
        };

        use tokio::sync::mpsc;
        use tokio_util::sync::PollSender;
        use wasmtime::{
            StoreContextMut,
            component::{
                Destination,
                StreamProducer,
                StreamReader,
                StreamResult,
                VecBuffer,
            },
        };

        use super::ItemsConsumer;

        struct Items(VecDeque<String>);
        impl StreamProducer<State> for Items {
            type Buffer = VecBuffer<String>;
            type Item = String;

            fn poll_produce<'a>(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _store: StoreContextMut<'a, State>,
                mut destination: Destination<'a, String, VecBuffer<String>>,
                _finish: bool,
            ) -> Poll<wasmtime::Result<StreamResult>> {
                if let Some(item) = self.0.pop_front() {
                    destination.set_buffer(vec![item].into());
                    Poll::Ready(Ok(StreamResult::Completed))
                } else {
                    Poll::Ready(Ok(StreamResult::Dropped))
                }
            }
        }
        let engine = Engine::new().unwrap();
        let manifest = validate_component(include_bytes!("../tests/fixtures/host-guest.wasm")).unwrap();
        for count in [0, 1, 65] {
            let mut store = engine
                .store(Arc::new(AdmissionHost), &manifest, None, Deadline::admission())
                .unwrap();
            let expected = (0..count).map(|index| index.to_string()).collect::<Vec<_>>();
            let (sender, mut receiver) = mpsc::channel(2);
            let stream = StreamReader::new(&mut store, Items(expected.iter().cloned().collect())).unwrap();
            stream
                .pipe(
                    &mut store,
                    ItemsConsumer {
                        sender: PollSender::new(sender),
                    },
                )
                .unwrap();
            let mut output = Vec::new();
            timeout(
                Duration::from_secs(5),
                store.run_concurrent(async |_| {
                    while let Some(item) = receiver.recv().await {
                        output.push(item);
                        yield_now().await;
                    }
                }),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(output, expected);
        }
    }
}
