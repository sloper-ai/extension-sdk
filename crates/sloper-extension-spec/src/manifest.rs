use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    fmt::Write as _,
    slice,
};

use schemars::JsonSchema;
use serde::{
    Deserialize,
    Serialize,
};

use super::{
    schema::{
        Budget,
        Schema,
        validate_schema,
    },
    strict_json::{
        fits_encoded_limit,
        parse_json,
    },
};

/// Maximum encoded manifest document size.
pub(super) const MAX_MANIFEST_BYTES: usize = 256 * 1024;
/// Maximum number of declarations of each manifest entry type.
pub(super) const MAX_ACTIONS: usize = 64;
pub(super) const MAX_RESOURCES: usize = 32;
pub(super) const MAX_CONNECTIONS: usize = 8;
const MAX_ACTION_RESOURCES: usize = 8;
const MAX_CONNECTION_SCOPES: usize = 32;

/// A stable, safe validation finding with a JSON Pointer location.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    /// Stable machine-readable code.
    pub code: String,
    /// JSON Pointer identifying the invalid declaration.
    pub location: String,
    /// Safe explanation containing no provider or credential data.
    pub message: String,
}

/// A rejected manifest, component envelope, or build declaration.
#[derive(Debug, thiserror::Error)]
#[error("extension validation failed")]
pub struct ManifestError {
    finding: Finding,
    #[source]
    source: Option<ManifestSource>,
}

#[derive(Debug, thiserror::Error)]
enum ManifestSource {
    #[error("invalid JSON")]
    Json(#[from] serde_json::Error),
}

impl ManifestError {
    /// Returns at most 32 deterministic findings.
    #[must_use]
    pub fn findings(&self) -> &[Finding] {
        slice::from_ref(&self.finding)
    }

    /// Creates a stable validation finding.
    #[must_use]
    pub fn invalid(code: &str, location: impl Into<String>, message: &str) -> Self {
        Self::invalid_owned(code, location.into(), message)
    }

    fn invalid_owned(code: &str, location: String, message: &str) -> Self {
        Self {
            finding: Finding {
                code: code.into(),
                location,
                message: message.into(),
            },
            source: None,
        }
    }

    pub(super) fn json(source: serde_json::Error) -> Self {
        Self {
            finding: Finding {
                code: "MANIFEST_MALFORMED".into(),
                location: String::new(),
                message: "Manifest must be UTF-8 JSON without duplicate keys and with the declared shape.".into(),
            },
            source: Some(ManifestSource::Json(source)),
        }
    }

    pub(super) fn part_json(source: serde_json::Error) -> Self {
        Self {
            finding: Finding {
                code: "EXTENSION_PARTS_INVALID".into(),
                location: String::new(),
                message: "Build declaration must be UTF-8 JSON without duplicate keys.".into(),
            },
            source: Some(ManifestSource::Json(source)),
        }
    }
}

/// One component's complete declaration. The public spec defines this wire
/// format.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Dotted publisher-qualified extension name.
    #[schemars(
        length(max = 128),
        regex(pattern = "^[a-z][a-z0-9]*(?:-[a-z0-9]+)*(?:\\.[a-z][a-z0-9]*(?:-[a-z0-9]+)*)+$")
    )]
    pub name: String,
    /// Exact semantic version without build metadata.
    pub version: String,
    /// Optional display label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 60))]
    pub label: Option<String>,
    /// Human-readable purpose.
    #[schemars(length(min = 1, max = 200))]
    pub description: String,
    /// Closed non-secret configuration schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<Schema>,
    /// Named OAuth profile declarations.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(length(max = MAX_CONNECTIONS))]
    pub connections: BTreeMap<String, Connection>,
    /// Named item schemas and capabilities.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(length(max = MAX_RESOURCES))]
    pub resources: BTreeMap<String, Resource>,
    /// Named executable actions.
    #[schemars(length(min = 1, max = MAX_ACTIONS))]
    pub actions: BTreeMap<String, Action>,
}

/// A named OAuth profile and scope set.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    /// Publisher-qualified OAuth profile.
    pub profile: String,
    /// Nonempty set of requested scopes.
    #[schemars(length(min = 1, max = MAX_CONNECTION_SCOPES))]
    pub scopes: Vec<String>,
}

/// A resource's host-mediated capability.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    /// Read committed projected items.
    Read,
    /// Write validated projected items.
    Write,
}

/// Named item schema shared by every referring action.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    /// Exact union of capabilities declared by actions.
    pub capabilities: Vec<Capability>,
    /// Required bounded string identity for writable resources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Object item schema.
    pub schema: Schema,
}

