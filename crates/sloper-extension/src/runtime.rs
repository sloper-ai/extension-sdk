//! Run-scoped resource, source, and connection capabilities.

use std::{
    cell::{
        Cell,
        RefCell,
    },
    collections::{
        BTreeMap,
        VecDeque,
    },
    fmt::{
        self,
        Write as _,
    },
    io,
    marker::PhantomData,
    ops::{
        Deref,
        DerefMut,
    },
    pin::Pin,
    rc::{
        Rc,
        Weak,
    },
    task::{
        Context,
        Poll,
    },
};

use bindings::{
    exports::sloper::extension::action::{
        Failure,
        Request,
    },
    sloper::api::{
        credentials,
        log as host_log,
        resources,
        sources,
    },
};
use futures::{
    Future,
    FutureExt,
    Sink,
    StreamExt,
    TryStreamExt,
    channel::{
        mpsc,
        oneshot,
    },
    future::{
        AbortHandle,
        Abortable,
        LocalBoxFuture,
        Shared,
    },
    io::{
        AsyncBufRead,
        AsyncRead,
        AsyncWrite,
    },
    stream::{
        self,
        LocalBoxStream,
        Stream,
    },
};
use serde::{
    Deserialize,
    Deserializer,
    Serialize,
    Serializer,
    de::DeserializeOwned,
};
use sha2::{
    Digest,
    Sha256,
};
use wit_bindgen::{
    StreamReader as WitStreamReader,
    StreamResult,
};
use zeroize::Zeroizing;

use crate::{
    __private::bindings,
    ConnectionType,
    Error,
    Resource,
    Result,
};

// Byte and item ceilings; streaming uses bounded 64 KiB chunks.
const CHUNK_BYTES: usize = 64 * 1024;
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;

mod json;
const MAX_ITEMS: usize = 1000;
const MAX_SOURCE_BYTES: u64 = 100 * 1024 * 1024;

type Completion = Shared<LocalBoxFuture<'static, Result<()>>>;
type SourceCompletion = Shared<LocalBoxFuture<'static, Result<Source>>>;

thread_local! {
    static CURRENT: RefCell<Option<Rc<RunState>>> = const { RefCell::new(None) };
}

struct RunState {
    operation: String,
    configuration: String,
    metadata: RefCell<BTreeMap<String, Source>>,
    pending: RefCell<Vec<Completion>>,
    uploads: RefCell<Vec<Weak<UploadState>>>,
    failed: RefCell<Option<Error>>,
}
impl RunState {
    fn fail(&self, error: Error) -> Error {
        if self.failed.borrow().is_none() {
            *self.failed.borrow_mut() = Some(error.clone());
        }
        error
    }

    fn check(&self) -> Result<()> {
        self.failed.borrow().clone().map_or(Ok(()), Err)
    }

    fn register(&self, completion: Completion) {
        self.pending.borrow_mut().retain(|future| future.peek().is_none());
        self.pending.borrow_mut().push(completion);
    }

    async fn drain(&self) {
        for upload in self.uploads.borrow().iter().filter_map(Weak::upgrade) {
            upload.abandon();
        }
        let pending = std::mem::take(&mut *self.pending.borrow_mut());
        for completion in pending {
            if let Err(error) = completion.await {
                self.fail(error);
            }
        }
    }
}
fn current() -> Result<Rc<RunState>> {
    CURRENT
        .with(|current| current.borrow().clone())
        .ok_or(Error::internal("extension failed"))
}

/// The current operation's opaque identity for provider idempotency.
///
/// # Errors
/// Returns an internal error outside action dispatch.
pub fn operation() -> Result<String> {
    Ok(current()?.operation.clone())
}

/// Decodes the pinned non-secret configuration for the current run.
///
/// # Errors
/// Returns an internal error outside dispatch or for an incompatible type.
pub fn configuration<T: DeserializeOwned>() -> Result<T> {
    serde_json::from_str(&current()?.configuration).map_err(|_| Error::internal("extension failed"))
}

