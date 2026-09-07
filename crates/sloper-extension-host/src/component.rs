use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    error::Error as StdError,
    fmt,
    str::{
        Utf8Error,
        from_utf8,
    },
};

use serde_json::{
    Map,
    Value,
};
use sloper_extension_spec::{
    Finding,
    Manifest,
    ManifestError,
    parse_fragments,
};

// Bound every component, declaration, and build traversal.
const MAX_COMPONENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_PARTS: usize = 1 + 64 + 32 + 8;
const MAX_NESTING: usize = 32;
const MANIFEST_SECTION: &str = "sloper:manifest";
const PARTS_SECTION: &str = "sloper:parts";

/// A rejected component envelope, build declaration, document, or world.
///
/// Safe publication findings are separate from the retained internal cause.
///
/// ```
/// use sloper_extension_host::extract_manifest;
/// let error = extract_manifest(b"invalid").expect_err("invalid bytes have no component header");
/// assert_eq!(error.findings()[0].code, "MANIFEST_MALFORMED");
/// ```
pub struct ComponentError {
    inner: ErrorInner,
}

#[derive(Debug, thiserror::Error)]
#[error("extension component validation failed")]
struct ErrorInner {
    findings: Vec<Finding>,
    #[source]
    source: Option<ErrorSource>,
}

#[derive(Debug, thiserror::Error)]
enum ErrorSource {
    #[error("manifest validation failed")]
    Manifest(#[from] ManifestError),
    #[error("JSON encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("invalid UTF-8")]
    Utf8(#[from] Utf8Error),
    #[error("component world validation failed")]
    World(#[source] Box<dyn StdError + Send + Sync>),
    #[error("build component envelope is malformed")]
    Assembly(#[source] Box<ComponentError>),
}

impl fmt::Debug for ComponentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComponentError")
            .field("findings", &self.findings())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ComponentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("extension component validation failed")
    }
}

impl StdError for ComponentError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner.source.as_ref().map(|source| source as &dyn StdError)
    }
}

impl From<ManifestError> for ComponentError {
    fn from(source: ManifestError) -> Self {
        Self {
            inner: ErrorInner {
                findings: Vec::new(),
                source: Some(ErrorSource::Manifest(source)),
            },
        }
    }
}

impl ComponentError {
    /// Returns bounded, stable findings suitable for publication responses.
    ///
    /// ```
    /// use sloper_extension_host::extract_manifest;
    /// let error = extract_manifest(b"invalid").expect_err("invalid component");
    /// assert_eq!(error.findings()[0].location, "");
    /// ```
    #[must_use]
    pub fn findings(&self) -> &[Finding] {
        match &self.inner.source {
            Some(ErrorSource::Manifest(source)) => source.findings(),
            _ => &self.inner.findings,
        }
    }

    #[cold]
    pub(crate) fn invalid(code: &str, message: &str) -> Self {
        Self {
            inner: ErrorInner {
                findings: vec![Finding {
                    code: code.into(),
                    location: String::new(),
                    message: message.into(),
                }],
                source: None,
            },
        }
    }

    #[cold]
    fn utf8(source: Utf8Error) -> Self {
        let mut error = Self::invalid("MANIFEST_MALFORMED", "Custom section names must be valid UTF-8.");
        error.inner.source = Some(ErrorSource::Utf8(source));
        error
    }

    #[cold]
    fn json(source: serde_json::Error) -> Self {
        let mut error = Self::invalid("EXTENSION_PARTS_INVALID", "Build declarations could not be encoded.");
        error.inner.source = Some(ErrorSource::Json(source));
        error
    }

    #[cold]
    pub(crate) fn world(source: Box<dyn StdError + Send + Sync>) -> Self {
        let mut error = Self::invalid(
            "MANIFEST_WORLD_INVALID",
            "Component does not implement the supported extension world.",
        );
        error.inner.source = Some(ErrorSource::World(source));
        error
    }

    #[cold]
    fn assembly(source: Self) -> Self {
        // The build boundary exposes assembly diagnostics while retaining the
        // precise malformed-envelope cause for developer diagnostics.
        let mut error = Self::invalid("EXTENSION_PARTS_INVALID", "Build component envelope is malformed.");
        error.inner.source = Some(ErrorSource::Assembly(Box::new(source)));
        error
    }
}

