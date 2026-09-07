mod inputs;
mod store;

use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};

use inputs::Inputs;
use serde_json::{
    Map,
    Value,
    json,
};
use sloper_extension_host::{
    Engine,
    Failure,
    Request,
    validate_component,
};
use sloper_extension_spec::{
    Capability,
    Manifest,
    Mode,
};
use store::LocalHost;
use tokio::{
    sync::watch,
    time::sleep,
};

use super::{
    Error,
    read_component,
    text,
};

pub(crate) async fn run(mut arguments: Map<String, Value>) -> Result<Value, Error> {
    let out = Path::new(text(&arguments, "out")?).to_owned();
    if out.try_exists()? {
        return Err(Error::usage("local run output directory already exists"));
    }
    let component = read_component(&Path::new(text(&arguments, "directory")?).join("dist/extension.wasm"))?;
    let manifest = validate_component(&component)?;
    let inputs = Inputs::parse(&mut arguments, &manifest)?;
    let directory = tempfile::tempdir()?;
    let host = Arc::new(LocalHost::new(directory.path(), manifest, inputs)?);
    let engine = Engine::new()?;
    engine.admit_component(&component).await?;
    host.admit()?;
    let started = Instant::now();
    let budget = Duration::from_hours(24);
    let background = host.manifest().actions[host.action()].mode == Mode::Background;
    let (_stop_sender, stopping) = watch::channel(None);
    let (_deadline_sender, deadline) = watch::channel(None);
    for attempt in 1..=5 {
        host.start(attempt)?;
        let request = Request {
            operation: host.operation_id().to_owned(),
            action: host.action().to_owned(),
            parameters: serde_json::to_string(host.parameters())?,
            configuration: serde_json::to_string(host.configuration())?,
            sources: host.parameter_sources()?,
            cursors: host.cursors()?,
        };
        let result = engine
            .run(
                &component,
                host.manifest(),
                request,
                host.clone(),
                stopping.clone(),
                deadline.clone(),
            )
            .await;
        let retry_delay = match &result {
            Ok(Err(Failure::Unavailable(retry))) => {
                Some(retry.not_before.as_ref().map_or_else(
                    || Duration::from_secs(5_u64 << (attempt - 1)),
                    |time| {
                        let now = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
                        let future = i128::from(time.seconds) * 1_000_000_000 + i128::from(time.nanoseconds);
                        Duration::from_nanos(u64::try_from(future.saturating_sub(now)).unwrap_or_default())
                    },
                ))
            },
            Err(sloper_extension_host::Error::Deadline) if background => {
                Some(Duration::from_secs(5_u64 << (attempt - 1)))
            },
            Err(sloper_extension_host::Error::Host(sloper_extension_host::HostError::Unavailable)) => {
                Some(Duration::from_secs(5_u64 << (attempt - 1)))
            },
            _ => None,
        };
        if let Some(delay) = retry_delay {
            if attempt < 5 && started.elapsed().saturating_add(delay) < budget {
                host.retry(delay)?;
                sleep(delay).await;
                continue;
            }
            host.complete(Some(json!({"code":"INTERRUPTED","extensionCode":"UNAVAILABLE","message":"The operation retry budget was exhausted."})))?;
        } else {
            let failure = match result {
                Ok(Ok(())) => None,
                Ok(Err(failure)) => Some(guest_failure(failure)),
                Err(error) => {
                    let mut failure = json!({"code":match error {
                        sloper_extension_host::Error::Deadline => "TIMED_OUT",
                        sloper_extension_host::Error::Trapped(_) => "EXTENSION_TRAPPED",
                        sloper_extension_host::Error::SourceFailed | sloper_extension_host::Error::SourceUnreadable | sloper_extension_host::Error::SourceRead(_) => "SOURCE_UNREADABLE",
                        sloper_extension_host::Error::ItemInvalid => "ITEM_INVALID",
                        sloper_extension_host::Error::LimitExceeded => "LIMIT_EXCEEDED",
                        _ => "HOST_FAILED",
                    }});
                    failure["message"] = Value::String(error.to_string());
                    Some(failure)
                },
            };
            host.complete(failure)?;
        }
        return host.export(&out);
    }
    Err(Error::invalid_response("local operation exceeded its attempt budget"))
}

fn guest_failure(failure: Failure) -> Value {
    let (extension_code, message) = match failure {
        Failure::InvalidParameters(message) => ("INVALID_PARAMETERS", message),
        Failure::NotConnected(message) => ("NOT_CONNECTED", message),
        Failure::Rejected(message) => ("REJECTED", message),
        Failure::Internal(message) => ("INTERNAL", message),
        Failure::Unavailable(retry) => ("UNAVAILABLE", retry.message),
    };
    let mut failure = json!({"code":if extension_code == "NOT_CONNECTED" {"CONNECTION_REQUIRED"} else {"EXTENSION_FAILED"},"extensionCode":extension_code});
    failure["message"] = Value::String(message);
    failure
}

fn reader_names(manifest: &Manifest, action: &str) -> BTreeMap<String, Vec<Value>> {
    manifest.actions[action]
        .resources
        .iter()
        .filter(|(_, capabilities)| capabilities.contains(&Capability::Read))
        .map(|(name, _)| (name.clone(), Vec::new()))
        .collect()
}