/// Execution mode, defaulting to foreground.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// An action without resource streams.
    #[default]
    Foreground,
    /// A durable background operation.
    Background,
}

/// The capabilities required by one action.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Action {
    /// Optional human-readable action purpose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 200))]
    pub description: Option<String>,
    /// Requested execution mode.
    #[serde(default)]
    pub mode: Mode,
    /// Connection names used by this action.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connections: Vec<String>,
    /// Resource names and nonempty capability sets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(length(max = MAX_ACTION_RESOURCES))]
    pub resources: BTreeMap<String, Vec<Capability>>,
    /// Object parameters schema; absent means an empty object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Schema>,
}

impl Manifest {
    /// Parses and validates the exact received document, rejecting duplicate
    /// JSON keys.
    ///
    /// # Errors
    /// Returns stable findings when the document violates the manifest
    /// spec.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        require(
            bytes.len() <= MAX_MANIFEST_BYTES,
            "MANIFEST_TOO_LARGE",
            "",
            "Manifest exceeds 256 KiB.",
        )?;
        let value = parse_json(bytes)?;
        let manifest: Self = serde_json::from_value(value).map_err(ManifestError::json)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Produces the JSON Schema from the public manifest type.
    #[must_use]
    pub fn json_schema() -> serde_json::Value {
        schemars::schema_for!(Self).to_value()
    }

    /// Checks schema shape, limits, and cross-field declarations.
    ///
    /// # Errors
    /// Returns the first stable finding; this is within the 32-finding response
    /// bound.
    pub fn validate(&self) -> Result<(), ManifestError> {
        self.validate_identity()?;
        let mut budget = Budget::default();
        if let Some(schema) = &self.configuration {
            validate_schema(schema, Pointer::root("/configuration"), true, &mut budget)?;
        }
        self.validate_connections()?;
        let used_resources = self.validate_actions(&mut budget)?;
        self.validate_resources(&used_resources, &mut budget)?;
        require(
            fits_encoded_limit(self, MAX_MANIFEST_BYTES).map_err(ManifestError::json)?,
            "MANIFEST_TOO_LARGE",
            "",
            "Manifest exceeds 256 KiB.",
        )
    }

    fn validate_identity(&self) -> Result<(), ManifestError> {
        require(
            dotted_name(&self.name),
            "MANIFEST_INVALID_VALUE",
            "/name",
            "Expected a dotted lowercase extension identity.",
        )?;
        require(
            semver::Version::parse(&self.version).is_ok_and(|v| v.build.is_empty()),
            "MANIFEST_INVALID_VALUE",
            "/version",
            "Expected exact SemVer without build metadata.",
        )?;
        require(
            self.label.as_ref().is_none_or(|s| plain_text(s, 60)),
            "MANIFEST_INVALID_VALUE",
            "/label",
            "Invalid display label.",
        )?;
        require(
            plain_text(&self.description, 200),
            "MANIFEST_INVALID_VALUE",
            "/description",
            "Invalid description.",
        )?;
        require(
            !self.actions.is_empty()
                && self.actions.len() <= MAX_ACTIONS
                && self.resources.len() <= MAX_RESOURCES
                && self.connections.len() <= MAX_CONNECTIONS,
            "MANIFEST_INVALID_VALUE",
            "",
            "Manifest declaration count exceeds its limit.",
        )?;
        Ok(())
    }

    fn validate_connections(&self) -> Result<(), ManifestError> {
        let root = Pointer::root("/connections");
        for (name, connection) in &self.connections {
            let path = root.property(name);
            require(
                map_key(name) && dotted_name(&connection.profile),
                "MANIFEST_INVALID_VALUE",
                path,
                "Invalid connection name or profile.",
            )?;
            let scopes = connection.scopes.iter().collect::<BTreeSet<_>>();
            require(
                !scopes.is_empty()
                    && scopes.len() == connection.scopes.len()
                    && scopes.len() <= MAX_CONNECTION_SCOPES
                    && connection.scopes.iter().all(|s| !s.is_empty() && safe_text(s)),
                "MANIFEST_INVALID_VALUE",
                path,
                "Scopes must be a nonempty set of safe strings.",
            )?;
        }
        Ok(())
    }

    fn validate_actions(&self, budget: &mut Budget) -> Result<BTreeMap<&str, BTreeSet<Capability>>, ManifestError> {
        let mut used_connections = BTreeSet::new();
        let mut used_resources = BTreeMap::<&str, BTreeSet<Capability>>::new();
        let root = Pointer::root("/actions");
        for (name, action) in &self.actions {
            let path = root.property(name);
            require(map_key(name), "MANIFEST_INVALID_VALUE", path, "Invalid action name.")?;
            require(
                action.description.as_ref().is_none_or(|text| plain_text(text, 200)),
                "MANIFEST_INVALID_VALUE",
                path.property("description"),
                "Invalid action description.",
            )?;
            require(
                action.resources.len() <= MAX_ACTION_RESOURCES,
                "MANIFEST_INVALID_VALUE",
                path,
                "An action permits at most eight resources.",
            )?;
            require(
                action.resources.is_empty() || action.mode == Mode::Background,
                "MANIFEST_INVALID_VALUE",
                path,
                "Actions with resources must be background.",
            )?;
            let connections = action.connections.iter().collect::<BTreeSet<_>>();
            require(
                connections.len() == action.connections.len()
                    && connections.iter().all(|s| self.connections.contains_key(*s)),
                "MANIFEST_INVALID_VALUE",
                path,
                "Action connections must be a set of declared names.",
            )?;
            used_connections.extend(connections);
            let reads = action.resources.values().any(|caps| caps.contains(&Capability::Read));
            let writes = action.resources.values().any(|caps| caps.contains(&Capability::Write));
            require(
                !reads || action.parameters.is_none(),
                "MANIFEST_READER_WITH_PARAMETERS",
                path,
                "Reader actions cannot declare parameters.",
            )?;
            require(
                !writes || action.connections.len() <= 1,
                "MANIFEST_RESOURCE_STREAM_AMBIGUOUS",
                path,
                "Writer actions permit at most one connection.",
            )?;
            if let Some(schema) = &action.parameters {
                validate_schema(schema, path.property("parameters"), false, budget)?;
            }
            for (resource, capabilities) in &action.resources {
                require(
                    self.resources.contains_key(resource) && capability_set(capabilities),
                    "MANIFEST_INVALID_VALUE",
                    path.property("resources").property(resource),
                    "Expected a declared resource and nonempty capability set.",
                )?;
                used_resources.entry(resource).or_default().extend(capabilities);
            }
        }
        require(
            used_connections.len() == self.connections.len(),
            "MANIFEST_INVALID_VALUE",
            "/connections",
            "Every connection must be used.",
        )?;
        Ok(used_resources)
    }

    fn validate_resources(
        &self,
        used_resources: &BTreeMap<&str, BTreeSet<Capability>>,
        budget: &mut Budget,
    ) -> Result<(), ManifestError> {
        let root = Pointer::root("/resources");
        for (name, resource) in &self.resources {
            let path = root.property(name);
            require(
                map_key(name) && capability_set(&resource.capabilities),
                "MANIFEST_INVALID_VALUE",
                path,
                "Invalid resource name or capability set.",
            )?;
            require(
                used_resources.get(name.as_str()) == Some(&resource.capabilities.iter().copied().collect()),
                "MANIFEST_INVALID_VALUE",
                path,
                "Resource capabilities must equal the union used by actions.",
            )?;
            validate_schema(&resource.schema, path.property("schema"), false, budget)?;
            if resource.capabilities.contains(&Capability::Write) || resource.key.is_some() {
                let valid = resource.key.as_ref().is_some_and(|key| {
                    let schema = resource.schema.as_value();
                    !key.is_empty()
                        && schema["required"]
                            .as_array()
                            .is_some_and(|required| required.iter().any(|v| v.as_str() == Some(key)))
                        && schema["properties"][key]["type"].as_str() == Some("string")
                        && schema["properties"][key]["maxLength"]
                            .as_u64()
                            .is_some_and(|max| (1..=512).contains(&max))
                });
                require(
                    valid,
                    "MANIFEST_RESOURCE_KEY_INVALID",
                    path,
                    "Writable resource keys must name a required string bounded to 512 characters.",
                )?;
            }
        }
        Ok(())
    }
}