/// A freshly issued token whose secret is neither cloneable nor serializable.
pub struct AccessToken {
    value: Zeroizing<String>,
    expires_at: String,
    scopes: Vec<String>,
}
impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessToken").finish_non_exhaustive()
    }
}
impl AccessToken {
    /// Borrows the secret for an authenticated provider request.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.value
    }

    /// Borrows the host's RFC 3339 expiration.
    #[must_use]
    pub fn expires_at(&self) -> &str {
        &self.expires_at
    }

    /// Borrows the granted OAuth scopes.
    #[must_use]
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }
}

/// A typed declared OAuth connection, valid only in its dispatch.
pub struct Connection<T: ConnectionType> {
    run: Weak<RunState>,
    marker: PhantomData<fn() -> T>,
}
impl<T: ConnectionType> Clone for Connection<T> {
    fn clone(&self) -> Self {
        Self {
            run: self.run.clone(),
            marker: PhantomData,
        }
    }
}
impl<T: ConnectionType> fmt::Debug for Connection<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("name", &T::NAME)
            .finish_non_exhaustive()
    }
}
impl<T: ConnectionType> Connection<T> {
    /// Requests a fresh short-lived token without caching its value.
    ///
    /// # Errors
    /// Returns connection repair, cancellation, transient, or internal errors.
    ///
    /// # Cancel safety
    /// Dropping the future discards this token request.
    pub async fn access_token(&self) -> Result<AccessToken> {
        self.run.upgrade().ok_or(Error::internal("extension failed"))?.check()?;
        let token = credentials::access_token(T::NAME.into())
            .await
            .map_err(|error| Error::from_wit(&error))?;
        Ok(AccessToken {
            value: Zeroizing::new(token.value),
            expires_at: token.expires_at,
            scopes: token.scopes,
        })
    }
}

/// An opaque source identity with metadata when known in the current run.
#[derive(Clone, PartialEq, Eq)]
pub struct Source {
    id: String,
    filename: Option<String>,
    media_type: Option<String>,
    size: Option<u64>,
}
impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Source")
            .field("metadata_known", &self.size.is_some())
            .finish_non_exhaustive()
    }
}
impl Serialize for Source {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.id)
    }
}
impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        if id.is_empty() {
            return Err(serde::de::Error::custom("source identity is empty"));
        }
        Ok(current()
            .ok()
            .and_then(|run| run.metadata.borrow().get(&id).cloned())
            .unwrap_or(Self {
                id,
                filename: None,
                media_type: None,
                size: None,
            }))
    }
}
impl Source {
    /// Borrows the opaque source identity.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Borrows the native filename when metadata was supplied.
    #[must_use]
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    /// Borrows the detected media type when known.
    #[must_use]
    pub fn media_type(&self) -> Option<&str> {
        self.media_type.as_deref()
    }

    /// Returns the exact byte size when known.
    #[must_use]
    pub const fn size(&self) -> Option<u64> {
        self.size
    }

    /// Opens a bounded asynchronous buffer reader through the current host.
    ///
    /// # Errors
    /// Returns authorization, cancellation, or transient host failures.
    ///
    /// # Cancel safety
    /// Dropping the future or reader discards unread bytes.
    pub async fn open(&self) -> Result<SourceReader> {
        current()?.check()?;
        let raw = sources::open(self.id.clone())
            .await
            .map_err(|error| Error::from_wit(&error))?;
        Ok(SourceReader::new(raw))
    }
}