/// Extracts the exact bytes of the single top-level manifest.
///
/// Nested modules are never searched for a manifest. This bounded envelope
/// walk does not replace document or component-world validation.
///
/// # Errors
/// Rejects malformed envelopes, invalid UTF-8 names, duplicate declarations,
/// missing declarations, and component or manifest size overflow.
///
/// ```
/// use sloper_extension_host::extract_manifest;
/// let error = extract_manifest(b"\0asm\x0d\0\x01\0").expect_err("manifest is absent");
/// assert_eq!(error.findings()[0].code, "MANIFEST_ABSENT");
/// ```
pub fn extract_manifest(component: &[u8]) -> Result<&[u8], ComponentError> {
    component_header(component)?;
    let mut manifest = None;
    let mut sections = Sections::new(component);
    while let Some(section) = sections.next()? {
        let Some((name, contents)) = section.custom()? else {
            continue;
        };
        if name != MANIFEST_SECTION {
            continue;
        }
        if manifest.is_some() {
            return Err(ComponentError::invalid(
                "MANIFEST_DUPLICATED",
                "Component has more than one top-level manifest.",
            ));
        }
        if contents.len() > MAX_MANIFEST_BYTES {
            return Err(ComponentError::invalid(
                "MANIFEST_TOO_LARGE",
                "Manifest exceeds 256 KiB.",
            ));
        }
        manifest = Some(contents);
    }
    manifest.ok_or_else(|| ComponentError::invalid("MANIFEST_ABSENT", "Top-level manifest section is absent."))
}

fn component_header(bytes: &[u8]) -> Result<(), ComponentError> {
    if bytes.len() < 8 || &bytes[..4] != b"\0asm" || bytes[6..8] != [1, 0] {
        return Err(ComponentError::invalid(
            "MANIFEST_MALFORMED",
            "Expected a WebAssembly component header.",
        ));
    }
    if bytes.len() > MAX_COMPONENT_BYTES {
        return Err(ComponentError::invalid(
            "MANIFEST_TOO_LARGE",
            "Component exceeds 64 MiB.",
        ));
    }
    Ok(())
}

struct Section<'a> {
    id: u8,
    contents: &'a [u8],
}

impl<'a> Section<'a> {
    fn custom(&self) -> Result<Option<(&'a str, &'a [u8])>, ComponentError> {
        if self.id != 0 {
            return Ok(None);
        }
        let (length, start) = unsigned_leb(self.contents, 0)?;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= self.contents.len())
            .ok_or_else(|| {
                ComponentError::invalid(
                    "MANIFEST_MALFORMED",
                    "Custom section name exceeds its enclosing section.",
                )
            })?;
        let name = from_utf8(&self.contents[start..end]).map_err(ComponentError::utf8)?;
        Ok(Some((name, &self.contents[end..])))
    }
}

struct Sections<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Sections<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            position: 8,
        }
    }

    fn next(&mut self) -> Result<Option<Section<'a>>, ComponentError> {
        if self.position == self.bytes.len() {
            return Ok(None);
        }
        let id = self.bytes[self.position];
        let (length, start) = unsigned_leb(self.bytes, self.position + 1)?;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| {
                ComponentError::invalid(
                    "MANIFEST_MALFORMED",
                    "Section exceeds its enclosing component or module.",
                )
            })?;
        self.position = end;
        Ok(Some(Section {
            id,
            contents: &self.bytes[start..end],
        }))
    }
}

fn unsigned_leb(bytes: &[u8], mut position: usize) -> Result<(usize, usize), ComponentError> {
    let mut value = 0;
    for index in 0..5 {
        let Some(&byte) = bytes.get(position) else {
            return Err(ComponentError::invalid(
                "MANIFEST_MALFORMED",
                "Truncated section length.",
            ));
        };
        position += 1;
        if index == 4 && byte > 0x0f {
            return Err(ComponentError::invalid(
                "MANIFEST_MALFORMED",
                "Section length exceeds unsigned 32-bit LEB128.",
            ));
        }
        value |= usize::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok((value, position));
        }
    }
    Err(ComponentError::invalid(
        "MANIFEST_MALFORMED",
        "Section length exceeds unsigned 32-bit LEB128.",
    ))
}