pub(super) fn require(valid: bool, code: &str, path: impl Into<String>, message: &str) -> Result<(), ManifestError> {
    if valid {
        Ok(())
    } else {
        Err(ManifestError::invalid(code, path, message))
    }
}
/// A borrowed JSON Pointer path; only rejected values allocate its spelling.
#[derive(Clone, Copy)]
pub(super) enum Pointer<'a> {
    Root(&'a str),
    Property(&'a Pointer<'a>, &'a str),
    Index(&'a Pointer<'a>, usize),
}

impl<'a> Pointer<'a> {
    pub(super) fn root(path: &'a str) -> Self {
        Self::Root(path)
    }

    pub(super) fn property<'b>(&'b self, name: &'b str) -> Pointer<'b> {
        Pointer::Property(self, name)
    }

    pub(super) fn index(&self, index: usize) -> Pointer<'_> {
        Pointer::Index(self, index)
    }

    fn append(&self, output: &mut String) {
        match self {
            Self::Root(path) => output.push_str(path),
            Self::Property(parent, name) => {
                parent.append(output);
                output.push('/');
                for character in name.chars() {
                    match character {
                        '~' => output.push_str("~0"),
                        '/' => output.push_str("~1"),
                        character => output.push(character),
                    }
                }
            },
            Self::Index(parent, index) => {
                parent.append(output);
                // Formatting an integer into a String cannot fail.
                let _ = write!(output, "/{index}");
            },
        }
    }
}