/// A bounded asynchronous source reader suitable for `copy_buf`.
pub struct SourceReader {
    inner: Pin<Box<dyn AsyncBufRead>>,
}
impl fmt::Debug for SourceReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceReader").finish_non_exhaustive()
    }
}
impl SourceReader {
    fn new(raw: WitStreamReader<u8>) -> Self {
        let chunks = stream::try_unfold(Some(raw), |raw| {
            async move {
                let Some(mut raw) = raw else {
                    return Ok(None);
                };
                let (status, bytes) = raw.read(Vec::with_capacity(CHUNK_BYTES)).await;
                match status {
                    StreamResult::Cancelled => Err(io::Error::from(Error::Stopped)),
                    StreamResult::Dropped if bytes.is_empty() => Ok(None),
                    StreamResult::Dropped => Ok(Some((bytes, None))),
                    StreamResult::Complete(_) => Ok(Some((bytes, Some(raw)))),
                }
            }
        });
        Self {
            inner: Box::pin(chunks.into_async_read()),
        }
    }
}
impl AsyncRead for SourceReader {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &mut [u8]) -> Poll<io::Result<usize>> {
        self.inner.as_mut().poll_read(cx, bytes)
    }
}
impl AsyncBufRead for SourceReader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        self.get_mut().inner.as_mut().poll_fill_buf(cx)
    }

    fn consume(mut self: Pin<&mut Self>, amount: usize) {
        self.inner.as_mut().consume(amount);
    }
}

/// A committed item carrying only its opaque revision seed.
pub struct Scanned<T> {
    value: T,
    seed: String,
}
impl<T> fmt::Debug for Scanned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scanned").finish_non_exhaustive()
    }
}
impl<T> Scanned<T> {
    /// Borrows the host-supplied resource revision seed.
    #[must_use]
    pub fn seed(&self) -> &str {
        &self.seed
    }

    /// Derives a deterministic, purpose-separated provider idempotency key.
    #[must_use]
    pub fn seed_key(&self, purpose: &str) -> String {
        let mut digest = Sha256::new();
        digest.update(self.seed.as_bytes());
        digest.update([0]);
        digest.update(purpose.as_bytes());
        let mut encoded = String::new();
        for byte in digest.finalize() {
            write!(&mut encoded, "{byte:02x}").expect("formatting into a String cannot fail");
        }
        encoded
    }
}
impl<T> Deref for Scanned<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}
impl<T> DerefMut for Scanned<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

/// Owned items and the cursor to make durable after the final chunk.
pub struct Batch<T> {
    items: Vec<T>,
    cursor: Option<String>,
}
impl<T> fmt::Debug for Batch<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Batch")
            .field("items", &self.items.len())
            .field("has_cursor", &self.cursor.is_some())
            .finish()
    }
}
impl<T> From<Vec<T>> for Batch<T> {
    fn from(items: Vec<T>) -> Self {
        Self {
            items,
            cursor: None,
        }
    }
}
impl<T> Batch<T> {
    /// Associates a provider cursor with all items in this batch.
    pub fn with_cursor(items: Vec<T>, cursor: impl Into<String>) -> Self {
        Self {
            items,
            cursor: Some(cursor.into()),
        }
    }
}