/// Extracts build fragments from nested core modules in a component.
///
/// A component-level `sloper:parts` section is not a toolkit declaration.
/// Nested components are visited only to reach their core modules.
///
/// # Errors
/// Rejects malformed envelopes and declarations exceeding manifest bounds.
///
/// ```
/// use sloper_extension_host::extract_parts;
/// assert!(extract_parts(b"\0asm\x0d\0\x01\0")?.is_empty());
/// # Ok::<(), sloper_extension_host::ComponentError>(())
/// ```
pub fn extract_parts(component: &[u8]) -> Result<Vec<&[u8]>, ComponentError> {
    component_header(component).map_err(ComponentError::assembly)?;
    let mut parts = Vec::new();
    walk_parts(component, &mut parts, 0).map_err(ComponentError::assembly)?;
    Ok(parts)
}

fn walk_parts<'a>(bytes: &'a [u8], parts: &mut Vec<&'a [u8]>, depth: usize) -> Result<(), ComponentError> {
    if depth > MAX_NESTING || bytes.len() < 8 || &bytes[..4] != b"\0asm" {
        return Err(ComponentError::invalid(
            "EXTENSION_PARTS_INVALID",
            "Malformed module or excessive nesting.",
        ));
    }
    let component = match &bytes[4..8] {
        [1, 0, 0, 0] => false,
        [_, _, 1, 0] => true,
        _ => {
            return Err(ComponentError::invalid(
                "EXTENSION_PARTS_INVALID",
                "Invalid module or component header.",
            ));
        },
    };
    let mut sections = Sections::new(bytes);
    while let Some(section) = sections.next()? {
        if let Some((name, contents)) = section.custom()? {
            if !component && name == PARTS_SECTION {
                if contents.len() > MAX_MANIFEST_BYTES || parts.len() >= MAX_PARTS {
                    return Err(ComponentError::invalid(
                        "EXTENSION_PARTS_INVALID",
                        "Build declarations exceed their bounds.",
                    ));
                }
                parts.push(contents);
            }
        } else if component && matches!(section.id, 1 | 4) {
            let expected_layer = if section.id == 1 {
                [0, 0]
            } else {
                [1, 0]
            };
            if section.contents.get(6..8) != Some(expected_layer.as_slice()) {
                return Err(ComponentError::invalid(
                    "EXTENSION_PARTS_INVALID",
                    "Nested section has the wrong encoding layer.",
                ));
            }
            walk_parts(section.contents, parts, depth + 1)?;
        }
    }
    Ok(())
}

