mod licenses;

use std::{
    borrow::Cow,
    env,
    fs,
    io::Write as _,
    path::{
        Path,
        PathBuf,
    },
    process::Stdio,
};

use serde::{
    Deserialize,
    Serialize,
};
use serde_json::json;
use sloper_extension_host::{
    assemble_parts,
    check_component,
    extract_parts,
    stamp_manifest,
    validate_component,
};
use tokio::{
    io::{
        AsyncBufReadExt as _,
        BufReader,
    },
    process::Command,
};

use super::{
    Error,
    read_component,
};

/// Successful static validation, preserving the parsed embedded manifest.
#[derive(Debug, Serialize)]
pub struct CheckResult {
    /// True after all static checks have passed.
    pub valid: bool,
    /// Empty on success; rejections are returned as structured errors.
    pub findings: Vec<sloper_extension_spec::Finding>,
    /// Validated declaration.
    pub manifest: sloper_extension_spec::Manifest,
}

/// The distributable component persisted by a build.
#[derive(Debug, Serialize)]
pub struct BuildResult {
    /// Exact stamped bytes to test and publish.
    pub component: PathBuf,
    /// Static admission of those bytes.
    pub check: CheckResult,
    /// Validated optional PNG distributed alongside the component.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<PathBuf>,
}

fn validate(component: &[u8]) -> Result<CheckResult, Error> {
    match validate_component(component) {
        Ok(manifest) => {
            Ok(CheckResult {
                valid: true,
                findings: Vec::new(),
                manifest,
            })
        },
        Err(error) => {
            Err(Error::from(crate::Validation {
                valid: false,
                findings: error.findings().to_vec(),
                manifest: None,
            }))
        },
    }
}

#[derive(Deserialize)]
struct CargoEvent<'a> {
    #[serde(borrow)]
    package_id: Option<CargoText<'a>>,
    #[serde(borrow)]
    reason: Option<CargoText<'a>>,
    #[serde(borrow)]
    message: Option<CargoMessage<'a>>,
    #[serde(borrow)]
    target: Option<CargoTarget<'a>>,
    #[serde(borrow)]
    filenames: Option<Vec<CargoText<'a>>>,
}

#[derive(Deserialize)]
struct CargoMessage<'a> {
    #[serde(borrow)]
    rendered: Option<CargoText<'a>>,
}

#[derive(Deserialize)]
struct CargoTarget<'a> {
    #[serde(borrow)]
    crate_types: Option<Vec<CargoText<'a>>>,
}

#[derive(Deserialize)]
#[serde(transparent)]
struct CargoText<'a>(#[serde(borrow)] Cow<'a, str>);

fn cargo_event(line: &str) -> Result<CargoEvent<'_>, Error> {
    serde_json::from_str(line).map_err(|_| Error::invalid_response("Cargo emitted malformed build metadata"))
}

struct Compiled {
    component: Vec<u8>,
    package_id: String,
}

async fn compile(directory: &Path) -> Result<Compiled, Error> {
    let mut command = Command::new("cargo");
    command
        .current_dir(directory)
        .args([
            "build",
            "-Ztrim-paths",
            "--config",
            "profile.release.trim-paths='object'",
            "--config",
            "profile.release.opt-level='s'",
            "--config",
            "profile.release.lto='thin'",
            "--config",
            "profile.release.codegen-units=1",
            "--config",
            "profile.release.debug=false",
            "--config",
            "profile.release.strip='symbols'",
            "--release",
            "--target",
            "wasm32-wasip2",
            "--config",
            "target.wasm32-wasip2.rustflags=['--cfg', 'tokio_unstable']",
            "--message-format=json-render-diagnostics",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    // Cargo build scripts are developer code. Publication credentials never
    // cross that subprocess boundary, even when provided in the invoking shell.
    strip_credentials(&mut command, env::vars_os().map(|(name, _)| name));
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::invalid_response("Cargo output pipe is missing"))?;
    let mut lines = BufReader::new(stdout).lines();
    let mut artifacts = Vec::new();
    while let Some(line) = lines.next_line().await? {
        let event = cargo_event(&line)?;
        if event.reason.as_ref().map(|reason| reason.0.as_ref()) == Some("compiler-message")
            && let Some(rendered) = event.message.and_then(|message| message.rendered)
        {
            eprint!("{}", rendered.0);
        }
        if event.reason.as_ref().map(|reason| reason.0.as_ref()) == Some("compiler-artifact")
            && event
                .target
                .as_ref()
                .and_then(|target| target.crate_types.as_ref())
                .is_some_and(|types| types.iter().any(|value| value.0 == "cdylib"))
            && let Some(files) = event.filenames
        {
            let package_id = event
                .package_id
                .ok_or_else(|| Error::invalid_response("Cargo artifact has no package identity"))?
                .0
                .into_owned();
            artifacts.extend(
                files
                    .into_iter()
                    .filter(|file| {
                        Path::new(file.0.as_ref())
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("wasm"))
                    })
                    .map(|file| (PathBuf::from(file.0.into_owned()), package_id.clone())),
            );
        }
    }
    if !child.wait().await?.success() {
        return Err(Error::invalid("extension Cargo build failed; see standard error"));
    }
    let [(artifact, package_id)] = artifacts.as_slice() else {
        return Err(Error::invalid(
            "extension build must produce exactly one Cargo library component",
        ));
    };
    let component = read_component(artifact)?;
    let parts = extract_parts(&component)?;
    let manifest = assemble_parts(&parts)?;
    Ok(Compiled {
        component: stamp_manifest(&component, &manifest)?,
        package_id: package_id.clone(),
    })
}