/// A lazy committed scan which fetches its next page only on demand.
pub struct Reader<T> {
    inner: LocalBoxStream<'static, Result<Scanned<T>>>,
}
impl<T> fmt::Debug for Reader<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader").finish_non_exhaustive()
    }
}
impl<T: DeserializeOwned + 'static> Reader<T> {
    fn new(name: String) -> Self {
        let scan = stream::try_unfold((name, VecDeque::<resources::Item>::new()), |(name, mut page)| {
            async move {
                loop {
                    if let Some(item) = page.pop_front() {
                        let value =
                            serde_json::from_str(&item.value).map_err(|_| Error::internal("extension failed"))?;
                        return Ok(Some((
                            Scanned {
                                value,
                                seed: item.seed,
                            },
                            (name, page),
                        )));
                    }
                    let Some(next) = resources::read(name.clone())
                        .await
                        .map_err(|error| Error::from_wit(&error))?
                    else {
                        return Ok(None);
                    };
                    // `remaining: None` denotes a whole-model scan, never EOF.
                    page = next.items.into();
                }
            }
        });
        Self {
            inner: scan.boxed_local(),
        }
    }
}
impl<T> Stream for Reader<T> {
    type Item = Result<Scanned<T>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

struct WriterState {
    host: Rc<resources::Writer>,
    cursor: Rc<RefCell<Option<String>>>,
    pending: Option<Completion>,
    run: Rc<RunState>,
}
/// A backpressured sink that commits each bounded host chunk in order.
///
/// Each encoded item is limited to 1 MiB, including JSON escaping. A batch is
/// split into chunks of at most 1,000 items and 8 MiB; only its final chunk
/// advances the provider cursor. Dispatch retains pending sends and observes
/// their failures even if the author drops the writer.
pub struct Writer<T> {
    state: Rc<RefCell<WriterState>>,
    marker: PhantomData<fn(T)>,
}
impl<T> Clone for Writer<T> {
    fn clone(&self) -> Self {
        Self {
            state: Rc::clone(&self.state),
            marker: PhantomData,
        }
    }
}
impl<T> fmt::Debug for Writer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Writer")
            .field("pending", &self.state.borrow().pending.is_some())
            .finish_non_exhaustive()
    }
}
impl<T: Serialize + 'static> Writer<T> {
    /// Returns the cursor pinned at admission or most recently checkpointed.
    #[must_use]
    pub fn cursor(&self) -> Option<String> {
        self.state.borrow().cursor.borrow().clone()
    }

    /// Observes all batches accepted by any clone of this writer.
    ///
    /// # Errors
    /// Returns retained serialization, host, or cancellation failures.
    ///
    /// # Cancel safety
    /// Dispatch retains and drains the operation if this future is dropped.
    pub async fn flush(&self) -> Result<()> {
        let mut writer = self.clone();
        futures::future::poll_fn(|cx| Pin::new(&mut writer).poll_flush(cx)).await
    }
}
impl<T: Resource + 'static> Writer<T> {
    /// Starts a bounded upload for a directly declared source property.
    ///
    /// The property is a 1–64 character identifier, such as `document`, in the
    /// resource's top-level properties. Schema paths are not accepted.
    ///
    /// # Errors
    /// Rejects undeclared property names or expired run capabilities.
    ///
    /// # Cancel safety
    /// An abandoned upload is drained and prevents a successful dispatch.
    pub async fn source(&self, property: &str, filename: &str) -> Result<SourceWriter> {
        self.flush().await?;
        let schema: serde_json::Value =
            serde_json::from_str(T::JSON).map_err(|_| Error::internal("extension failed"))?;
        let state = self.state.borrow();
        let invalid = || state.run.fail(Error::internal("extension failed"));
        if property.len() > 64
            || !property
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
            || !property
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(invalid());
        }
        let node = schema
            .get("properties")
            .and_then(|properties| properties.get(property))
            .filter(|node| node.get("format").and_then(serde_json::Value::as_str) == Some("source"))
            .ok_or_else(invalid)?;
        let limit = node
            .get("maxBytes")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(MAX_SOURCE_BYTES)
            .min(MAX_SOURCE_BYTES);
        Ok(SourceWriter::new(
            Rc::clone(&state.host),
            &state.run,
            property.into(),
            filename.into(),
            limit,
        ))
    }
}
impl<T: Serialize + 'static> Sink<Batch<T>> for Writer<T> {
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.poll_flush(cx)
    }

    fn start_send(self: Pin<&mut Self>, batch: Batch<T>) -> Result<()> {
        let mut state = self.state.borrow_mut();
        state.run.check()?;
        if state.pending.as_ref().is_some_and(|future| future.peek().is_none()) {
            return Err(state.run.fail(Error::internal("extension failed")));
        }
        let host = Rc::clone(&state.host);
        let cursor = Rc::clone(&state.cursor);
        let run = Rc::downgrade(&state.run);
        let completion = async move {
            let result = write_batch(host, cursor, batch).await;
            if let (Err(error), Some(run)) = (&result, run.upgrade()) {
                run.fail(error.clone());
            }
            result
        }
        .boxed_local()
        .shared();
        state.run.register(completion.clone());
        state.pending = Some(completion);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let mut state = self.state.borrow_mut();
        if let Some(pending) = &mut state.pending {
            match Pin::new(pending).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => {
                    state.pending = None;
                    result?;
                },
            }
        }
        Poll::Ready(state.run.check())
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.poll_flush(cx)
    }
}

