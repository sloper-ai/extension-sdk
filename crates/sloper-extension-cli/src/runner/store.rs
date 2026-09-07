use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    fs,
    io::{
        self,
        BufWriter,
        Cursor,
        Write as _,
    },
    mem,
    path::{
        Path,
        PathBuf,
    },
    pin::Pin,
    sync::{
        Arc,
        Mutex,
    },
    task::{
        Context,
        Poll,
    },
    time::Duration,
};

use async_trait::async_trait;
use serde::{
    Deserialize,
    Serialize,
};
use serde_json::{
    Map,
    Value,
    json,
};
use sha2::{
    Digest as _,
    Sha256,
};
use sloper_extension_host::{
    AccessToken,
    Cursor as HostCursor,
    Host,
    HostError,
    Item,
    LogKind,
    LogLevel,
    Page,
    Source,
};
use sloper_extension_spec::{
    Capability,
    Manifest,
    detect_media_type,
    parse_object,
    validate_source_property,
};
use time::format_description::well_known::Rfc3339;
use tokio::{
    io::{
        AsyncRead,
        AsyncReadExt as _,
        ReadBuf,
    },
    sync::mpsc,
};

use super::{
    super::{
        Error,
        hex,
    },
    inputs::{
        Inputs,
        LocalSource,
        source_allowed,
        validate_filename,
        validate_sources,
    },
};
use crate::serialized_exceeds;

// Bound each capability call and retained reader page.
const MAX_BATCH_ITEMS: usize = 1000;
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
const MAX_ITEM_BYTES: usize = 1024 * 1024;
const MAX_CURSOR_BYTES: usize = 1024;
// Operation summaries retain at most 100 keys and 64 KiB of encoded keys.
const MAX_SUMMARY_KEYS: usize = 100;
const MAX_SUMMARY_KEY_BYTES: usize = 64 * 1024;
// Charge final UTF-8 plus replay metadata and reserve one full
// notice.
const MAX_LOG_LINE_BYTES: usize = 4 * 1024;
const MAX_LOG_BYTES: usize = 1024 * 1024;
const LOG_ENTRY_BYTES: usize = 192;
const LOG_DROPPED_MESSAGE: &str = "Further attempt logs were dropped after the 1 MiB limit.";