fn strip_credentials(command: &mut Command, names: impl Iterator<Item = std::ffi::OsString>) {
    command.env_remove("SLOPER_API_TOKEN");
    for name in names {
        if name.to_str().is_some_and(|name| name.starts_with("SLOPER_TOKEN_")) {
            command.env_remove(name);
        }
    }
}

/// Builds and atomically persists the final stamped distributable.
///
/// Components use size-optimized Cargo release builds with `ThinLTO`, disabled
/// debug information, and compiler symbol stripping.
///
/// # Errors
/// Returns build, declaration, validation, icon, or filesystem failures.
/// # Cancel safety
/// Cancellation terminates Cargo and leaves an existing distributable
/// untouched.
pub async fn build(directory: &Path) -> Result<BuildResult, Error> {
    let compiled = compile(directory).await?;
    let component = compiled.component;
    check_component(&component)?;
    let check = validate(&component)?;
    let dist = directory.join("dist");
    fs::create_dir_all(&dist)?;
    let icon = dist.join("icon.png");
    let has_icon = icon.try_exists()?;
    if has_icon {
        super::icon::read(&icon)?;
    }
    licenses::copy(directory, &dist, &compiled.package_id).await?;
    let destination = dist.join("extension.wasm");
    let mut temporary = tempfile::NamedTempFile::new_in(&dist)?;
    temporary.write_all(&component)?;
    temporary.as_file().sync_all()?;
    temporary.persist(&destination).map_err(|error| error.error)?;
    Ok(BuildResult {
        component: destination,
        check,
        icon: has_icon.then_some(icon),
    })
}

/// Rebuilds declarations and checks that persisted distributable bytes are
/// current.
///
/// # Errors
/// Returns build, declaration, static validation, or stale-distributable
/// findings. # Cancel safety
/// Cancellation terminates Cargo; checking never replaces distributable bytes.
pub async fn check(directory: &Path) -> Result<CheckResult, Error> {
    let fresh = compile(directory).await?.component;
    check_component(&fresh)?;
    validate(&fresh)?;
    let destination = directory.join("dist/extension.wasm");
    let existing = read_component(&destination)?;
    check_component(&existing)?;
    let validation = validate(&existing)?;
    if existing != fresh {
        let mut check = json!({"valid":false,"findings":[{"code":"EXTENSION_BUILD_STALE","location":"dist/extension.wasm","message":"Built component differs from fresh assembly"}]});
        check["manifest"] = serde_json::to_value(&validation.manifest)?;
        return Err(Error::ComponentCheck(check));
    }
    Ok(validation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_component_is_a_validation_failure_without_a_manifest() {
        let error = validate(b"not a component").unwrap_err();
        assert!(error.to_string().contains("validation"), "error={error}");
    }

    #[test]
    fn bounded_input_rejects_oversized_objects() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("component");
        fs::write(&path, b"12345").unwrap();
        assert_eq!(super::super::read_bounded(&path, 5).unwrap(), b"12345");
        assert!(super::super::read_bounded(&path, 4).is_err());
    }
    #[test]
    fn cargo_metadata_borrows_plain_fields_and_retains_escaped_diagnostics() {
        let event = cargo_event(r#"{"reason":"compiler-artifact","package_id":"registry+https://example.test#library@1.0.0","fresh":true,"target":{"crate_types":["cdylib"],"unused":{"large":[1,2,3]}},"filenames":["target/component.wasm","escaped\\component.wasm"]}"#).unwrap();
        assert!(matches!(event.reason.unwrap().0, Cow::Borrowed("compiler-artifact")));
        assert!(matches!(
            event.package_id.unwrap().0,
            Cow::Borrowed("registry+https://example.test#library@1.0.0")
        ));
        let files = event.filenames.unwrap();
        assert!(matches!(files[0].0, Cow::Borrowed("target/component.wasm")));
        assert!(matches!(files[1].0, Cow::Owned(_)));
        assert_eq!(files[1].0, "escaped\\component.wasm");
        let event = cargo_event(r#"{"reason":"compiler-message","message":{"rendered":"error:\nfailed"}}"#).unwrap();
        assert_eq!(event.message.unwrap().rendered.unwrap().0, "error:\nfailed");
        assert!(cargo_event("not JSON").is_err());
        assert!(cargo_event(r#"{"filenames":4}"#).is_err());
        assert!(
            cargo_event(r#"{"reason":"build-finished","success":true}"#)
                .unwrap()
                .filenames
                .is_none()
        );
    }
    #[test]
    fn cargo_never_inherits_publishing_or_provider_bearers() {
        let mut command = Command::new("cargo");
        strip_credentials(
            &mut command,
            ["SLOPER_TOKEN_GMAIL".into(), "SLOPER_TOKEN_CUSTOM".into(), "PATH".into()].into_iter(),
        );
        let variables = command
            .as_std()
            .get_envs()
            .collect::<std::collections::BTreeMap<_, _>>();
        for name in ["SLOPER_API_TOKEN", "SLOPER_TOKEN_GMAIL", "SLOPER_TOKEN_CUSTOM"] {
            assert_eq!(variables[std::ffi::OsStr::new(name)], None);
        }
        assert!(!variables.contains_key(std::ffi::OsStr::new("PATH")));
    }
}