async fn write_batch<T: Serialize>(
    host: Rc<resources::Writer>,
    cursor: Rc<RefCell<Option<String>>>,
    batch: Batch<T>,
) -> Result<()> {
    let mut values = batch.items.into_iter().peekable();
    let mut next = None;
    loop {
        let mut chunk = Vec::new();
        let mut size = 0usize;
        while chunk.len() < MAX_ITEMS {
            let encoded = match next.take() {
                Some(value) => value,
                None => {
                    match values.next() {
                        Some(value) => json::encode(&value)?,
                        None => break,
                    }
                },
            };
            if size + encoded.len() > MAX_BATCH_BYTES && !chunk.is_empty() {
                next = Some(encoded);
                break;
            }
            size += encoded.len();
            chunk.push(encoded);
        }
        let last = next.is_none() && values.peek().is_none();
        if !chunk.is_empty() {
            write_chunk(&host, chunk).await?;
        }
        let checkpoint = if last {
            batch.cursor.clone().or_else(|| cursor.borrow().clone())
        } else {
            cursor.borrow().clone()
        };
        host.checkpoint(checkpoint.clone().unwrap_or_default())
            .await
            .map_err(|error| Error::from_wit(&error))?;
        *cursor.borrow_mut() = checkpoint;
        if last {
            return Ok(());
        }
    }
}
async fn write_chunk(host: &resources::Writer, values: Vec<String>) -> Result<()> {
    let (mut output, input) = bindings::wit_stream::new::<String>();
    let feed = async move {
        let remaining = output.write_all(values).await;
        if remaining.is_empty() {
            Ok(())
        } else {
            Err(Error::internal("extension failed"))
        }
    };
    let (write_result, stream_result) = futures::join!(host.write(input), feed);
    write_result.map_err(|error| Error::from_wit(&error))?;
    stream_result
}
struct UploadChunk {
    bytes: Vec<u8>,
    acknowledged: oneshot::Sender<Result<()>>,
}
struct UploadState {
    sender: RefCell<Option<mpsc::Sender<UploadChunk>>>,
    result: SourceCompletion,
    abort: AbortHandle,
    run: Weak<RunState>,
    finished: Cell<bool>,
}
impl UploadState {
    fn fail(&self, error: Error) -> Error {
        self.abort.abort();
        if let Some(run) = self.run.upgrade() {
            run.fail(error.clone());
        }
        error
    }

    fn close_sender(&self) {
        if let Some(mut sender) = self.sender.borrow_mut().take() {
            sender.close_channel();
        }
    }

    fn abandon(&self) {
        if !self.finished.get() {
            self.fail(Error::internal("source upload was not sealed"));
        }
    }
}

