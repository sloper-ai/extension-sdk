//! Bounded static validation without execution or authoring dependencies.

use std::{
    fs::File,
    io::{
        self,
        Read as _,
    },
    path::Path,
};

use serde::Serialize;
use sloper_extension_host::{
    extract_manifest,
    validate_component,
};
use sloper_extension_spec::Finding;

const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;

/// Static component verdict, preserving the exact embedded manifest text.
#[derive(Debug, Serialize)]
pub struct Validation {
    /// Whether the component satisfies the published extension specification.
    pub valid: bool,
    /// Bounded findings for a rejected component, empty on success.
    pub findings: Vec<Finding>,
    /// Exact UTF-8 manifest text on success, absent on rejection.
    pub manifest: Option<String>,
}

/// An operational failure while reading or extracting a validation input.
///
/// A rejected component is returned as [`Validation`] with `valid: false`.
/// Display output omits the input bytes and filesystem path.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ValidationError {
    /// The component file could not be read.
    #[error("component input could not be read")]
    Read(#[from] io::Error),
    /// An accepted manifest could not be decoded as UTF-8.
    #[error("validated manifest encoding is invalid")]
    Encoding(#[from] std::str::Utf8Error),
    /// An accepted component's manifest could not be extracted.
    #[error("validated component lost its manifest")]
    Manifest(#[from] sloper_extension_host::ComponentError),
}

/// Checks a component without instantiating guest code.
///
/// Input is capped at 64 MiB before parsing. Successful validation retains the
/// exact embedded manifest bytes as UTF-8 text, including whitespace.
///
/// # Errors
/// Returns an operational read or extraction failure. Invalid and oversized
/// components return a rejected [`Validation`] instead.
pub fn validate_file(path: &Path) -> Result<Validation, ValidationError> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_COMPONENT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_COMPONENT_BYTES {
        return Ok(Validation {
            valid: false,
            findings: vec![Finding {
                code: "MANIFEST_TOO_LARGE".into(),
                location: String::new(),
                message: "Component exceeds 64 MiB.".into(),
            }],
            manifest: None,
        });
    }
    Ok(match validate_component(&bytes) {
        Ok(_) => {
            Validation {
                valid: true,
                findings: Vec::new(),
                manifest: Some(std::str::from_utf8(extract_manifest(&bytes)?)?.to_owned()),
            }
        },
        Err(error) => {
            Validation {
                valid: false,
                findings: error.findings().to_vec(),
                manifest: None,
            }
        },
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn invalid_component_returns_findings_and_no_manifest() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), b"invalid component").unwrap();
        let result = validate_file(file.path()).unwrap();
        assert!(!result.valid);
        assert_ne!(result.findings, []);
        assert!(result.manifest.is_none());
    }

    #[test]
    fn missing_file_is_an_operational_failure_without_its_path() {
        let directory = tempfile::tempdir().unwrap();
        let error = validate_file(&directory.path().join("missing.wasm")).unwrap_err();
        assert_eq!(error.to_string(), "component input could not be read");
    }

    #[test]
    fn accepted_component_preserves_exact_embedded_manifest_bytes() {
        let original = include_bytes!("../tests/fixtures/host-guest.wasm");
        let manifest = extract_manifest(original).unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), original).unwrap();
        let result = validate_file(file.path()).unwrap();
        assert!(result.valid);
        assert_eq!(result.findings, []);
        assert_eq!(result.manifest.unwrap().as_bytes(), manifest);
    }
}