pub(super) struct LocalHost {
    directory: PathBuf,
    manifest: Manifest,
    inputs: Inputs,
    operation: String,
    workspace: String,
    input_commit: String,
    state: Arc<Mutex<State>>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Reader {
    acknowledged: usize,
    #[serde(skip)]
    pending: Option<usize>,
    drained: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Writer {
    items: BTreeMap<String, Value>,
    written: u64,
    cursor: String,
    #[serde(skip)]
    staged: Vec<(String, Value)>,
    #[serde(skip)]
    staged_bytes: usize,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    admitted: bool,
    operation: Value,
    readers: BTreeMap<String, Reader>,
    writers: BTreeMap<String, Writer>,
    #[serde(skip)]
    sources: BTreeMap<String, LocalSource>,
    produced: BTreeSet<String>,
    source_bindings: BTreeMap<String, (String, String)>,
    lent: BTreeSet<String>,
    logs: Vec<String>,
    log_bytes: usize,
    source_reads: BTreeMap<String, Value>,
    #[serde(skip)]
    logs_dropped: bool,
}

impl LocalHost {
    pub(super) fn new(directory: &Path, manifest: Manifest, mut inputs: Inputs) -> Result<Self, Error> {
        // Field order preserves the original JSON object bytes and commit hash.
        #[derive(Serialize)]
        struct Snapshot<'a> {
            resources: &'a BTreeMap<String, Vec<Value>>,
            parameters: &'a Value,
            configuration: &'a Value,
            sources: BTreeMap<&'a str, String>,
        }
        let operation = uuid::Uuid::new_v4().simple().to_string();
        let workspace = format!("ws_{}", uuid::Uuid::new_v4().simple());
        let initial = Snapshot {
            configuration: &inputs.configuration,
            parameters: &inputs.parameters,
            resources: &inputs.readers,
            sources: inputs
                .sources
                .iter()
                .map(|(id, source)| (id.as_str(), digest(&source.bytes)))
                .collect(),
        };
        let input_commit = write_commit(&directory.join("input.json"), &initial)?;
        let readers = inputs
            .readers
            .keys()
            .map(|name| (name.clone(), Reader::default()))
            .collect();
        let writers = manifest.actions[&inputs.action]
            .resources
            .iter()
            .filter(|(_, caps)| caps.contains(&Capability::Write))
            .map(|(name, _)| (name.clone(), Writer::default()))
            .collect();
        let mut lent = BTreeSet::new();
        collect_sources(&inputs.parameters, &inputs.sources, &mut |id| {
            if !lent.contains(id) {
                lent.insert(id.to_owned());
            }
        });
        let state = State {
            readers,
            writers,
            sources: mem::take(&mut inputs.sources),
            lent,
            ..State::default()
        };
        Ok(Self {
            directory: directory.to_owned(),
            manifest,
            inputs,
            operation,
            workspace,
            input_commit,
            state: Arc::new(Mutex::new(state)),
        })
    }

    #[inline]
    pub(super) fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub(super) fn action(&self) -> &str {
        &self.inputs.action
    }

    pub(super) fn operation_id(&self) -> &str {
        &self.operation
    }

    pub(super) fn parameters(&self) -> &Value {
        &self.inputs.parameters
    }

    pub(super) fn configuration(&self) -> &Value {
        &self.inputs.configuration
    }

    pub(super) fn parameter_sources(&self) -> Result<Vec<Source>, Error> {
        let state = self
            .state
            .lock()
            .map_err(|_| Error::invalid_response("local store lock is poisoned"))?;
        let mut ids = BTreeSet::new();
        collect_sources(&self.inputs.parameters, &state.sources, &mut |id| {
            ids.insert(id);
        });
        Ok(ids
            .into_iter()
            .map(|id| {
                let source = &state.sources[id];
                Source {
                    id: id.to_owned(),
                    filename: source.filename.clone(),
                    media_type: source.media_type.to_owned(),
                    size: source.bytes.len() as u64,
                }
            })
            .collect())
    }

    pub(super) fn admit(&self) -> Result<(), Error> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::invalid_response("local store lock is poisoned"))?;
        let time = timestamp()?;
        state.operation = json!({"id":self.operation,"workspaceId":self.workspace,"branch":"local","inputCommitId":self.input_commit,
            "workspaceVersion":"1","extensionAction":{"extensionId":self.manifest.name,"actionId":self.inputs.action},
            "state":"QUEUED","actor":{"kind":"AGENT","origin":"CLI","id":"local-run"},"parameters":self.inputs.parameters,
            "attempt":0});
        state.operation["createTime"] = Value::String(time.clone());
        state.operation["updateTime"] = Value::String(time);
        state.admitted = true;
        self.persist(&state)?;
        Ok(())
    }

    pub(super) fn start(&self, attempt: u32) -> Result<(), Error> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::invalid_response("local store lock is poisoned"))?;
        if state.operation["state"] != "QUEUED" || state.operation["attempt"].as_u64() != Some(u64::from(attempt - 1)) {
            return Err(Error::invalid_response("local operation cannot claim this attempt"));
        }
        state.log_bytes = 0;
        state.logs_dropped = false;
        state.lent.clear();
        let State {
            sources,
            lent,
            ..
        } = &mut *state;
        collect_sources(&self.inputs.parameters, sources, &mut |id| {
            if !lent.contains(id) {
                lent.insert(id.to_owned());
            }
        });
        let time = timestamp()?;
        if attempt == 1 {
            state.operation["startTime"] = Value::String(time.clone());
        }
        state.operation["updateTime"] = Value::String(time);
        state.operation["state"] = json!("RUNNING");
        state.operation["attempt"] = json!(attempt);
        if let Some(operation) = state.operation.as_object_mut() {
            operation.remove("nextAttemptTime");
        }
        for reader in state.readers.values_mut() {
            reader.pending = None;
        }
        for writer in state.writers.values_mut() {
            writer.staged.clear();
            writer.staged_bytes = 0;
        }
        self.persist(&state)?;
        Ok(())
    }

    pub(super) fn retry(&self, delay: Duration) -> Result<(), Error> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::invalid_response("local store lock is poisoned"))?;
        Self::active(&state)?;
        state.operation["state"] = json!("QUEUED");
        state.operation["updateTime"] = Value::String(timestamp()?);
        let next = time::OffsetDateTime::now_utc() + delay;
        state.operation["nextAttemptTime"] = Value::String(
            next.format(&Rfc3339)
                .map_err(|_| Error::invalid_response("local clock cannot be encoded"))?,
        );
        self.persist(&state)?;
        Ok(())
    }

    pub(super) fn cursors(&self) -> Result<Vec<HostCursor>, Error> {
        let state = self
            .state
            .lock()
            .map_err(|_| Error::invalid_response("local store lock is poisoned"))?;
        if !state.readers.is_empty() {
            return Ok(Vec::new());
        }
        Ok(state
            .writers
            .iter()
            .map(|(name, writer)| {
                HostCursor {
                    name: name.clone(),
                    value: writer.cursor.clone(),
                }
            })
            .collect())
    }

    pub(super) fn complete(&self, mut failure: Option<Value>) -> Result<(), Error> {
        if let Some(failure) = failure.as_mut() {
            self.redact_value(failure)?;
            if let Some(Value::String(message)) = failure.get_mut("message") {
                truncate(message, MAX_LOG_LINE_BYTES);
            }
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::invalid_response("local store lock is poisoned"))?;
        Self::active(&state)?;
        for writer in state.writers.values_mut() {
            writer.staged.clear();
            writer.staged_bytes = 0;
        }
        let time = timestamp()?;
        state.operation["endTime"] = Value::String(time.clone());
        state.operation["updateTime"] = Value::String(time);
        if let Some(failure) = failure {
            state.operation["state"] = json!("FAILED");
            state.operation["failure"] = failure;
        } else {
            // Preserve the original JSON object field order and Writer serializer.
            #[derive(Serialize)]
            struct Snapshot<'a> {
                parent: &'a str,
                operation: &'a str,
                resources: &'a BTreeMap<String, Writer>,
                sources: BTreeMap<&'a str, String>,
            }
            let commit = Snapshot {
                operation: &self.operation,
                parent: &self.input_commit,
                resources: &state.writers,
                sources: state
                    .produced
                    .iter()
                    .map(|id| (id.as_str(), digest(&state.sources[id].bytes)))
                    .collect(),
            };
            let digest = write_commit(&self.directory.join("result.json"), &commit)?;
            state.operation["state"] = json!("SUCCEEDED");
            state.operation["resultCommitId"] = Value::String(digest);
        }
        state.operation["resources"] = Value::Array(
            state
                .writers
                .iter()
                .map(|(name, writer)| {
                    let mut resource = json!({"extensionResourceId":name,"inbox":"0"});
                    resource["written"] = Value::String(writer.written.to_string());
                    resource
                })
                .collect(),
        );
        state.operation["reads"] = Value::Array(
            state
                .readers
                .iter()
                .map(|(name, reader)| {
                    let mut read = json!({"extensionResourceId":name,"selected":false,"skipped":"0","absent":"0"});
                    read["total"] = Value::String(self.inputs.readers[name].len().to_string());
                    read["read"] = Value::String(reader.acknowledged.to_string());
                    read
                })
                .collect(),
        );
        let (keys, truncated) = written_item_keys(&state.writers)?;
        state.operation["writtenItemKeysTruncated"] = Value::Bool(truncated);
        state.operation["writtenItemKeys"] = Value::Array(keys);
        state.operation["sourceCount"] = Value::String(state.source_reads.len().to_string());
        state.operation["settledCount"] = json!("0");
        self.persist(&state)?;
        Ok(())
    }

    pub(super) fn export(&self, out: &Path) -> Result<Value, Error> {
        if out.try_exists()? {
            return Err(Error::usage("local run output directory already exists"));
        }
        let parent = out
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let output = tempfile::tempdir_in(parent)?;
        let state = self
            .state
            .lock()
            .map_err(|_| Error::invalid_response("local store lock is poisoned"))?;
        let mut resources = Vec::new();
        for (name, writer) in &state.writers {
            let file = format!("{name}.jsonl");
            let mut stream = fs::File::create(output.path().join(&file))?;
            for item in writer.items.values() {
                serde_json::to_writer(&mut stream, item)?;
                stream.write_all(b"\n")?;
            }
            stream.sync_all()?;
            let mut resource = json!({"extensionResourceId":name,"path":out.join(file)});
            resource["items"] = Value::String(writer.items.len().to_string());
            resources.push(resource);
        }
        let mut sources = Vec::new();
        for id in &state.produced {
            let source = &state.sources[id];
            let relative = Path::new("sources").join(id).join(export_filename(&source.filename));
            let path = output.path().join(&relative);
            fs::create_dir_all(
                path.parent()
                    .ok_or_else(|| Error::invalid_response("source export has no parent"))?,
            )?;
            fs::write(path, source.bytes.as_ref())?;
            sources.push(json!({"sourceId":id,"path":out.join(relative)}));
        }
        for log in &state.logs {
            eprintln!("{log}");
        }
        fs::rename(output.path(), out)?;
        let mut result = json!({"operation":state.operation,"out":out});
        result["resources"] = Value::Array(resources);
        result["sources"] = Value::Array(sources);
        Ok(result)
    }

    fn persist(&self, state: &State) -> Result<(), Error> {
        atomic_write(&self.directory.join("working.json"), &mut |writer| {
            serde_json::to_writer(writer, state).map_err(io::Error::other)
        })?;
        Ok(())
    }

    fn redact(&self, message: &str) -> String {
        // A token in the marker makes every replacement unsafe. Detect that
        // before repeated tokens can exponentially expand the marker itself.
        if self
            .inputs
            .tokens
            .values()
            .any(|token| !token.is_empty() && "[redacted]".contains(token))
            && self.inputs.tokens.values().any(|token| message.contains(token))
        {
            return String::new();
        }
        let mut message = self.inputs.tokens.values().fold(message.to_owned(), |message, token| {
            message.replace(token, "[redacted]")
        });
        // Replacement markers and their boundaries can themselves contain a
        // token. Omit those diagnostics without repeatedly expanding them.
        if self
            .inputs
            .tokens
            .values()
            .any(|token| !token.is_empty() && message.contains(token))
        {
            message.clear();
        }
        message
    }

    fn log_message(&self, level: LogLevel, message: &str) -> String {
        let mut message = self.redact(&format!("{level:?}: {message}"));
        truncate(&mut message, MAX_LOG_LINE_BYTES);
        message
    }

    fn redact_value(&self, value: &mut Value) -> Result<(), HostError> {
        // Leave null behind if any nested property collision rejects the value.
        *value = match value.take() {
            Value::String(text) => Value::String(self.redact(&text)),
            Value::Array(mut values) => {
                for value in &mut values {
                    self.redact_value(value)?;
                }
                Value::Array(values)
            },
            Value::Object(values) => {
                let mut redacted = Map::new();
                for (name, mut value) in values {
                    self.redact_value(&mut value)?;
                    if redacted.insert(self.redact(&name), value).is_some() {
                        return Err(HostError::invalid("redacted item property names collide"));
                    }
                }
                Value::Object(redacted)
            },
            value => value,
        };
        Ok(())
    }

    fn active(state: &State) -> Result<(), HostError> {
        if !state.admitted || state.operation["state"] != "RUNNING" {
            return Err(HostError::stopped());
        }
        Ok(())
    }

    fn capability(&self, name: &str, capability: Capability) -> Result<(), HostError> {
        if !self.manifest.actions[self.action()]
            .resources
            .get(name)
            .is_some_and(|caps| caps.contains(&capability))
        {
            return Err(HostError::unauthorized());
        }
        Ok(())
    }
}

#[async_trait]
impl Host for LocalHost {
    async fn access_token(&self, connection: &str) -> Result<AccessToken, HostError> {
        let state = self.state.lock().map_err(|_| HostError::unavailable())?;
        Self::active(&state)?;
        if !self.manifest.actions[self.action()]
            .connections
            .iter()
            .any(|name| name == connection)
        {
            return Err(HostError::unauthorized());
        }
        let value = self
            .inputs
            .tokens
            .get(connection)
            .ok_or_else(HostError::unauthorized)?
            .clone();
        Ok(AccessToken {
            value,
            // This is the local host's cache lease. The opaque developer
            // token itself is never refreshed or reinterpreted as a session.
            expires_at: (time::OffsetDateTime::now_utc() + time::Duration::minutes(5))
                .format(&Rfc3339)
                .map_err(|_| HostError::unavailable())?,
            scopes: self.manifest.connections[connection].scopes.clone(),
        })
    }

    async fn open_source(&self, id: &str) -> Result<Pin<Box<dyn AsyncRead + Send>>, HostError> {
        let state = self.state.lock().map_err(|_| HostError::unavailable())?;
        Self::active(&state)?;
        if !state.lent.contains(id) && !state.source_bindings.contains_key(id) {
            return Err(HostError::unauthorized());
        }
        let source = state.sources.get(id).ok_or_else(HostError::source_unreadable)?;
        Ok(Box::pin(SourceReader {
            bytes: Cursor::new(Arc::clone(&source.bytes)),
            state: Arc::clone(&self.state),
            path: self.directory.join("working.json"),
            attempt: state.operation["attempt"].clone(),
            id: id.to_owned(),
            filename: source.filename.clone(),
            read_id: uuid::Uuid::new_v4().simple().to_string(),
            hash: Sha256::new(),
            count: 0,
        }))
    }