/// A bounded asynchronous upload sealed explicitly before publishing its ID.
pub struct SourceWriter {
    state: Rc<UploadState>,
    pending: Option<oneshot::Receiver<Result<()>>>,
    closing: Option<SourceCompletion>,
    size: Rc<Cell<u64>>,
    limit: u64,
}
impl fmt::Debug for SourceWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceWriter")
            .field("written", &self.size.get())
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}
impl SourceWriter {
    fn new(host: Rc<resources::Writer>, run: &Rc<RunState>, property: String, filename: String, limit: u64) -> Self {
        let (sender, mut chunks) = mpsc::channel::<UploadChunk>(1);
        let (done, completed) = oneshot::channel::<Result<Source>>();
        let (abort, cancellation) = AbortHandle::new_pair();
        let size = Rc::new(Cell::new(0u64));
        let count = Rc::clone(&size);
        let owner = Rc::downgrade(run);
        wit_bindgen::spawn_local(async move {
            let (output, input) = bindings::wit_stream::new::<u8>();
            // The endpoint lives outside the cancellable call. Cancellation
            // drops the imported host subtask before EOF can reach its reader,
            // so an abandoned upload cannot seal a truncated source.
            let mut output = Some(output);
            let upload = async {
                let pump = async {
                    while let Some(chunk) = chunks.next().await {
                        let remaining = output
                            .as_mut()
                            .expect("stream stays open until explicit seal")
                            .write_all(chunk.bytes)
                            .await;
                        let result = if remaining.is_empty() {
                            Ok(())
                        } else {
                            Err(Error::internal("source stream closed before upload completed"))
                        };
                        let _ = chunk.acknowledged.send(result.clone());
                        result?;
                    }
                    // Only an explicit successful close shuts the channel.
                    // Dropping this endpoint gives the host a successful EOF.
                    drop(output.take());
                    Ok(())
                };
                let (sealed, pumped): (_, Result<()>) =
                    futures::join!(host.source(property, filename.clone(), input), pump);
                sealed.map_err(|error| Error::from_wit(&error)).and_then(|id| {
                    pumped?;
                    Ok(Source {
                        id,
                        filename: Some(filename),
                        media_type: None,
                        size: Some(count.get()),
                    })
                })
            };
            let result = Abortable::new(upload, cancellation)
                .await
                .unwrap_or_else(|_| Err(Error::internal("source upload was not sealed")));
            drop(output);
            if let Some(run) = owner.upgrade() {
                match &result {
                    Ok(source) => {
                        run.metadata.borrow_mut().insert(source.id.clone(), source.clone());
                    },
                    Err(error) => {
                        run.fail(error.clone());
                    },
                }
            }
            let _ = done.send(result);
        });
        let result = async move { completed.await.map_err(|_| Error::internal("extension failed"))? }
            .boxed_local()
            .shared();
        let state = Rc::new(UploadState {
            sender: RefCell::new(Some(sender)),
            result: result.clone(),
            abort,
            run: Rc::downgrade(run),
            finished: Cell::new(false),
        });
        run.uploads.borrow_mut().retain(|upload| upload.strong_count() > 0);
        run.uploads.borrow_mut().push(Rc::downgrade(&state));
        run.register(async move { result.await.map(|_| ()) }.boxed_local().shared());
        Self {
            state,
            pending: None,
            closing: None,
            size,
            limit,
        }
    }

    /// Seals exact uploaded bytes and returns the durable source identity.
    ///
    /// # Errors
    /// Returns a retained upload, declared size, or host failure.
    ///
    /// # Cancel safety
    /// Dropping this future drains the upload and prevents run success.
    pub async fn close(mut self) -> Result<Source> {
        futures::io::AsyncWriteExt::flush(&mut self)
            .await
            .map_err(Error::from)?;
        self.state.close_sender();
        let result = self.state.result.clone().await;
        self.state.finished.set(true);
        if let Err(error) = result {
            return Err(self.state.fail(error));
        }
        result
    }

