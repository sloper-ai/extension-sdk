use std::{
    fs,
    path::Path,
};

use serde::Serialize;

use super::Error;

/// Files created for a new extension.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaffoldResult {
    /// Publisher-qualified extension name.
    pub extension_id: String,
    /// Created project directory.
    pub directory: std::path::PathBuf,
}

/// Immutable SDK revision embedded when these tools were built.
///
/// Builds prefer an explicit `SLOPER_EXTENSION_SDK_REVISION`, followed by
/// the release's bundled source revision, Cargo's packaged Git provenance,
/// then the SDK checkout's own Git HEAD.
/// Registry installations therefore retain the packaged revision without
/// requiring Git. Returns `None` when provenance is missing or the package
/// contains uncommitted changes, unless an explicit or release revision was
/// supplied.
#[must_use]
pub fn sdk_revision() -> Option<&'static str> {
    option_env!("SLOPER_EXTENSION_SDK_REVISION")
}

/// Creates a complete extension pinned to an explicit immutable SDK revision.
///
/// # Errors
/// Rejects malformed identities, non-immutable revisions, existing targets,
/// and filesystem failures without replacing an existing project.
pub fn scaffold(name: &str, directory: &Path, revision: &str) -> Result<ScaffoldResult, Error> {
    validate_identity(name, revision)?;
    let target = directory.join(name);
    if target.try_exists()? {
        return Err(Error::usage("extension directory already exists"));
    }
    fs::create_dir_all(directory)?;
    let temporary = tempfile::tempdir_in(directory)?;
    fs::create_dir(temporary.path().join("src"))?;
    fs::create_dir(temporary.path().join(".cargo"))?;
    fs::write(
        temporary.path().join(".cargo/config.toml"),
        "[target.wasm32-wasip2]\nrustflags = [\"--cfg\", \"tokio_unstable\"]\n",
    )?;
    fs::write(
        temporary.path().join("Cargo.toml"),
        format!(
            r#"[package]
name = "{}"
version = "0.0.1"
description = "Normalize record labels without changing their identity."
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
sloper-extension = {{ git = "https://github.com/sloper-ai/extension-sdk", rev = "{revision}" }}
serde = {{ version = "1", features = ["derive"] }}
futures = {{ version = "0.3.34", default-features = false, features = ["std", "async-await"] }}
"#,
            name.replace('.', "-")
        ),
    )?;
    fs::write(
        temporary.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly-2026-08-15\"\ntargets = [\"wasm32-wasip2\"]\nprofile = \"minimal\"\n",
    )?;
    fs::write(
        temporary.path().join("src/lib.rs"),
        format!(
            r#"use futures::{{SinkExt, TryStreamExt}};
use serde::{{Deserialize, Serialize}};
use sloper_extension::{{action, extension, Reader, Resource, Result, Writer}};

extension! {{ name: "{name}", actions: [normalize] }}

#[derive(Deserialize, Serialize, Resource)]
#[resource(name = "records", key = id)]
struct Record {{
    #[schema(max_length = 512)]
    id: String,
    label: String,
}}

/// Trim whitespace from record labels.
#[action]
async fn normalize(mut input: Reader<Record>, mut output: Writer<Record>) -> Result<()> {{
    while let Some(record) = input.try_next().await? {{
        output.send(vec![Record {{ id: record.id.clone(), label: record.label.trim().to_owned() }}].into()).await?;
    }}
    Ok(())
}}
"#
        ),
    )?;
    fs::write(temporary.path().join(".gitignore"), "/target\n/dist\n/run\n")?;
    fs::write(temporary.path().join("LICENSE"), include_str!("../templates/LICENSE"))?;
    fs::write(
        temporary.path().join("records.jsonl"),
        "{\"id\":\"record-1\",\"label\":\"  Example record  \"}\n",
    )?;
    fs::write(
        temporary.path().join("README.md"),
        format!(
            r"# {name}

Trim record labels while preserving each record's key.

## Build and run

```sh
sloper-extension build .
sloper-extension check .
sloper-extension run normalize --directory . --resource records=records.jsonl --out run
```

Read the results in `run/records.jsonl`. Each line in `records.jsonl` supplies one input record. Use a new output directory for each run.

See [LICENSE](LICENSE) for the terms covering the Sloper template and SDK. Independently authored code remains yours.
"
        ),
    )?;
    fs::rename(temporary.path(), &target)?;
    Ok(ScaffoldResult {
        extension_id: name.to_owned(),
        directory: target,
    })
}

fn validate_identity(name: &str, revision: &str) -> Result<(), Error> {
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::usage("SDK revision must be a full 40-character Git commit"));
    }
    if name.len() > 128
        || !name.contains('.')
        || name.split('.').any(|part| {
            !part.bytes().next().is_some_and(|byte| byte.is_ascii_lowercase())
                || part.split('-').any(str::is_empty)
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return Err(Error::invalid(
            "extension name must be a dotted lowercase publisher-qualified name",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVISION: &str = "1234567890abcdef1234567890abcdef12345678";

    mod provenance {
        include!("../build.rs");
    }

    #[test]
    fn scaffold_has_complete_example_and_refuses_existing_targets() {
        let directory = tempfile::tempdir().unwrap();
        let result = scaffold("acme.records", directory.path(), REVISION).unwrap();
        assert_eq!(result.extension_id, "acme.records");
        assert!(directory.path().join("acme.records/src/lib.rs").is_file());
        assert!(directory.path().join("acme.records/records.jsonl").is_file());
        assert_eq!(
            fs::read_to_string(directory.path().join("acme.records/LICENSE")).unwrap(),
            include_str!("../templates/LICENSE")
        );
        assert!(!directory.path().join("acme.records/NOTICE").exists());
        let manifest = fs::read_to_string(directory.path().join("acme.records/Cargo.toml")).unwrap();
        assert!(manifest.contains("version = \"0.0.1\"\n"));
        assert!(manifest.contains(REVISION));
        assert!(manifest.contains("https://github.com/sloper-ai/extension-sdk"));
        assert!(scaffold("acme.invalid", directory.path(), "main").is_err());
        assert_eq!(
            fs::read_to_string(directory.path().join("acme.records/.cargo/config.toml")).unwrap(),
            "[target.wasm32-wasip2]\nrustflags = [\"--cfg\", \"tokio_unstable\"]\n",
        );
        assert!(scaffold("acme.records", directory.path(), REVISION).is_err());
        assert!(scaffold("acme..records", directory.path(), REVISION).is_err());
        assert!(scaffold("../records", directory.path(), REVISION).is_err());
    }
}