    async fn read(&self, resource: &str) -> Result<Option<Page>, HostError> {
        self.capability(resource, Capability::Read)?;
        let mut state = self.state.lock().map_err(|_| HostError::unavailable())?;
        Self::active(&state)?;
        let reader = state.readers.get_mut(resource).ok_or_else(HostError::unauthorized)?;
        let original = reader.clone();
        if let Some(end) = reader.pending.take() {
            reader.acknowledged = end;
        }
        let input = &self.inputs.readers[resource];
        let start = reader.acknowledged;
        if start == input.len() {
            reader.drained = true;
            if self.persist(&state).is_err() {
                *state
                    .readers
                    .get_mut(resource)
                    .expect("reader was validated before persistence") = original;
                return Err(HostError::unavailable());
            }
            return Ok(None);
        }
        let mut bytes = 0;
        let mut items = Vec::new();
        for (index, value) in input.iter().enumerate().skip(start).take(MAX_BATCH_ITEMS) {
            let text = value.to_string();
            if bytes + text.len() > MAX_BATCH_BYTES {
                break;
            }
            bytes += text.len();
            items.push(Item {
                seed: format!("local-{resource}-{}", index + 1),
                value: text,
            });
        }
        reader.pending = Some(start + items.len());
        let mut newly_lent = Vec::new();
        let State {
            sources,
            lent,
            ..
        } = &mut *state;
        for value in &input[start..start + items.len()] {
            collect_sources(value, sources, &mut |id| {
                if !lent.contains(id) {
                    lent.insert(id.to_owned());
                    newly_lent.push(id);
                }
            });
        }
        if self.persist(&state).is_err() {
            *state
                .readers
                .get_mut(resource)
                .expect("reader was validated before persistence") = original;
            for id in newly_lent {
                state.lent.remove(id);
            }
            return Err(HostError::unavailable());
        }
        Ok(Some(Page {
            remaining: None,
            items,
        }))
    }

    async fn write(
        &self,
        resource: &str,
        connection: Option<&str>,
        mut items: mpsc::Receiver<String>,
    ) -> Result<(), HostError> {
        self.capability(resource, Capability::Write)?;
        let attempt = {
            let state = self.state.lock().map_err(|_| HostError::unavailable())?;
            Self::active(&state)?;
            state.operation["attempt"].clone()
        };
        let declaration = &self.manifest.resources[resource];
        let key = declaration
            .key
            .as_deref()
            .ok_or_else(|| HostError::invalid("writer has no key"))?;
        let mut staged = Vec::new();
        let mut bytes = 0;
        while let Some(text) = items.recv().await {
            if text.len() > MAX_ITEM_BYTES || staged.len() >= MAX_BATCH_ITEMS || bytes + text.len() > MAX_BATCH_BYTES {
                return Err(HostError::too_large());
            }
            let mut value = parse_object(text.as_bytes()).map_err(|_| HostError::invalid("item is not valid JSON"))?;
            self.redact_value(&mut value)?;
            declaration
                .schema
                .validate_instance(&value)
                .map_err(|_| HostError::invalid("item violates its schema"))?;
            let id = value
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| HostError::invalid("item key is missing"))?
                .to_owned();
            bytes += text.len();
            staged.push((id, value));
        }
        let mut state = self.state.lock().map_err(|_| HostError::unavailable())?;
        Self::active(&state)?;
        if state.operation["attempt"] != attempt {
            return Err(HostError::stopped());
        }
        if connection.is_some_and(|name| {
            !self.manifest.actions[self.action()]
                .connections
                .iter()
                .any(|value| value == name)
        }) {
            return Err(HostError::unauthorized());
        }
        for (_, value) in &staged {
            validate_sources(Some(declaration.schema.as_value()), value, &state.sources)
                .map_err(|_| HostError::invalid("item source violates its declaration"))?;
            validate_item_sources(declaration.schema.as_value(), value, "", resource, &state)?;
        }
        let writer = state.writers.get_mut(resource).ok_or_else(HostError::unauthorized)?;
        if writer.staged.len() + staged.len() > MAX_BATCH_ITEMS || writer.staged_bytes + bytes > MAX_BATCH_BYTES {
            return Err(HostError::too_large());
        }
        writer.staged.extend(staged);
        writer.staged_bytes += bytes;
        Ok(())
    }

    async fn checkpoint(&self, resource: &str, _connection: Option<&str>, cursor: &str) -> Result<(), HostError> {
        self.capability(resource, Capability::Write)?;
        if cursor.len() > MAX_CURSOR_BYTES {
            return Err(HostError::too_large());
        }
        let mut state = self.state.lock().map_err(|_| HostError::unavailable())?;
        Self::active(&state)?;
        if !state.readers.is_empty() && !cursor.is_empty() {
            return Err(HostError::invalid("reader actions have no provider cursor"));
        }
        let writer = state.writers.get_mut(resource).ok_or_else(HostError::unauthorized)?;
        let original_written = writer.written;
        let original_cursor = mem::replace(&mut writer.cursor, cursor.to_owned());
        let staged_bytes = mem::take(&mut writer.staged_bytes);
        let staged = mem::take(&mut writer.staged);
        let mut displaced = Vec::with_capacity(staged.len());
        writer.written += staged.len() as u64;
        for (key, value) in staged {
            let previous = writer.items.remove_entry(&key);
            writer.items.insert(key.clone(), value);
            displaced.push((key, previous));
        }
        if self.persist(&state).is_err() {
            let writer = state
                .writers
                .get_mut(resource)
                .expect("writer was validated before persistence");
            writer.written = original_written;
            writer.cursor = original_cursor;
            writer.staged_bytes = staged_bytes;
            // Reverse application recovers each staged value even when the
            // same key appeared several times in this checkpoint.
            for (key, previous) in displaced.into_iter().rev() {
                let value = writer.items.remove(&key).expect("checkpoint inserted every undo key");
                writer.staged.push((key, value));
                if let Some((key, value)) = previous {
                    writer.items.insert(key, value);
                }
            }
            writer.staged.reverse();
            return Err(HostError::unavailable());
        }
        Ok(())
    }

    async fn source(
        &self,
        resource: &str,
        property: &str,
        filename: &str,
        bytes: Pin<Box<dyn AsyncRead + Send>>,
    ) -> Result<String, HostError> {
        self.capability(resource, Capability::Write)?;
        let attempt = {
            let state = self.state.lock().map_err(|_| HostError::unavailable())?;
            Self::active(&state)?;
            state.operation["attempt"].clone()
        };
        validate_filename(filename).map_err(|_| HostError::invalid("invalid source filename"))?;
        validate_source_property(property)
            .map_err(|_| HostError::invalid("source property must be a direct identifier"))?;
        let schema = self.manifest.resources[resource]
            .schema
            .as_value()
            .get("properties")
            .and_then(|properties| properties.get(property))
            .filter(|schema| schema.get("format").and_then(Value::as_str) == Some("source"))
            .ok_or_else(|| HostError::invalid("property is not a declared source"))?;
        let limit = schema
            .get("maxBytes")
            .and_then(Value::as_u64)
            .unwrap_or(100 * 1024 * 1024)
            .min(100 * 1024 * 1024);
        let mut content = Vec::new();
        bytes
            .take(limit + 1)
            .read_to_end(&mut content)
            .await
            .map_err(|_| HostError::source_unreadable())?;
        if content.len() as u64 > limit {
            return Err(HostError::too_large());
        }
        let source = LocalSource {
            filename: filename.to_owned(),
            media_type: detect_media_type(&content)
                .ok_or_else(|| HostError::invalid("produced source media type cannot be identified"))?,
            bytes: content.into(),
        };
        if !source_allowed(schema, &source) {
            return Err(HostError::invalid("source violates its declaration"));
        }
        let mut state = self.state.lock().map_err(|_| HostError::unavailable())?;
        Self::active(&state)?;
        if state.operation["attempt"] != attempt {
            return Err(HostError::stopped());
        }
        let id = format!("src_{}", uuid::Uuid::new_v4().simple());
        atomic_write(&self.directory.join(&id), &mut |writer| writer.write_all(&source.bytes))
            .map_err(|_| HostError::unavailable())?;
        let previous_source = state.sources.insert(id.clone(), source);
        let previous_binding = state
            .source_bindings
            .insert(id.clone(), (resource.to_owned(), property.to_owned()));
        let produced = state.produced.insert(id.clone());
        if self.persist(&state).is_err() {
            state.sources.remove(&id);
            if let Some(source) = previous_source {
                state.sources.insert(id.clone(), source);
            }
            state.source_bindings.remove(&id);
            if let Some(binding) = previous_binding {
                state.source_bindings.insert(id.clone(), binding);
            }
            if produced {
                state.produced.remove(&id);
            }
            return Err(HostError::unavailable());
        }
        Ok(id)
    }

    fn log(&self, level: LogLevel, message: &str, kind: LogKind) {
        let Ok(mut state) = self.state.lock() else {
            // Logs are disposable diagnostics; an unavailable store drops them.
            return;
        };
        if state.logs_dropped {
            return;
        }
        let mut message = self.log_message(level, message);
        let reserve = MAX_LOG_LINE_BYTES + LOG_ENTRY_BYTES;
        match kind {
            LogKind::DroppedNotice => state.logs_dropped = true,
            LogKind::Message if state.log_bytes + message.len() + LOG_ENTRY_BYTES > MAX_LOG_BYTES - reserve => {
                state.logs_dropped = true;
                message = self.log_message(LogLevel::Warn, LOG_DROPPED_MESSAGE);
            },
            LogKind::Message => {},
        }
        state.log_bytes += message.len() + LOG_ENTRY_BYTES;
        state.logs.push(message);
    }

    async fn finish(&self) -> Result<(), HostError> {
        let mut state = self.state.lock().map_err(|_| HostError::unavailable())?;
        for writer in state.writers.values_mut() {
            writer.staged.clear();
            writer.staged_bytes = 0;
        }
        self.persist(&state).map_err(|_| HostError::unavailable())?;
        Ok(())
    }
}