    fn poll_ack(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(pending) = &mut self.pending {
            match Pin::new(pending).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => {
                    self.pending = None;
                    if let Err(error) = result.unwrap_or(Err(Error::internal("extension failed"))) {
                        return Poll::Ready(Err(self.state.fail(error).into()));
                    }
                },
            }
        }
        Poll::Ready(Ok(()))
    }
}
impl Drop for SourceWriter {
    fn drop(&mut self) {
        self.state.abandon();
    }
}
impl AsyncWrite for SourceWriter {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        futures::ready!(self.poll_ack(cx))?;
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if bytes.len() as u64 > self.limit.saturating_sub(self.size.get()) {
            return Poll::Ready(Err(self.state.fail(Error::TooLarge).into()));
        }
        let amount = bytes.len().min(CHUNK_BYTES);
        let (acknowledged, pending) = oneshot::channel();
        {
            let mut sender = self.state.sender.borrow_mut();
            let Some(sender) = sender.as_mut() else {
                return Poll::Ready(Err(self.state.fail(Error::internal("extension failed")).into()));
            };
            futures::ready!(Pin::new(&mut *sender).poll_ready(cx))
                .map_err(|_| io::Error::from(self.state.fail(Error::internal("extension failed"))))?;
            Pin::new(sender)
                .start_send(UploadChunk {
                    bytes: bytes[..amount].to_vec(),
                    acknowledged,
                })
                .map_err(|_| io::Error::from(self.state.fail(Error::internal("extension failed"))))?;
        }
        self.size.set(self.size.get() + amount as u64);
        self.pending = Some(pending);
        Poll::Ready(Ok(amount))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_ack(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        futures::ready!(self.poll_ack(cx))?;
        self.state.close_sender();
        if self.closing.is_none() {
            self.closing = Some(self.state.result.clone());
        }
        let closing = self.closing.as_mut().expect("closing was initialized above");
        match Pin::new(closing).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.state.finished.set(true);
                Poll::Ready(result.map(|_| ()).map_err(|error| self.state.fail(error).into()))
            },
        }
    }
}

/// Run-scoped dispatch composition used by generated actions.
pub struct RunContext {
    request: Request,
    state: Rc<RunState>,
}
impl fmt::Debug for RunContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunContext")
            .field("action", &self.request.action)
            .finish_non_exhaustive()
    }
}
impl RunContext {
    fn new(request: Request) -> Self {
        let metadata = request
            .sources
            .iter()
            .map(|source| {
                (
                    source.id.clone(),
                    Source {
                        id: source.id.clone(),
                        filename: Some(source.filename.clone()),
                        media_type: Some(source.media_type.clone()),
                        size: Some(source.size),
                    },
                )
            })
            .collect();
        let state = Rc::new(RunState {
            operation: request.operation.clone(),
            configuration: request.configuration.clone(),
            metadata: RefCell::new(metadata),
            pending: RefCell::new(Vec::new()),
            uploads: RefCell::new(Vec::new()),
            failed: RefCell::new(None),
        });
        Self {
            request,
            state,
        }
    }

    /// Returns the action name selected by the host.
    #[must_use]
    pub fn action(&self) -> &str {
        &self.request.action
    }

    /// Returns the current operation identity.
    #[must_use]
    pub fn operation(&self) -> &str {
        &self.request.operation
    }

    /// Decodes validated owned parameters.
    ///
    /// # Errors
    /// Returns invalid parameters for an incompatible authored type.
    pub fn parameters<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_str(&self.request.parameters).map_err(|_| Error::invalid_parameters("invalid action input"))
    }

    /// Decodes pinned configuration.
    ///
    /// # Errors
    /// Returns an internal error for a mismatched configuration type.
    pub fn configuration<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_str(&self.request.configuration).map_err(|_| Error::internal("extension failed"))
    }

    /// Lends a declared typed connection to the generated action.
    #[must_use]
    pub fn connection<T: ConnectionType>(&self) -> Connection<T> {
        Connection {
            run: Rc::downgrade(&self.state),
            marker: PhantomData,
        }
    }

    /// Creates a lazy scan without fetching a page.
    ///
    /// # Errors
    /// Returns a retained run failure.
    ///
    /// # Cancel safety
    /// No host page is consumed before the reader is polled.
    pub fn reader<T: Resource + 'static>(&self) -> impl Future<Output = Result<Reader<T>>> + '_ {
        std::future::ready(self.state.check().map(|()| Reader::new(T::NAME.into())))
    }

    /// Opens one host writer and registers its pending work for dispatch drain.
    ///
    /// # Errors
    /// Returns declared capability or host admission failures.
    ///
    /// # Cancel safety
    /// Dropping an unopened future creates no SDK writer.
    pub async fn writer<T: Resource + 'static>(&self, connection: Option<&str>) -> Result<Writer<T>> {
        self.state.check()?;
        let host = resources::open(T::NAME.into(), connection.map(str::to_owned))
            .await
            .map_err(|error| Error::from_wit(&error))?;
        let cursor = self
            .request
            .cursors
            .iter()
            .find(|cursor| cursor.name == T::NAME)
            .map(|cursor| cursor.value.clone());
        Ok(Writer {
            state: Rc::new(RefCell::new(WriterState {
                host: Rc::new(host),
                cursor: Rc::new(RefCell::new(cursor)),
                pending: None,
                run: Rc::clone(&self.state),
            })),
            marker: PhantomData,
        })
    }
}
struct AmbientGuard(Option<Rc<RunState>>);
impl Drop for AmbientGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| {
            *current.borrow_mut() = self.0.take();
        });
    }
}