/// Assembles one extension and its named declarations deterministically.
///
/// Named maps sort by name; schema property and required-field order remain
/// authored order. Resource capability sets are derived when not specified.
///
/// # Errors
/// Rejects missing, duplicate, malformed, or excessive parts; mismatched action
/// lists; and documents failing the public manifest validator.
///
/// ```
/// use sloper_extension_host::assemble_parts;
/// let parts = [
///     br#"{"kind":"extension","name":"acme.demo","version":"1.0.0","description":"Demo","actions":["run"]}"#.as_slice(),
///     br#"{"kind":"action","name":"run"}"#.as_slice(),
/// ];
/// assert!(!assemble_parts(&parts)?.is_empty());
/// # Ok::<(), sloper_extension_host::ComponentError>(())
/// ```
pub fn assemble_parts(parts: &[&[u8]]) -> Result<Vec<u8>, ComponentError> {
    if parts.len() > MAX_PARTS {
        return Err(ComponentError::invalid(
            "EXTENSION_PARTS_INVALID",
            "Too many build declaration sections.",
        ));
    }
    let mut extension = None;
    let mut actions = BTreeMap::new();
    let mut resources = BTreeMap::new();
    let mut connections = BTreeMap::new();
    let mut count = 0usize;
    let mut total_bytes = 0usize;
    for bytes in parts {
        total_bytes = total_bytes
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_COMPONENT_BYTES)
            .ok_or_else(|| {
                ComponentError::invalid(
                    "EXTENSION_PARTS_INVALID",
                    "Build declaration bytes exceed their bounds.",
                )
            })?;
        for value in parse_fragments(bytes)? {
            count += 1;
            if count > MAX_PARTS {
                return Err(ComponentError::invalid(
                    "EXTENSION_PARTS_INVALID",
                    "Too many build declarations.",
                ));
            }
            let Value::Object(mut part) = value else {
                return Err(ComponentError::invalid(
                    "EXTENSION_PARTS_INVALID",
                    "Build declaration must be an object.",
                ));
            };
            let kind = take_part_string(&mut part, "kind", "Build declaration kind is required.")?;
            if kind == "extension" {
                if extension.replace(part).is_some() {
                    return Err(ComponentError::invalid(
                        "EXTENSION_PARTS_DUPLICATED",
                        "Exactly one extension part is required.",
                    ));
                }
                continue;
            }
            let name = take_part_string(&mut part, "name", "Named build declaration requires a name.")?;
            let entries = match kind.as_str() {
                "action" => &mut actions,
                "resource" => &mut resources,
                "connection" => &mut connections,
                _ => {
                    return Err(ComponentError::invalid(
                        "EXTENSION_PARTS_INVALID",
                        "Unknown build declaration kind.",
                    ));
                },
            };
            if entries.insert(name, Value::Object(part)).is_some() {
                return Err(ComponentError::invalid(
                    "EXTENSION_PARTS_DUPLICATED",
                    "Named build declarations must be unique.",
                ));
            }
        }
    }
    let mut extension =
        extension.ok_or_else(|| ComponentError::invalid("EXTENSION_PARTS_ABSENT", "Extension part is missing."))?;
    if extension.contains_key("resources") || extension.contains_key("connections") {
        return Err(ComponentError::invalid(
            "EXTENSION_PARTS_INVALID",
            "Resources and connections must use named build declarations.",
        ));
    }
    validate_action_list(&mut extension, &actions)?;
    derive_capabilities(&actions, &mut resources);
    extension.insert("actions".into(), Value::Object(actions.into_iter().collect()));
    extension.insert("resources".into(), Value::Object(resources.into_iter().collect()));
    extension.insert("connections".into(), Value::Object(connections.into_iter().collect()));
    let bytes = serde_json::to_vec(&extension).map_err(ComponentError::json)?;
    let manifest = Manifest::parse(&bytes)?;
    let bytes = serde_json::to_vec(&manifest).map_err(ComponentError::json)?;
    // Defaults introduced by canonical encoding must obey the same byte bound.
    Manifest::parse(&bytes)?;
    Ok(bytes)
}

fn take_part_string(part: &mut Map<String, Value>, key: &str, message: &'static str) -> Result<String, ComponentError> {
    match part.remove(key) {
        Some(Value::String(value)) => Ok(value),
        _ => Err(ComponentError::invalid("EXTENSION_PARTS_INVALID", message)),
    }
}

fn validate_action_list(
    extension: &mut Map<String, Value>,
    actions: &BTreeMap<String, Value>,
) -> Result<(), ComponentError> {
    let declared = extension
        .remove("actions")
        .and_then(|value| {
            match value {
                Value::Array(value) => Some(value),
                _ => None,
            }
        })
        .ok_or_else(|| {
            ComponentError::invalid(
                "EXTENSION_ACTION_LIST_INVALID",
                "Extension part requires an explicit action list.",
            )
        })?;
    let names = declared.iter().filter_map(Value::as_str).collect::<BTreeSet<_>>();
    if names.len() != declared.len()
        || names.len() != actions.len()
        || !names.iter().all(|name| actions.contains_key(*name))
    {
        return Err(ComponentError::invalid(
            "EXTENSION_ACTION_LIST_INVALID",
            "Explicit action list must match action parts in both directions.",
        ));
    }
    Ok(())
}

fn derive_capabilities(actions: &BTreeMap<String, Value>, resources: &mut BTreeMap<String, Value>) {
    let mut unions = BTreeMap::<String, BTreeSet<String>>::new();
    for action in actions.values() {
        if let Some(entries) = action.get("resources").and_then(Value::as_object) {
            for (name, capabilities) in entries {
                if let Some(capabilities) = capabilities.as_array() {
                    for capability in capabilities.iter().filter_map(Value::as_str) {
                        unions.entry(name.clone()).or_default().insert(capability.into());
                    }
                }
            }
        }
    }
    for (name, resource) in resources {
        if let Some(resource) = resource.as_object_mut() {
            resource.entry("capabilities").or_insert_with(|| {
                Value::Array(
                    unions
                        .remove(name)
                        .unwrap_or_default()
                        .into_iter()
                        .map(Value::String)
                        .collect(),
                )
            });
        }
    }
}