// Each stream records the bytes actually pulled by the extension host,
// including partial reads. Its captured attempt prevents a delayed old reader
// from observing bytes or mutating provenance after a retry or terminal
// operation.
struct SourceReader {
    bytes: Cursor<Arc<[u8]>>,
    state: Arc<Mutex<State>>,
    path: PathBuf,
    attempt: Value,
    id: String,
    filename: String,
    read_id: String,
    hash: Sha256,
    count: u64,
}

impl AsyncRead for SourceReader {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let owner = Arc::clone(&self.state);
        let Ok(mut state) = owner.lock() else {
            return Poll::Ready(Err(io::Error::other("local source observation lock is poisoned")));
        };
        if state.operation["state"] != "RUNNING" || state.operation["attempt"] != self.attempt {
            return Poll::Ready(Err(io::Error::other(
                "local source read belongs to an inactive attempt",
            )));
        }
        let before = buffer.filled().len();
        match Pin::new(&mut self.bytes).poll_read(cx, buffer) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                let bytes = &buffer.filled()[before..];
                let previous_hash = self.hash.clone();
                let previous_count = self.count;
                self.hash.update(bytes);
                self.count += bytes.len() as u64;
                let hash = Value::String(hex(&self.hash.clone().finalize()));
                let count = Value::String(self.count.to_string());
                let previous = if let Some(read) = state.source_reads.get_mut(&self.read_id) {
                    Some((
                        mem::replace(&mut read["sha256"], hash),
                        mem::replace(&mut read["sizeBytes"], count),
                    ))
                } else {
                    let mut read = json!({"sourceId":self.id,"filename":self.filename});
                    read["sha256"] = hash;
                    read["sizeBytes"] = count;
                    state.source_reads.insert(self.read_id.clone(), read);
                    None
                };
                let result = atomic_write(&self.path, &mut |writer| {
                    serde_json::to_writer(writer, &*state).map_err(io::Error::other)
                });
                if result.is_err() {
                    buffer.set_filled(before);
                    self.bytes.set_position(previous_count);
                    self.hash = previous_hash;
                    self.count = previous_count;
                    if let Some((hash, count)) = previous {
                        let read = state
                            .source_reads
                            .get_mut(&self.read_id)
                            .expect("source observation exists until rollback");
                        read["sha256"] = hash;
                        read["sizeBytes"] = count;
                    } else {
                        state.source_reads.remove(&self.read_id);
                    }
                }
                Poll::Ready(result)
            },
        }
    }
}

fn atomic_write(path: &Path, write: &mut dyn FnMut(&mut dyn io::Write) -> io::Result<()>) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "local store path has no parent"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut writer = BufWriter::new(file.as_file_mut());
        write(&mut writer)?;
        writer.flush()?;
    }
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn write_commit(path: &Path, value: &impl Serialize) -> Result<String, Error> {
    struct HashWriter<'a> {
        writer: &'a mut dyn io::Write,
        hash: &'a mut Sha256,
    }
    impl io::Write for HashWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let written = self.writer.write(bytes)?;
            self.hash.update(&bytes[..written]);
            Ok(written)
        }

        #[inline]
        fn flush(&mut self) -> io::Result<()> {
            self.writer.flush()
        }
    }
    let mut hash = Sha256::new();
    atomic_write(path, &mut |writer| {
        serde_json::to_writer(
            HashWriter {
                writer,
                hash: &mut hash,
            },
            value,
        )
        .map_err(io::Error::other)
    })?;
    Ok(prefixed_digest(&hash.finalize()))
}

fn written_item_keys(writers: &BTreeMap<String, Writer>) -> Result<(Vec<Value>, bool), Error> {
    if writers.values().map(|writer| writer.items.len()).sum::<usize>() > MAX_SUMMARY_KEYS {
        return Ok((Vec::new(), true));
    }
    let keys: Vec<_> = writers
        .iter()
        .flat_map(|(name, writer)| {
            writer
                .items
                .keys()
                .map(move |key| json!({"extensionResourceId":name,"key":key}))
        })
        .collect();
    if serialized_exceeds(&keys, MAX_SUMMARY_KEY_BYTES)? {
        Ok((Vec::new(), true))
    } else {
        Ok((keys, false))
    }
}

fn digest(bytes: &[u8]) -> String {
    prefixed_digest(&Sha256::digest(bytes))
}

fn prefixed_digest(hash: &[u8]) -> String {
    let mut value = String::with_capacity("sha256:".len() + 64);
    value.push_str("sha256:");
    super::super::append_hex(&mut value, hash);
    value
}

fn timestamp() -> Result<String, Error> {
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| Error::invalid_response("local clock cannot be encoded"))
}

fn collect_sources<'a>(value: &'a Value, sources: &BTreeMap<String, LocalSource>, visit: &mut dyn FnMut(&'a str)) {
    match value {
        Value::String(id) if sources.contains_key(id) => visit(id),
        Value::Array(values) => values.iter().for_each(|value| collect_sources(value, sources, visit)),
        Value::Object(values) => values.values().for_each(|value| collect_sources(value, sources, visit)),
        _ => {},
    }
}

fn validate_item_sources(
    schema: &Value,
    value: &Value,
    pointer: &str,
    resource: &str,
    state: &State,
) -> Result<(), HostError> {
    if value.is_null() {
        return Ok(());
    }
    if schema.get("format").and_then(Value::as_str) == Some("source") {
        let id = value
            .as_str()
            .ok_or_else(|| HostError::invalid("source reference must be a string"))?;
        if let Some((owner, property)) = state.source_bindings.get(id) {
            let direct = pointer
                .strip_prefix("/properties/")
                .filter(|property| validate_source_property(property).is_ok());
            if owner != resource || direct != Some(property.as_str()) {
                return Err(HostError::invalid(
                    "source was sealed under a different writer property",
                ));
            }
        } else if !state.lent.contains(id) {
            return Err(HostError::invalid("source was not lent to this attempt"));
        }
    }
    if let Some(values) = value.as_object() {
        for (name, value) in values {
            if let Some(child) = schema.get("properties").and_then(|properties| properties.get(name)) {
                let name = name.replace('~', "~0").replace('/', "~1");
                validate_item_sources(child, value, &format!("{pointer}/properties/{name}"), resource, state)?;
            } else if let Some(child) = schema.get("additionalProperties").filter(|child| child.is_object()) {
                validate_item_sources(
                    child,
                    value,
                    &format!("{pointer}/additionalProperties"),
                    resource,
                    state,
                )?;
            }
        }
    }
    if let (Some(schema), Some(values)) = (schema.get("items"), value.as_array()) {
        for value in values {
            validate_item_sources(schema, value, &format!("{pointer}/items"), resource, state)?;
        }
    }
    Ok(())
}