struct Logger;
impl log::Log for Logger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        if CURRENT.with(|current| current.borrow().is_none()) {
            return;
        }
        let level = match record.level() {
            log::Level::Error => host_log::Level::Error,
            log::Level::Warn => host_log::Level::Warn,
            log::Level::Info => host_log::Level::Info,
            log::Level::Debug | log::Level::Trace => host_log::Level::Debug,
        };
        host_log::write(level, &record.args().to_string());
    }

    fn flush(&self) {}
}
static LOGGER: Logger = Logger;

/// Invokes an action and drains all accepted resource and source work.
///
/// # Errors
/// Returns a static WIT failure after pending work has been observed.
///
/// # Cancel safety
/// The host owns final cancellation and fencing if dispatch itself is dropped.
pub async fn dispatch(
    request: Request,
    action: impl for<'a> FnOnce(&'a RunContext) -> LocalBoxFuture<'a, Result<()>>,
) -> std::result::Result<(), Failure> {
    let context = RunContext::new(request);
    let previous = CURRENT.with(|current| current.replace(Some(Rc::clone(&context.state))));
    let _guard = AmbientGuard(previous);
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Trace);
    }
    let result = execute(async {
        let result = action(&context).await;
        context.state.drain().await;
        context.state.check().and(result)
    })
    .await;
    match result {
        Ok(()) | Err(Error::Stopped) => Ok(()),
        Err(error) => Err(error.into_failure()),
    }
}

// Native callers already supply their executor. Component exports use the
// wit-bindgen executor, which cannot drive Tokio's WASI socket and timer
// reactor.
#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
async fn execute(work: impl Future<Output = Result<()>>) -> Result<()> {
    work.await
}

#[cfg(all(target_os = "wasi", target_env = "p2"))]
async fn execute(work: impl Future<Output = Result<()>>) -> Result<()> {
    use std::time::Duration;

    use futures::future::{
        Either,
        select,
    };

    // Bound each reactor turn so component-model imports, stream pumps, and
    // cancellation regain control even while TCP requests remain pending.
    const REACTOR_QUANTUM: Duration = Duration::from_millis(1);

    // Reactor components do not run the standard library's command startup,
    // so apply the working directory supplied by the host before action code.
    if let Some(directory) = wasip2::cli::environment::initial_cwd() {
        std::env::set_current_dir(directory)
            .map_err(|_| Error::internal("scratch working directory is unavailable"))?;
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let mut work = std::pin::pin!(work);
    loop {
        let result = runtime.block_on(async {
            let turn = std::pin::pin!(tokio::time::sleep(REACTOR_QUANTUM));
            match select(work.as_mut(), turn).await {
                Either::Left((result, _)) => Some(result),
                Either::Right(_) => None,
            }
        });
        if let Some(result) = result {
            return result;
        }
        wit_bindgen::yield_async().await;
    }
}

#[cfg(test)]
mod tests;