/// Appends the exact validated manifest bytes as one top-level section.
///
/// # Errors
/// Refuses an already stamped component, malformed envelope, invalid manifest,
/// or stamped output exceeding the component size limit.
///
/// ```
/// use sloper_extension_host::{
///     extract_manifest,
///     stamp_manifest,
/// };
/// let manifest =
///     br#"{"name":"acme.demo","version":"1.0.0","description":"Demo","actions":{"run":{}}}"#;
/// let bytes = stamp_manifest(b"\0asm\x0d\0\x01\0", manifest)?;
/// assert_eq!(extract_manifest(&bytes)?, manifest);
/// # Ok::<(), sloper_extension_host::ComponentError>(())
/// ```
pub fn stamp_manifest(component: &[u8], manifest: &[u8]) -> Result<Vec<u8>, ComponentError> {
    match extract_manifest(component) {
        Ok(_) => {
            return Err(ComponentError::invalid(
                "EXTENSION_ALREADY_STAMPED",
                "Component already has a top-level manifest.",
            ));
        },
        Err(error)
            if error
                .findings()
                .first()
                .is_some_and(|finding| finding.code == "MANIFEST_ABSENT") => {},
        Err(error) => return Err(ComponentError::assembly(error)),
    }
    Manifest::parse(manifest)?;
    let name_length = MANIFEST_SECTION.len();
    let payload_length = leb_width(name_length)
        .checked_add(name_length)
        .and_then(|length| length.checked_add(manifest.len()));
    let output_length = payload_length
        .and_then(|length| {
            component
                .len()
                .checked_add(1)?
                .checked_add(leb_width(length))?
                .checked_add(length)
        })
        .filter(|length| *length <= MAX_COMPONENT_BYTES);
    let Some((payload_length, output_length)) = payload_length.zip(output_length) else {
        return Err(ComponentError::invalid(
            "EXTENSION_PARTS_INVALID",
            "Stamped component exceeds 64 MiB.",
        ));
    };
    let mut output = Vec::with_capacity(output_length);
    output.extend_from_slice(component);
    output.push(0);
    encode_leb(payload_length, &mut output);
    encode_leb(name_length, &mut output);
    output.extend_from_slice(MANIFEST_SECTION.as_bytes());
    output.extend_from_slice(manifest);
    Ok(output)
}

fn leb_width(mut value: usize) -> usize {
    let mut width = 1;
    while value >= 0x80 {
        value >>= 7;
        width += 1;
    }
    width
}