fn truncate(value: &mut String, bytes: usize) {
    let mut end = value.len().min(bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

fn export_filename(filename: &str) -> &str {
    let stem = filename.split('.').next().unwrap_or_default();
    let reserved = ["CON", "PRN", "AUX", "NUL"]
        .iter()
        .any(|name| stem.eq_ignore_ascii_case(name))
        || (stem.len() == 4
            && (stem.as_bytes()[..3].eq_ignore_ascii_case(b"COM")
                || stem.as_bytes()[..3].eq_ignore_ascii_case(b"LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if cfg!(windows) && (reserved || filename.chars().any(|c| "<>:\"|?*".contains(c)) || filename.ends_with([' ', '.']))
    {
        "source"
    } else {
        filename
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(directory: &Path, items: Vec<Value>) -> LocalHost {
        let mut manifest: Value = serde_json::from_slice(include_bytes!("../../tests/fixtures/valid.json")).unwrap();
        manifest["actions"]["inspect"]["resources"]["invoices"] = json!(["read", "write"]);
        let manifest = Manifest::parse(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        let inputs = Inputs {
            action: "inspect".into(),
            parameters: json!({}),
            configuration: json!({}),
            readers: BTreeMap::from([("invoices".into(), items)]),
            sources: BTreeMap::new(),
            tokens: BTreeMap::new(),
        };
        let host = LocalHost::new(directory, manifest, inputs).unwrap();
        host.admit().unwrap();
        host.start(1).unwrap();
        host
    }

    fn host_with_token(directory: &Path, token: &str) -> LocalHost {
        let mut host = host(directory, Vec::new());
        host.inputs.tokens.insert("ledger".into(), token.into());
        host
    }

    async fn write(host: &LocalHost, items: Vec<Value>) -> Result<(), HostError> {
        let (sender, receiver) = mpsc::channel(items.len().max(1));
        for item in items {
            sender.send(item.to_string()).await.unwrap();
        }
        drop(sender);
        host.write("invoices", None, receiver).await
    }

    mod logs {
        use sloper_extension_host::{
            Host as _,
            LogKind,
            LogLevel,
        };

        use super::{
            super::{
                LOG_DROPPED_MESSAGE,
                LOG_ENTRY_BYTES,
                MAX_LOG_BYTES,
                MAX_LOG_LINE_BYTES,
            },
            host,
            host_with_token,
        };

        #[test]
        fn ordinary_notice_text_does_not_end_admission_but_explicit_notice_does() {
            let directory = tempfile::tempdir().unwrap();
            let host = host(directory.path(), Vec::new());
            host.log(LogLevel::Warn, LOG_DROPPED_MESSAGE, LogKind::Message);
            host.log(LogLevel::Info, "still admitted", LogKind::Message);
            host.log(LogLevel::Warn, LOG_DROPPED_MESSAGE, LogKind::DroppedNotice);
            host.log(LogLevel::Info, "must be dropped", LogKind::Message);
            host.log(LogLevel::Warn, LOG_DROPPED_MESSAGE, LogKind::DroppedNotice);

            let state = host.state.lock().unwrap();
            assert_eq!(
                state.logs,
                [
                    format!("Level::Warn: {LOG_DROPPED_MESSAGE}"),
                    "Level::Info: still admitted".into(),
                    format!("Level::Warn: {LOG_DROPPED_MESSAGE}"),
                ]
            );
            assert!(state.logs_dropped, "stored rows={}", state.logs.len());
            assert_eq!(
                state.log_bytes,
                state
                    .logs
                    .iter()
                    .map(|line| line.len() + LOG_ENTRY_BYTES)
                    .sum::<usize>()
            );
        }

        #[test]
        fn full_formatted_line_redacts_credentials_across_the_level_prefix() {
            let directory = tempfile::tempdir().unwrap();
            let host = host_with_token(directory.path(), "Info: secret");
            host.log(LogLevel::Info, "secret value", LogKind::Message);

            let state = host.state.lock().unwrap();
            assert_eq!(state.logs, ["Level::[redacted] value"]);
            assert_eq!(state.log_bytes, "Level::[redacted] value".len() + LOG_ENTRY_BYTES);
        }

        #[test]
        fn full_formatted_lines_are_truncated_on_a_utf8_boundary_and_charged_afterward() {
            let directory = tempfile::tempdir().unwrap();
            let host = host(directory.path(), Vec::new());
            host.log(LogLevel::Info, &"é".repeat(MAX_LOG_LINE_BYTES), LogKind::Message);
            host.log(LogLevel::Info, "", LogKind::Message);

            let state = host.state.lock().unwrap();
            assert_eq!(
                state.logs[0],
                format!(
                    "Level::Info: {}",
                    "é".repeat((MAX_LOG_LINE_BYTES - "Level::Info: ".len()) / 2)
                )
            );
            assert_eq!(state.logs[0].len(), MAX_LOG_LINE_BYTES - 1);
            assert_eq!(state.logs[1], "Level::Info: ");
            assert_eq!(
                state.log_bytes,
                MAX_LOG_LINE_BYTES - 1 + "Level::Info: ".len() + 2 * LOG_ENTRY_BYTES
            );
        }

        /// Fully redacted rows still consume the per-entry metadata budget.
        #[test]
        fn fully_redacted_empty_rows_exhaust_the_metadata_budget() {
            let directory = tempfile::tempdir().unwrap();
            let host = host_with_token(directory.path(), "a");
            let ordinary_rows = (MAX_LOG_BYTES - MAX_LOG_LINE_BYTES - LOG_ENTRY_BYTES) / LOG_ENTRY_BYTES;
            for _ in 0..ordinary_rows + 2 {
                host.log(LogLevel::Info, "a", LogKind::Message);
            }
            host.log(LogLevel::Info, "", LogKind::Message);
            host.log(LogLevel::Warn, "a", LogKind::DroppedNotice);

            let state = host.state.lock().unwrap();
            assert_eq!(state.logs, vec![String::new(); ordinary_rows + 1]);
            assert_eq!(state.log_bytes, (ordinary_rows + 1) * LOG_ENTRY_BYTES);
            assert!(state.logs_dropped, "stored rows={}", state.logs.len());
        }

        /// Each replacement marker contains two `e` characters. Replacing a
        /// repeated token 64 times would expand exponentially before a
        /// final redaction check.
        #[test]
        fn repeated_marker_tokens_are_omitted_before_replacement_expansion() {
            let directory = tempfile::tempdir().unwrap();
            let mut host = host(directory.path(), Vec::new());
            host.inputs.tokens = (0..64)
                .map(|index| (format!("connection-{index}"), "e".into()))
                .collect();
            host.log(LogLevel::Info, "e", LogKind::Message);
            host.log(LogLevel::Warn, LOG_DROPPED_MESSAGE, LogKind::DroppedNotice);

            let state = host.state.lock().unwrap();
            assert_eq!(state.logs, ["", ""]);
            assert_eq!(state.log_bytes, 2 * LOG_ENTRY_BYTES);
        }

        #[test]
        fn credentials_reconstructed_across_replacement_boundaries_are_omitted() {
            let directory = tempfile::tempdir().unwrap();
            let mut host = host_with_token(directory.path(), "s[redacted]");
            host.inputs.tokens.insert("replacement".into(), "X".into());
            host.log(LogLevel::Info, "sX", LogKind::Message);

            let state = host.state.lock().unwrap();
            assert_eq!(state.logs, [""]);
            assert_eq!(state.log_bytes, LOG_ENTRY_BYTES);
        }

        #[test]
        fn full_notice_reservation_accounts_for_the_final_utf8_and_metadata() {
            let directory = tempfile::tempdir().unwrap();
            let host = host(directory.path(), Vec::new());
            let row_charge = MAX_LOG_LINE_BYTES + LOG_ENTRY_BYTES;
            let ordinary_budget = MAX_LOG_BYTES - row_charge;
            let full_rows = ordinary_budget / row_charge;
            for _ in 0..full_rows {
                host.log(LogLevel::Info, &"x".repeat(MAX_LOG_LINE_BYTES), LogKind::Message);
            }
            let remaining_charge = ordinary_budget % row_charge;
            host.log(
                LogLevel::Info,
                &"x".repeat(remaining_charge - LOG_ENTRY_BYTES - "Level::Info: ".len()),
                LogKind::Message,
            );
            {
                let state = host.state.lock().unwrap();
                assert_eq!(state.log_bytes, ordinary_budget);
                assert!(!state.logs_dropped, "charged bytes={}", state.log_bytes);
            }

            host.log(LogLevel::Warn, &"n".repeat(MAX_LOG_LINE_BYTES), LogKind::DroppedNotice);
            host.log(LogLevel::Info, "", LogKind::Message);
            let state = host.state.lock().unwrap();
            assert_eq!(state.logs.len(), full_rows + 2);
            assert_eq!(
                state.logs.last().unwrap(),
                &format!(
                    "Level::Warn: {}",
                    "n".repeat(MAX_LOG_LINE_BYTES - "Level::Warn: ".len())
                )
            );
            assert_eq!(state.log_bytes, MAX_LOG_BYTES);
            assert_eq!(
                state.log_bytes,
                state
                    .logs
                    .iter()
                    .map(|line| line.len() + LOG_ENTRY_BYTES)
                    .sum::<usize>()
            );
        }

        #[test]
        fn local_overflow_redacts_the_notice_and_permanently_drops_smaller_rows() {
            let directory = tempfile::tempdir().unwrap();
            let host = host_with_token(directory.path(), "attempt logs");
            let full_rows =
                (MAX_LOG_BYTES - MAX_LOG_LINE_BYTES - LOG_ENTRY_BYTES) / (MAX_LOG_LINE_BYTES + LOG_ENTRY_BYTES);
            for _ in 0..=full_rows {
                host.log(LogLevel::Info, &"x".repeat(MAX_LOG_LINE_BYTES), LogKind::Message);
            }
            host.log(LogLevel::Info, "", LogKind::Message);
            host.log(LogLevel::Warn, LOG_DROPPED_MESSAGE, LogKind::DroppedNotice);

            let state = host.state.lock().unwrap();
            assert_eq!(state.logs.len(), full_rows + 1);
            assert_eq!(
                state.logs.last().unwrap(),
                "Level::Warn: Further [redacted] were dropped after the 1 MiB limit."
            );
            assert!(state.logs_dropped, "stored rows={}", state.logs.len());
            assert!(state.log_bytes <= MAX_LOG_BYTES, "charged bytes={}", state.log_bytes);
            assert_eq!(
                state.log_bytes,
                state
                    .logs
                    .iter()
                    .map(|line| line.len() + LOG_ENTRY_BYTES)
                    .sum::<usize>()
            );
        }

        #[test]
        fn host_notice_redacts_its_text_and_prefix_before_ending_admission() {
            let directory = tempfile::tempdir().unwrap();
            let host = host_with_token(directory.path(), "rn: Further attempt logs");
            host.log(LogLevel::Warn, LOG_DROPPED_MESSAGE, LogKind::DroppedNotice);
            host.log(LogLevel::Info, "must be dropped", LogKind::Message);

            let state = host.state.lock().unwrap();
            assert_eq!(state.logs, ["Level::Wa[redacted] were dropped after the 1 MiB limit."]);
            assert_eq!(state.log_bytes, state.logs[0].len() + LOG_ENTRY_BYTES);
            assert!(state.logs_dropped, "stored rows={}", state.logs.len());
        }
    }
    mod redaction {
        use std::fs;

        use serde_json::{
            Value,
            json,
        };
        use sloper_extension_host::{
            Host as _,
            HostError,
        };

        use super::{
            super::Error,
            host_with_token,
            write,
        };

        #[test]
        fn failure_property_names_and_nested_values_are_redacted_before_persistence() {
            let directory = tempfile::tempdir().unwrap();
            let host = host_with_token(directory.path(), "secret-key");
            host.complete(Some(json!({
                "code":"EXTENSION_FAILED",
                "message":"stopped",
                "secret-key":{"nested":[{"secret-key":"secret-key"}]}
            })))
            .unwrap();

            let persisted = fs::read_to_string(directory.path().join("working.json")).unwrap();
            assert!(!persisted.contains("secret-key"), "persisted state={persisted}");
            let state: Value = serde_json::from_str(&persisted).unwrap();
            assert_eq!(state["operation"]["state"], "FAILED");
            assert_eq!(
                state["operation"]["failure"],
                json!({
                    "code":"EXTENSION_FAILED",
                    "message":"stopped",
                    "[redacted]":{"nested":[{"[redacted]":"[redacted]"}]}
                })
            );
        }

        #[test]
        fn nested_property_collisions_clear_the_rejected_value() {
            let directory = tempfile::tempdir().unwrap();
            let host = host_with_token(directory.path(), "secret-key");
            let mut value = json!({"details":[{"secret-key":1,"[redacted]":2}],"other":"secret-key"});
            let error = host.redact_value(&mut value).unwrap_err();

            assert!(matches!(error, HostError::Invalid(_)), "error={error:?}");
            assert_eq!(value, Value::Null);
        }

        #[tokio::test]
        async fn failure_property_collision_preserves_the_running_operation_and_staging() {
            let directory = tempfile::tempdir().unwrap();
            let host = host_with_token(directory.path(), "secret-key");
            write(&host, vec![json!({"id":"pending","number":"A"})]).await.unwrap();
            let before = fs::read(directory.path().join("working.json")).unwrap();
            let error = host
                .complete(Some(json!({
                    "code":"EXTENSION_FAILED",
                    "message":"stopped",
                    "details":{"secret-key":1,"[redacted]":2}
                })))
                .unwrap_err();

            assert!(matches!(error, Error::Host(HostError::Invalid(_))), "error={error:?}");
            assert_eq!(fs::read(directory.path().join("working.json")).unwrap(), before);
            let state = host.state.lock().unwrap();
            assert_eq!(state.operation["state"], "RUNNING");
            assert!(
                state.operation.get("endTime").is_none(),
                "operation={}",
                state.operation
            );
            assert_eq!(state.writers["invoices"].staged.len(), 1);
            assert_eq!(state.writers["invoices"].staged[0].0, "pending");
        }

        #[tokio::test]
        async fn writer_property_credentials_cannot_bypass_schema_validation() {
            let directory = tempfile::tempdir().unwrap();
            let host = host_with_token(directory.path(), "number");
            let error = write(&host, vec![json!({"id":"rejected","number":"A"})])
                .await
                .unwrap_err();

            assert!(matches!(error, HostError::Invalid(_)), "error={error:?}");
            assert_eq!(host.state.lock().unwrap().writers["invoices"].staged.len(), 0);
            let persisted = fs::read_to_string(directory.path().join("working.json")).unwrap();
            assert!(!persisted.contains("number"), "persisted state={persisted}");
        }

        #[tokio::test]
        async fn writer_property_collision_rejects_the_batch_and_preserves_valid_jsonl() {
            let directory = tempfile::tempdir().unwrap();
            let host = host_with_token(directory.path(), "secret-key");
            write(&host, vec![json!({"id":"kept","number":"A"})]).await.unwrap();
            let error = write(
                &host,
                vec![
                    json!({"id":"prefix","number":"B"}),
                    json!({"id":"collision","number":"C","secret-key":1,"[redacted]":2}),
                ],
            )
            .await
            .unwrap_err();
            assert!(matches!(error, HostError::Invalid(_)), "error={error:?}");

            host.checkpoint("invoices", None, "").await.unwrap();
            host.complete(None).unwrap();
            let out = directory.path().join("export");
            host.export(&out).unwrap();
            assert_eq!(
                fs::read_to_string(out.join("invoices.jsonl")).unwrap(),
                "{\"id\":\"kept\",\"number\":\"A\"}\n"
            );
            let persisted = fs::read_to_string(directory.path().join("working.json")).unwrap();
            assert!(!persisted.contains("secret-key"), "persisted state={persisted}");
        }
    }
    mod source {
        //! Disposable execution uses the same direct writer-source capability
        //! spec.

        use super::*;

        #[tokio::test]
        async fn source_writer_rejects_paths_and_undeclared_or_non_source_properties() {
            let directory = tempfile::tempdir().unwrap();
            let host = host(directory.path(), Vec::new());
            for property in [
                "",
                "/properties/document",
                "/properties/nested/properties/document",
                "nested.document",
                "1document",
                "missing",
                "id",
                &"x".repeat(65),
            ] {
                assert!(
                    host.source(
                        "invoices",
                        property,
                        "report.pdf",
                        Box::pin(Cursor::new(b"%PDF-1.7".to_vec()))
                    )
                    .await
                    .is_err(),
                    "{property}"
                );
            }
            assert!(host.state.lock().unwrap().produced.is_empty());
            let source = host
                .source(
                    "invoices",
                    "document",
                    "report.pdf",
                    Box::pin(Cursor::new(b"%PDF-1.7".to_vec())),
                )
                .await
                .unwrap();
            write(&host, vec![json!({"id":"one","number":"N","document":source})])
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn opening_own_output_does_not_lend_it_to_other_properties_or_writers() {
            let directory = tempfile::tempdir().unwrap();
            let host = host(directory.path(), Vec::new());
            let source = host
                .source(
                    "invoices",
                    "document",
                    "report.pdf",
                    Box::pin(Cursor::new(b"%PDF-1.7".to_vec())),
                )
                .await
                .unwrap();
            let mut bytes = Vec::new();
            host.open_source(&source)
                .await
                .unwrap()
                .read_to_end(&mut bytes)
                .await
                .unwrap();
            assert_eq!(bytes, b"%PDF-1.7");
            let state = host.state.lock().unwrap();
            assert!(!state.lent.contains(&source));
            let schema = json!({"type":"string","format":"source"});
            let value = json!(source);
            assert!(validate_item_sources(&schema, &value, "/properties/document", "invoices", &state).is_ok());
            for (resource, pointer) in [
                ("invoices", "/properties/other"),
                ("other", "/properties/document"),
                ("invoices", "/properties/nested/properties/document"),
                ("invoices", "/properties/documents/items"),
            ] {
                assert!(validate_item_sources(&schema, &value, pointer, resource, &state).is_err());
            }
        }

        #[test]
        fn already_lent_sources_remain_authorized_in_nested_fields() {
            let directory = tempfile::tempdir().unwrap();
            let host = host(directory.path(), Vec::new());
            let mut state = host.state.lock().unwrap();
            state.lent.insert("lent".into());
            let schema = json!({"type":"object","properties":{"documents":{"type":"array","items":{"type":"string","format":"source"}}}});
            assert!(validate_item_sources(&schema, &json!({"documents":["lent"]}), "", "invoices", &state).is_ok());
            assert!(validate_item_sources(&schema, &json!({"documents":["unlent"]}), "", "invoices", &state).is_err());
            let map = json!({"type":"object","additionalProperties":{"type":"string","format":"source"}});
            assert!(validate_item_sources(&map, &json!({"any":"lent"}), "", "invoices", &state).is_ok());
            assert!(validate_item_sources(&map, &json!({"any":"unlent"}), "", "invoices", &state).is_err());
        }
    }

    #[tokio::test]
    async fn reader_acknowledges_only_its_successor_and_retries_the_pending_page() {
        let directory = tempfile::tempdir().unwrap();
        let host = host(
            directory.path(),
            (0..1001).map(|id| json!({"id":id.to_string(),"number":"N"})).collect(),
        );
        let page = host.read("invoices").await.unwrap().unwrap();
        assert_eq!(page.items.len(), 1000);
        assert_eq!(page.items[0].seed, "local-invoices-1");
        assert_eq!(host.state.lock().unwrap().readers["invoices"].acknowledged, 0);
        host.retry(Duration::ZERO).unwrap();
        host.start(2).unwrap();
        let replay = host.read("invoices").await.unwrap().unwrap();
        assert_eq!(replay.items[0].seed, page.items[0].seed);
        assert_eq!(replay.items[0].value, page.items[0].value);
        let tail = host.read("invoices").await.unwrap().unwrap();
        assert_eq!(tail.items.len(), 1);
        assert_eq!(host.state.lock().unwrap().readers["invoices"].acknowledged, 1000);
        assert!(host.read("invoices").await.unwrap().is_none());
        assert_eq!(host.state.lock().unwrap().readers["invoices"].acknowledged, 1001);
    }

    #[tokio::test]
    async fn source_provenance_counts_actual_partial_reads_and_fences_old_attempts() {
        let directory = tempfile::tempdir().unwrap();
        let host = host(directory.path(), Vec::new());
        let id = "src_fixture";
        {
            let mut state = host.state.lock().unwrap();
            state.sources.insert(
                id.into(),
                LocalSource {
                    filename: "file.pdf".into(),
                    media_type: "application/pdf",
                    bytes: Arc::from(b"%PDF-1.7\nfixture".as_slice()),
                },
            );
            state.lent.insert(id.into());
        }
        let mut reader = host.open_source(id).await.unwrap();
        assert!(host.state.lock().unwrap().source_reads.is_empty());
        let mut prefix = [0_u8; 4];
        reader.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"%PDF");
        {
            let state = host.state.lock().unwrap();
            let read = state.source_reads.values().next().unwrap();
            assert_eq!(read["sizeBytes"], "4");
            assert_eq!(read["sha256"], digest(&prefix).trim_start_matches("sha256:"));
        }
        host.retry(Duration::ZERO).unwrap();
        host.start(2).unwrap();
        assert!(reader.read_exact(&mut prefix).await.is_err());
        host.complete(None).unwrap();
        assert_eq!(host.state.lock().unwrap().operation["sourceCount"], "1");
    }

    #[tokio::test]
    async fn invalid_write_preserves_prior_staging_and_failure_exports_only_checkpoints() {
        let directory = tempfile::tempdir().unwrap();
        let host = host(directory.path(), Vec::new());
        write(&host, vec![json!({"id":"kept","number":"A"})]).await.unwrap();
        assert!(
            write(
                &host,
                vec![json!({"id":"valid-prefix","number":"B"}), json!({"id":"invalid"})]
            )
            .await
            .is_err()
        );
        host.checkpoint("invoices", None, "").await.unwrap();
        write(&host, vec![json!({"id":"uncheckpointed","number":"C"})])
            .await
            .unwrap();
        host.complete(Some(json!({"code":"EXTENSION_FAILED","message":"stopped"})))
            .unwrap();
        let out = directory.path().join("export");
        let result = host.export(&out).unwrap();
        assert_eq!(result["operation"]["state"], "FAILED");
        assert!(result["operation"].get("resultCommitId").is_none());
        assert_eq!(result["resources"][0]["items"], "1");
        assert_eq!(
            fs::read_to_string(out.join("invoices.jsonl")).unwrap(),
            "{\"id\":\"kept\",\"number\":\"A\"}\n"
        );
        assert_eq!(result["operation"]["resources"][0]["written"], "1");
    }

    #[tokio::test]
    async fn success_has_one_result_commit_and_sealed_sources_survive_failure() {
        let directory = tempfile::tempdir().unwrap();
        let host = host(directory.path(), Vec::new());
        let source = host
            .source(
                "invoices",
                "document",
                "invoice.pdf",
                Box::pin(Cursor::new(b"%PDF-1.7".to_vec())),
            )
            .await
            .unwrap();
        write(&host, vec![json!({"id":"one","number":"A","document":source})])
            .await
            .unwrap();
        host.checkpoint("invoices", None, "").await.unwrap();
        host.complete(None).unwrap();
        let out = directory.path().join("export");
        let result = host.export(&out).unwrap();
        assert_eq!(result["operation"]["state"], "SUCCEEDED");
        assert!(
            result["operation"]["resultCommitId"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert_ne!(
            result["operation"]["inputCommitId"],
            result["operation"]["resultCommitId"]
        );
        assert_eq!(result["sources"].as_array().unwrap().len(), 1);
        assert_eq!(
            fs::read(result["sources"][0]["path"].as_str().unwrap()).unwrap(),
            b"%PDF-1.7"
        );
        assert!(host.checkpoint("invoices", None, "").await.is_err());
    }

    #[tokio::test]
    async fn real_component_runs_with_a_fresh_guest_and_disposable_result_commit() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("dist")).unwrap();
        fs::write(
            directory.path().join("dist/extension.wasm"),
            include_bytes!("../../tests/fixtures/host-guest.wasm"),
        )
        .unwrap();
        let mut ids = BTreeSet::new();
        for index in 0..2 {
            let arguments = json!({"action":"counter","directory":directory.path(),"out":directory.path().join(format!("run-{index}"))});
            let result = super::super::run(arguments.as_object().unwrap().clone()).await.unwrap();
            assert_eq!(result["operation"]["state"], "SUCCEEDED");
            assert_ne!(
                result["operation"]["inputCommitId"],
                result["operation"]["resultCommitId"]
            );
            assert!(ids.insert(result["operation"]["id"].as_str().unwrap().to_owned()));
        }
    }
    #[tokio::test]
    async fn component_without_sdk_declarations_runs() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("dist")).unwrap();
        let mut component = include_bytes!("../../tests/fixtures/host-guest.wasm").to_vec();
        // Renaming the inert declaration custom sections preserves the guest's
        // runtime spec while making it independent of the Rust SDK.
        let declaration = b"sloper:parts";
        let mut declarations = 0;
        for offset in 0..=component.len() - declaration.len() {
            if &component[offset..offset + declaration.len()] == declaration {
                component[offset..offset + declaration.len()].copy_from_slice(b"sloper:other");
                declarations += 1;
            }
        }
        assert!(declarations > 0, "fixture must contain SDK declarations");
        fs::write(directory.path().join("dist/extension.wasm"), component).unwrap();
        let result = crate::run(crate::RunOptions {
            directory: directory.path().to_owned(),
            action: "counter".to_owned(),
            out: directory.path().join("run"),
            parameters: json!({}),
            configuration: json!({}),
            sources: BTreeMap::new(),
            resources: BTreeMap::new(),
        })
        .await
        .unwrap();
        assert_eq!(result.operation["state"], "SUCCEEDED");
        assert!(result.out.is_dir());
    }

    #[tokio::test]
    async fn checkpoint_failure_restores_repeated_keys_staging_and_cursor() {
        let directory = tempfile::tempdir().unwrap();
        let host = host(directory.path(), Vec::new());
        write(&host, vec![json!({"id":"same","number":"original"})])
            .await
            .unwrap();
        host.checkpoint("invoices", None, "").await.unwrap();
        let batch = vec![
            json!({"id":"same","number":"first"}),
            json!({"id":"new","number":"second"}),
            json!({"id":"same","number":"last"}),
        ];
        write(&host, batch.clone()).await.unwrap();
        let original_bytes = host.state.lock().unwrap().writers["invoices"].staged_bytes;
        let working = directory.path().join("working.json");
        fs::remove_file(&working).unwrap();
        fs::create_dir(&working).unwrap();
        assert!(host.checkpoint("invoices", None, "").await.is_err());
        {
            let state = host.state.lock().unwrap();
            let writer = &state.writers["invoices"];
            assert_eq!(writer.written, 1);
            assert_eq!(writer.items.len(), 1);
            assert_eq!(writer.items["same"]["number"], "original");
            assert_eq!(
                writer.staged.iter().map(|(_, value)| value).collect::<Vec<_>>(),
                batch.iter().collect::<Vec<_>>()
            );
            assert_eq!(writer.staged_bytes, original_bytes);
            assert_eq!(writer.cursor, "");
        }
        fs::remove_dir(&working).unwrap();
        host.checkpoint("invoices", None, "").await.unwrap();
        let state = host.state.lock().unwrap();
        assert_eq!(state.writers["invoices"].written, 4);
        assert_eq!(state.writers["invoices"].items["same"]["number"], "last");
        assert_eq!(state.writers["invoices"].items.len(), 2);
    }

    #[tokio::test]
    async fn reader_failure_restores_lending_pending_page_and_drained_state() {
        let directory = tempfile::tempdir().unwrap();
        let host = host(
            directory.path(),
            vec![json!({"id":"one","number":"A","document":"fixture"})],
        );
        host.state.lock().unwrap().sources.insert(
            "fixture".into(),
            LocalSource {
                filename: "fixture.pdf".into(),
                media_type: "application/pdf",
                bytes: Arc::from(b"%PDF-1.7".as_slice()),
            },
        );
        let working = directory.path().join("working.json");
        fs::remove_file(&working).unwrap();
        fs::create_dir(&working).unwrap();
        assert!(host.read("invoices").await.is_err());
        {
            let state = host.state.lock().unwrap();
            assert!(state.lent.is_empty());
            assert_eq!(state.readers["invoices"].pending, None);
            assert_eq!(state.readers["invoices"].acknowledged, 0);
        }
        fs::remove_dir(&working).unwrap();
        assert!(host.read("invoices").await.unwrap().is_some());
        fs::remove_file(&working).unwrap();
        fs::create_dir(&working).unwrap();
        assert!(host.read("invoices").await.is_err());
        {
            let state = host.state.lock().unwrap();
            assert!(state.lent.contains("fixture"));
            assert_eq!(state.readers["invoices"].pending, Some(1));
            assert_eq!(state.readers["invoices"].acknowledged, 0);
            assert!(!state.readers["invoices"].drained);
        }
        fs::remove_dir(&working).unwrap();
        assert!(host.read("invoices").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn source_creation_failure_restores_only_its_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let host = host(directory.path(), Vec::new());
        let working = directory.path().join("working.json");
        fs::remove_file(&working).unwrap();
        fs::create_dir(&working).unwrap();
        assert!(
            host.source("invoices", "document", "report.pdf", Box::pin(Cursor::new(b"%PDF-1.7")))
                .await
                .is_err()
        );
        {
            let state = host.state.lock().unwrap();
            assert!(state.sources.is_empty());
            assert!(state.source_bindings.is_empty());
            assert!(state.produced.is_empty());
        }
        fs::remove_dir(&working).unwrap();
        let id = host
            .source("invoices", "document", "report.pdf", Box::pin(Cursor::new(b"%PDF-1.7")))
            .await
            .unwrap();
        assert!(host.state.lock().unwrap().produced.contains(&id));
    }

    #[tokio::test]
    async fn source_poll_failure_keeps_bytes_retryable_and_preserves_prior_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let host = host(directory.path(), Vec::new());
        let id = host
            .source("invoices", "document", "report.pdf", Box::pin(Cursor::new(b"%PDF-1.7")))
            .await
            .unwrap();
        let mut reader = host.open_source(&id).await.unwrap();
        let working = directory.path().join("working.json");
        fs::remove_file(&working).unwrap();
        fs::create_dir(&working).unwrap();
        let mut prefix = [0; 4];
        assert!(reader.read_exact(&mut prefix).await.is_err());
        assert!(host.state.lock().unwrap().source_reads.is_empty());
        fs::remove_dir(&working).unwrap();
        reader.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"%PDF");
        let previous = host.state.lock().unwrap().source_reads.clone();
        fs::remove_file(&working).unwrap();
        fs::create_dir(&working).unwrap();
        assert!(reader.read_exact(&mut prefix).await.is_err());
        assert_eq!(host.state.lock().unwrap().source_reads, previous);
        fs::remove_dir(&working).unwrap();
        reader.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"-1.7");
        let state = host.state.lock().unwrap();
        assert_eq!(state.source_reads.values().next().unwrap()["sizeBytes"], "8");
        assert_eq!(
            state.source_reads.values().next().unwrap()["sha256"],
            hex(&Sha256::digest(b"%PDF-1.7"))
        );
    }

    #[tokio::test]
    async fn canonical_source_owner_is_shared_by_independent_readers() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = Manifest::parse(include_bytes!("../../tests/fixtures/valid.json")).unwrap();
        let bytes: Arc<[u8]> = Arc::from(b"%PDF-1.7".as_slice());
        let inputs = Inputs {
            action: "inspect".into(),
            parameters: json!({"nested":["fixture","fixture"]}),
            configuration: json!({}),
            readers: BTreeMap::from([("invoices".into(), Vec::new())]),
            sources: BTreeMap::from([(
                "fixture".into(),
                LocalSource {
                    filename: "fixture.pdf".into(),
                    media_type: "application/pdf",
                    bytes: Arc::clone(&bytes),
                },
            )]),
            tokens: BTreeMap::new(),
        };
        let host = LocalHost::new(directory.path(), manifest, inputs).unwrap();
        assert_eq!(Arc::strong_count(&bytes), 2);
        assert!(host.inputs.sources.is_empty());
        assert_eq!(host.parameter_sources().unwrap().len(), 1);
        host.admit().unwrap();
        host.start(1).unwrap();
        let mut first = host.open_source("fixture").await.unwrap();
        let mut second = host.open_source("fixture").await.unwrap();
        assert_eq!(Arc::strong_count(&bytes), 4);
        let mut prefix = [0; 4];
        first.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"%PDF");
        second.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"%PDF");
        first.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"-1.7");
    }

    #[tokio::test]
    async fn streamed_snapshots_preserve_canonical_bytes_and_commit_digests() {
        let directory = tempfile::tempdir().unwrap();
        let host = host(directory.path(), vec![json!({"id":"é","number":"quoted\""})]);
        let initial = json!({"resources":host.inputs.readers,"parameters":host.inputs.parameters,"configuration":host.inputs.configuration,"sources":{}});
        let bytes = serde_json::to_vec(&initial).unwrap();
        assert_eq!(fs::read(directory.path().join("input.json")).unwrap(), bytes);
        assert_eq!(host.input_commit, digest(&bytes));
        write(&host, vec![json!({"id":"one","number":"A"})]).await.unwrap();
        host.checkpoint("invoices", None, "").await.unwrap();
        let expected = {
            let state = host.state.lock().unwrap();
            serde_json::to_vec(
                &json!({"parent":host.input_commit,"operation":host.operation,"resources":state.writers,"sources":{}}),
            )
            .unwrap()
        };
        host.complete(None).unwrap();
        assert_eq!(fs::read(directory.path().join("result.json")).unwrap(), expected);
        let state = host.state.lock().unwrap();
        assert_eq!(state.operation["resultCommitId"], digest(&expected));
        assert_eq!(
            fs::read(directory.path().join("working.json")).unwrap(),
            serde_json::to_vec(&*state).unwrap()
        );
        assert_eq!(
            digest(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn written_key_summary_enforces_item_and_encoded_byte_limits() {
        let mut writers = BTreeMap::from([("invoices".into(), Writer::default())]);
        for index in 0..100 {
            writers
                .get_mut("invoices")
                .unwrap()
                .items
                .insert(index.to_string(), Value::Null);
        }
        assert_eq!(written_item_keys(&writers).unwrap().0.len(), 100);
        writers
            .get_mut("invoices")
            .unwrap()
            .items
            .insert("overflow".into(), Value::Null);
        assert_eq!(written_item_keys(&writers).unwrap(), (Vec::new(), true));
        let writer = writers.get_mut("invoices").unwrap();
        writer.items.clear();
        let overhead = serde_json::to_vec(&json!([{"extensionResourceId":"invoices","key":""}]))
            .unwrap()
            .len();
        writer
            .items
            .insert("x".repeat(MAX_SUMMARY_KEY_BYTES - overhead), Value::Null);
        assert!(!written_item_keys(&writers).unwrap().1);
        let writer = writers.get_mut("invoices").unwrap();
        writer.items.clear();
        writer
            .items
            .insert("x".repeat(MAX_SUMMARY_KEY_BYTES - overhead + 1), Value::Null);
        assert_eq!(written_item_keys(&writers).unwrap(), (Vec::new(), true));
    }

    #[test]
    fn truncation_keeps_owned_buffer_and_filename_export_borrows_safe_names() {
        let mut message = "ééé".to_owned();
        let allocation = message.as_ptr();
        truncate(&mut message, 5);
        assert_eq!(message, "éé");
        assert_eq!(message.as_ptr(), allocation);
        let filename = "invoice.pdf".to_owned();
        assert_eq!(export_filename(&filename).as_ptr(), filename.as_ptr());
        for filename in ["cOn.txt", "com1.pdf", "Lpt9", "tail.", "tail ", "bad:name"] {
            assert_eq!(
                export_filename(filename),
                if cfg!(windows) {
                    "source"
                } else {
                    filename
                }
            );
        }
        assert_eq!(export_filename("cöm1.pdf"), "cöm1.pdf");
    }
    #[tokio::test]
    async fn provider_cursors_remain_sorted_across_retries_and_absent_for_readers() {
        let directory = tempfile::tempdir().unwrap();
        let host = host(directory.path(), Vec::new());
        assert!(host.cursors().unwrap().is_empty());
        {
            let mut state = host.state.lock().unwrap();
            state.readers.clear();
            state.writers.insert("alpha".into(), Writer::default());
        }
        host.checkpoint("invoices", None, "checkpoint").await.unwrap();
        host.retry(Duration::ZERO).unwrap();
        host.start(2).unwrap();
        let cursors = host.cursors().unwrap();
        assert_eq!(
            cursors.iter().map(|cursor| cursor.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "invoices"]
        );
        assert_eq!(cursors[1].value, "checkpoint");
    }
}