impl From<Pointer<'_>> for String {
    fn from(path: Pointer<'_>) -> Self {
        let mut output = Self::new();
        path.append(&mut output);
        output
    }
}

pub(super) fn safe_text(text: &str) -> bool {
    !text.chars().any(|c| {
        c.is_control()
            || matches!(c, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    })
}
pub(super) fn plain_text(text: &str, max: usize) -> bool {
    !text.is_empty() && text.chars().count() <= max && safe_text(text)
}
pub(super) fn map_key(text: &str) -> bool {
    text.len() <= 64 && segment(text)
}
fn dotted_name(text: &str) -> bool {
    text.len() <= 128 && text.contains('.') && text.split('.').all(segment)
}
fn segment(text: &str) -> bool {
    text.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && !text.ends_with('-')
        && !text.contains("--")
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn capability_set(values: &[Capability]) -> bool {
    !values.is_empty() && values.iter().collect::<BTreeSet<_>>().len() == values.len()
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use serde_json::json;

    use super::{
        Manifest,
        ManifestError,
        Pointer,
    };

    // One owned finding retains moved location storage and serialized
    // shape.
    #[test]
    fn manifest_error_moves_owned_locations_and_keeps_one_finding() {
        let location = String::from("/actions/example/parameters");
        let allocation = location.as_ptr();
        let error = ManifestError::invalid("INVALID", location, "Invalid declaration.");
        assert_eq!(error.findings()[0].location.as_ptr(), allocation);
        assert_eq!(
            serde_json::to_value(error.findings()).unwrap(),
            json!([{"code":"INVALID","location":"/actions/example/parameters","message":"Invalid declaration."}])
        );
        assert!(error.source().is_none());
        for constructor in [ManifestError::json, ManifestError::part_json] {
            let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
            let error = constructor(source);
            assert_eq!(error.findings().len(), 1);
            assert!(error.source().is_some());
        }
    }

    // Escaping occurs exactly once when materializing a borrowed path.
    #[test]
    fn pointer_escaping_preserves_all_segment_spellings() {
        let root = Pointer::root("/parent");
        for (segment, expected) in [
            ("", "/parent/"),
            ("ordinary", "/parent/ordinary"),
            ("é", "/parent/é"),
            ("~", "/parent/~0"),
            ("/", "/parent/~1"),
            ("~/", "/parent/~0~1"),
            ("~0~1", "/parent/~00~01"),
        ] {
            assert_eq!(String::from(root.property(segment)), expected);
        }
        assert_eq!(String::from(root.property("~/").index(123)), "/parent/~0~1/123");
    }

    // Strict SemVer parsing supplies the canonical-spelling boundary.
    #[test]
    fn manifest_identity_requires_exact_versions_without_build_metadata() {
        let mut manifest = Manifest::parse(include_bytes!("../tests/testdata/extension/valid.json")).unwrap();
        for version in [
            "0.0.0",
            "1.2.3",
            "1.2.3-alpha.1",
            "1.2.3-RC.1",
            "18446744073709551615.0.0",
        ] {
            manifest.version = version.into();
            manifest.validate_identity().unwrap();
        }
        for version in [
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.3-01",
            " 1.2.3",
            "1.2.3 ",
            "v1.2.3",
            "1.2",
            "1.2.3+build",
            "18446744073709551616.0.0",
            "1.2.3junk",
        ] {
            manifest.version = version.into();
            let error = manifest.validate_identity().unwrap_err();
            assert_eq!(error.findings()[0].location, "/version", "{version}");
        }
    }
}