fn encode_leb(mut value: usize, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f).to_le_bytes()[0];
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Validates a component and compares its embedded bytes with reassembly.
///
/// The component is never rewritten and no guest code is executed.
///
/// # Errors
/// Returns document, world, build, or manifest-mismatch findings.
///
/// ```
/// use sloper_extension_host::check_component;
/// assert!(check_component(b"invalid").is_err());
/// ```
pub fn check_component(component: &[u8]) -> Result<Manifest, ComponentError> {
    let manifest = crate::validate_component(component)?;
    let embedded = extract_manifest(component)?;
    let parts = extract_parts(component)?;
    let assembled = assemble_parts(&parts)?;
    if embedded != assembled {
        return Err(ComponentError::invalid(
            "EXTENSION_MANIFEST_MISMATCH",
            "Embedded manifest differs from its assembled declarations.",
        ));
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error as _,
        ptr,
    };

    use sloper_extension_spec::ManifestError;

    use super::{
        ComponentError,
        MANIFEST_SECTION,
        assemble_parts,
        encode_leb,
        extract_manifest,
        extract_parts,
        leb_width,
        stamp_manifest,
    };

    const HEADER: &[u8] = b"\0asm\x0d\0\x01\0";
    const MANIFEST: &[u8] = br#"{"name":"acme.demo","version":"1.0.0","description":"Demo","actions":{"run":{}}}"#;

    #[test]
    fn extracted_metadata_borrows_the_original_component() {
        let stamped = stamp_manifest(HEADER, MANIFEST).unwrap();
        let manifest = extract_manifest(&stamped).unwrap();
        assert!(ptr::eq(
            manifest.as_ptr(),
            stamped[stamped.len() - MANIFEST.len()..].as_ptr()
        ));
        let mut module = b"\0asm\x01\0\0\0".to_vec();
        let part = br#"{"kind":"action","name":"run"}"#;
        let mut section = Vec::new();
        encode_leb("sloper:parts".len(), &mut section);
        section.extend_from_slice(b"sloper:parts");
        section.extend_from_slice(part);
        module.push(0);
        encode_leb(section.len(), &mut module);
        module.extend_from_slice(&section);
        let mut component = HEADER.to_vec();
        component.push(1);
        encode_leb(module.len(), &mut component);
        component.extend_from_slice(&module);
        let parts = extract_parts(&component).unwrap();
        assert_eq!(parts, [part.as_slice()]);
        assert!(ptr::eq(
            parts[0].as_ptr(),
            component[component.len() - part.len()..].as_ptr()
        ));
    }

    #[test]
    fn manifest_findings_reuse_the_retained_native_cause() {
        let source = ManifestError::invalid("MANIFEST_INVALID_VALUE", "item", "Invalid value.");
        let error = ComponentError::from(source);
        let Some(super::ErrorSource::Manifest(source)) = &error.inner.source else {
            panic!("manifest cause must be retained");
        };
        assert!(ptr::eq(error.findings().as_ptr(), source.findings().as_ptr()));
        assert_eq!(error.findings()[0].location, "item");
        assert!(error.source().and_then(|source| source.source()).is_some());
        assert!(format!("{error:?}").contains("MANIFEST_INVALID_VALUE"));
        assert_eq!(error.to_string(), "extension component validation failed");
    }

    #[test]
    fn stamping_renders_exact_sections_across_leb_widths() {
        for padding in [0, 1, 127, 128, 16383, 16384] {
            let mut manifest = MANIFEST.to_vec();
            manifest.resize(manifest.len() + padding, b' ');
            let mut payload = Vec::new();
            encode_leb(MANIFEST_SECTION.len(), &mut payload);
            payload.extend_from_slice(MANIFEST_SECTION.as_bytes());
            payload.extend_from_slice(&manifest);
            let mut expected = HEADER.to_vec();
            expected.push(0);
            encode_leb(payload.len(), &mut expected);
            expected.extend_from_slice(&payload);
            let actual = stamp_manifest(HEADER, &manifest).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(actual.capacity(), actual.len());
        }
        for value in [0, 1, 127, 128, 16383, 16384, u32::MAX as usize, usize::MAX] {
            let mut bytes = Vec::new();
            encode_leb(value, &mut bytes);
            assert_eq!(leb_width(value), bytes.len());
        }
    }

    #[test]
    fn moved_declaration_fields_keep_type_and_action_list_rejections() {
        let extension =
            br#"{"kind":"extension","name":"acme.demo","version":"1.0.0","description":"Demo","actions":["run"]}"#
                .as_slice();
        let action = br#"{"kind":"action","name":"run"}"#.as_slice();
        for invalid in [
            br#"{"name":"run"}"#.as_slice(),
            br#"{"kind":1,"name":"run"}"#,
            br#"{"kind":"action"}"#,
            br#"{"kind":"action","name":[]}"#,
        ] {
            assert_eq!(
                assemble_parts(&[extension, invalid]).unwrap_err().findings()[0].code,
                "EXTENSION_PARTS_INVALID"
            );
        }
        for actions in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!(["run", "run"]),
            serde_json::json!(["other"]),
        ] {
            let mut value: serde_json::Value = serde_json::from_slice(extension).unwrap();
            value["actions"] = actions;
            let bytes = serde_json::to_vec(&value).unwrap();
            assert_eq!(
                assemble_parts(&[&bytes, action]).unwrap_err().findings()[0].code,
                "EXTENSION_ACTION_LIST_INVALID"
            );
        }
        assert_eq!(
            assemble_parts(&[extension, action, action]).unwrap_err().findings()[0].code,
            "EXTENSION_PARTS_DUPLICATED"
        );
    }
}
