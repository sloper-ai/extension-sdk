use std::{
    collections::BTreeMap,
    path::PathBuf,
};

use serde::{
    Deserialize,
    Serialize,
};
use serde_json::{
    Value,
    json,
};

use crate::Error;

/// Explicit inputs for a disposable extension execution.
///
/// Values with extension-defined schemas remain JSON. Fixture files are keyed
/// by declared resource or source parameter names.
#[derive(Clone, Debug)]
pub struct RunOptions {
    /// Directory containing `dist/extension.wasm`.
    pub directory: PathBuf,
    /// Declared action name.
    pub action: String,
    /// Destination which must not already exist.
    pub out: PathBuf,
    /// Parameters accepted by the action's schema.
    pub parameters: Value,
    /// Extension configuration.
    pub configuration: Value,
    /// Source keys mapped to local files.
    pub sources: BTreeMap<String, PathBuf>,
    /// Resource reader names mapped to JSONL fixtures.
    pub resources: BTreeMap<String, PathBuf>,
}

/// Persisted result of one disposable local operation.
#[derive(Debug, Deserialize, Serialize)]
pub struct RunResult {
    /// Operation state, input/result commits, failure details, and timing.
    pub operation: Value,
    /// Export directory.
    pub out: PathBuf,
    /// Resource JSONL receipts.
    pub resources: Vec<ResourceResult>,
    /// Produced source receipts.
    pub sources: Vec<SourceResult>,
}

/// One exported resource fixture.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceResult {
    /// Declared resource identity.
    pub extension_resource_id: String,
    /// Number of committed items encoded as an exact decimal integer.
    pub items: String,
    /// Exported JSONL path.
    pub path: PathBuf,
}

/// One exported source.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceResult {
    /// Local source identity.
    pub source_id: String,
    /// Exported file path.
    pub path: PathBuf,
}

/// Executes the exact distributable bytes in a fresh disposable local host.
///
/// # Errors
/// Returns argument, fixture, runtime admission, execution, or export errors.
/// Guest operation failures are preserved in the returned operation receipt.
/// # Cancel safety
/// Cancellation drops the isolated host and temporary state. An export appears
/// only after a complete operation has been atomically persisted.
pub async fn run(options: RunOptions) -> Result<RunResult, Error> {
    let files = |values: BTreeMap<String, PathBuf>| -> Result<Vec<String>, Error> {
        values
            .into_iter()
            .map(|(name, path)| {
                let path = path
                    .to_str()
                    .ok_or_else(|| Error::usage("fixture paths must be UTF-8"))?;
                if name.is_empty() || name.contains('=') {
                    return Err(Error::usage("fixture keys must be nonempty and cannot contain '='"));
                }
                Ok(format!("{name}={path}"))
            })
            .collect()
    };
    let Value::Object(arguments) = json!({
        "directory":options.directory,"action":options.action,"out":options.out,
        "parameters":options.parameters,"configuration":options.configuration,
        "source":files(options.sources)?,"resource":files(options.resources)?,
    }) else {
        unreachable!("object literal is a JSON object")
    };
    Ok(serde_json::from_value(crate::runner::run(arguments).await?)?)
}
